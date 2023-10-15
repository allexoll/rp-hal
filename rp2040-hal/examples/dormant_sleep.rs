//! # DORMANT low-power mode example
//!
//! This application demonstrates how to enter and exit the RP2040's lowest-power DORMANT mode
//! where all clocks and PLLs are stopped.
//!
//! Pulling GPIO 14 low (e.g. via a debounced momentary-contact button) alternately wakes the
//! RP2040 from DORMANT mode and a regular WFI sleep. A LED attached to GPIO 25 (the onboard LED
//! on the Raspberry Pi Pico) pulses once before entering DORMANT mode and twice before entering WFI sleep.
//!
//! It may need to be adapted to your particular board layout and/or pin assignment.
//!
//! See the `Cargo.toml` file for Copyright and license details.

#![no_std]
#![no_main]

#[allow(unused_imports)]
use panic_halt as _;

use rp2040_hal as hal;

use core::{cell::RefCell, ops::DerefMut};

use critical_section::Mutex;

use embedded_hal::digital::v2::ToggleableOutputPin;

use fugit::RateExtU32;

use hal::{
    clocks::{ClockError, ClocksManager, InitError, StoppableClock},
    gpio,
    gpio::{Interrupt::EdgeLow, Pins},
    pac,
    pac::{interrupt, CLOCKS, PLL_SYS, PLL_USB, RESETS, ROSC, XOSC},
    pll::{Disabled, Locked, PhaseLockedLoop, common_configs::{PLL_SYS_125MHZ, PLL_USB_48MHZ}, setup_pll_blocking, start_pll_blocking},
    sio::Sio,
    watchdog::Watchdog,
    xosc::{CrystalOscillator, Dormant, setup_xosc_blocking, Stable},
    rosc::RingOscillator,
    Clock,
};

use nb::block;

/// The button input.
type ButtonPin = gpio::Pin<gpio::bank0::Gpio14, gpio::FunctionSioInput, gpio::PullNone>;

/// Devices shared between the foreground code and interrupt handlers.
static GLOBAL_DEVICES: Mutex<RefCell<Option<ButtonPin>>> = Mutex::new(RefCell::new(None));

/// The linker will place this boot block at the start of our program image. We
/// need this to help the ROM bootloader get our code up and running.
/// Note: This boot block is not necessary when using a rp-hal based BSP
/// as the BSPs already perform this step.
#[link_section = ".boot2"]
#[used]
pub static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_GENERIC_03H;

/// External high-speed crystal on the Raspberry Pi Pico board is 12 MHz. Adjust
/// if your board has a different frequency.
const XTAL_FREQ_HZ: u32 = 12_000_000u32;

#[rp2040_hal::entry]
fn main() -> ! {
    let mut pac = pac::Peripherals::take().unwrap();
    let mut watchdog = Watchdog::new(pac.WATCHDOG);
    let sio = Sio::new(pac.SIO);

    // Configure the clocks
    let (mut clocks, mut xosc, mut pll_sys, mut pll_usb) = init_clocks_and_plls(
        XTAL_FREQ_HZ,
        pac.XOSC,
        pac.ROSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
        .ok()
        .unwrap();

    // Set the pins to their default state
    let pins = Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    // Configure GPIO 25 as an output
    let mut led_pin = pins.gpio25.into_push_pull_output();

    // Configure GPIO 14 as an input that wakes the RP2040 from a sleep state
    let button_pin = pins.gpio14.reconfigure();
    button_pin.set_dormant_wake_enabled(EdgeLow, true);
    button_pin.set_interrupt_enabled(EdgeLow, true);

    critical_section::with(|cs| {
        GLOBAL_DEVICES.borrow(cs).replace(Some(button_pin));
    });

    unsafe {
        pac::NVIC::unmask(pac::Interrupt::IO_IRQ_BANK0);
    }

    let mut use_dormant = true;
    loop {
        if use_dormant {
            pulse(&mut led_pin, 1);

            let (disabled_pll_sys, disabled_pll_usb) = prepare_clocks_and_plls_for_dormancy(&mut xosc, &mut clocks, pll_sys, pll_usb);

            // Stop the crystal oscillator and enter the RP2040's dormant state
            let dormant_xosc = unsafe { xosc.dormant() };

            match restart_clocks_and_plls(&mut clocks, dormant_xosc, disabled_pll_sys, disabled_pll_usb, &mut pac.RESETS) {
                Ok((stable_xosc, stable_pll_sys, stable_pll_usb)) => {
                    xosc = stable_xosc;
                    pll_sys = stable_pll_sys;
                    pll_usb = stable_pll_usb;
                },
                Err(_) => {
                    panic!();
                }
            }

            // Clear dormant wake interrupt status to enable wake next time
            critical_section::with(|cs| {
                let mut global_devices = GLOBAL_DEVICES.borrow(cs).borrow_mut();
                let Some(ref mut trigger_pin) = global_devices.deref_mut() else {
                    panic!()
                };

                trigger_pin.clear_interrupt(EdgeLow);
            });
        } else {
            pulse(&mut led_pin, 2);

            // Enter the regular RP2040 sleep state: clocks and PLLs stay running
            cortex_m::asm::wfi();
        }

        use_dormant = !use_dormant;
    }
}

/// Pulse an LED-connected pin the specified number of times.
fn pulse<P: ToggleableOutputPin>(pin: &mut P, count: u32) {
    const LED_PULSE_CYCLES: u32 = 2_000_000;

    for i in 0..count*2 {
        let _ = pin.toggle();
        cortex_m::asm::delay(LED_PULSE_CYCLES + (i % 2) * 9 * LED_PULSE_CYCLES); // 1:10 duty cycle
    }
}

/// Initialize clocks and PLLs in much the same way as rp2040-hal::clocks::init_clocks_and_plls().
/// Returns the crystal oscillator and the PLLs so we can reconfigure them later.
fn init_clocks_and_plls(
    xosc_crystal_freq: u32,
    xosc_dev: XOSC,
    rosc_dev: ROSC,
    clocks_dev: CLOCKS,
    pll_sys_dev: PLL_SYS,
    pll_usb_dev: PLL_USB,
    resets: &mut RESETS,
    watchdog: &mut Watchdog,
) -> Result<(ClocksManager, CrystalOscillator<Stable>, PhaseLockedLoop<Locked, PLL_SYS>, PhaseLockedLoop<Locked, PLL_USB>), InitError> {
    let xosc = setup_xosc_blocking(xosc_dev, xosc_crystal_freq.Hz()).unwrap();

    // Configure watchdog tick generation to tick over every microsecond
    watchdog.enable_tick_generation((xosc_crystal_freq / 1_000_000) as u8);

    let mut clocks = ClocksManager::new(clocks_dev);

    let pll_sys = setup_pll_blocking(
        pll_sys_dev,
        xosc.operating_frequency(),
        PLL_SYS_125MHZ,
        &mut clocks,
        resets,
    )
        .map_err(InitError::PllError)?;
    let pll_usb = setup_pll_blocking(
        pll_usb_dev,
        xosc.operating_frequency(),
        PLL_USB_48MHZ,
        &mut clocks,
        resets,
    )
        .map_err(InitError::PllError)?;

    clocks
        .init_default(&xosc, &pll_sys, &pll_usb)
        .map_err(InitError::ClockError)?;

    let rosc = RingOscillator::new(rosc_dev).initialize();
    rosc.disable(); // disable ring oscillator to maximise power savings

    Ok((clocks, xosc, pll_sys, pll_usb))
}

/// Switch clocks to the crystal oscillator or disable them as appropriate, and stop PLLs so
/// that we're ready to go dormant.
fn prepare_clocks_and_plls_for_dormancy(
    xosc: &mut CrystalOscillator<Stable>,
    clocks: &mut ClocksManager,
    pll_sys: PhaseLockedLoop<Locked, PLL_SYS>,
    pll_usb: PhaseLockedLoop<Locked, PLL_USB>,
) -> (PhaseLockedLoop<Disabled, PLL_SYS>, PhaseLockedLoop<Disabled, PLL_USB>) {
    // switch system clock from pll_sys to xosc so that we can stop the system PLL
    nb::block!(clocks.system_clock.reset_source_await()).unwrap();

    clocks.usb_clock.disable();
    clocks.adc_clock.disable();

    clocks.rtc_clock.configure_clock(xosc, 46875u32.Hz()).unwrap();
    clocks.peripheral_clock.configure_clock(&clocks.system_clock, clocks.system_clock.freq()).unwrap();

    (pll_sys.disable(), pll_usb.disable())
}

/// Restart the PLLs and start/reconfigure the clocks back to how they were before going dormant.
fn restart_clocks_and_plls(
    clocks: &mut ClocksManager,
    dormant_xosc: CrystalOscillator<Dormant>,
    disabled_pll_sys: PhaseLockedLoop<Disabled, PLL_SYS>,
    disabled_pll_usb: PhaseLockedLoop<Disabled, PLL_USB>,
    resets: &mut RESETS,
) -> Result<(CrystalOscillator<Stable>, PhaseLockedLoop<Locked, PLL_SYS>, PhaseLockedLoop<Locked, PLL_USB>), ClockError> {
    // Wait for the restarted XOSC to stabilise
    let initialized_xosc = dormant_xosc.get_initialized();
    let stable_xosc_token = block!(initialized_xosc.await_stabilization()).unwrap();
    let xosc = initialized_xosc.get_stable(stable_xosc_token);

    let pll_sys = start_pll_blocking(disabled_pll_sys, resets).unwrap();
    let pll_usb = start_pll_blocking(disabled_pll_usb, resets).unwrap();

    clocks.init_default(&xosc, &pll_sys, &pll_usb).map(|_| (xosc, pll_sys, pll_usb))
}

#[interrupt]
fn IO_IRQ_BANK0() {
    critical_section::with(|cs| {
        let mut global_devices = GLOBAL_DEVICES.borrow(cs).borrow_mut();
        let Some(ref mut button_pin) = global_devices.deref_mut() else {
            panic!()
        };

        // Check if the interrupt source is from the push button going from high-to-low.
        // Note: this will always be true in this example, as that is the only enabled GPIO interrupt source
        if button_pin.interrupt_status(EdgeLow) {
            // Our interrupt doesn't clear itself.
            // Do that now so we don't immediately jump back to this interrupt handler.
            button_pin.clear_interrupt(EdgeLow);
        }
    });
}

// End of file
