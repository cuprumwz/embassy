#![no_std]
#![no_main]
#![feature(generic_associated_types)]
#![feature(type_alias_impl_trait)]

use core::mem;
use core::sync::atomic::{AtomicBool, Ordering};
use defmt::*;
use embassy::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy::channel::Channel;
use embassy::executor::Spawner;
use embassy::interrupt::InterruptExt;
use embassy::time::Duration;
use embassy_nrf::gpio::{Input, Pin, Pull};
use embassy_nrf::interrupt;
use embassy_nrf::pac;
use embassy_nrf::usb::Driver;
use embassy_nrf::Peripherals;
use embassy_usb::control::OutResponse;
use embassy_usb::{Config, DeviceCommand, DeviceStateHandler, UsbDeviceBuilder};
use embassy_usb_hid::{HidClass, ReportId, RequestHandler, State};
use futures::future::join;
use usbd_hid::descriptor::{KeyboardReport, SerializedDescriptor};

use defmt_rtt as _; // global logger
use panic_probe as _;

static USB_COMMANDS: Channel<CriticalSectionRawMutex, DeviceCommand, 1> = Channel::new();
static SUSPENDED: AtomicBool = AtomicBool::new(false);

fn on_power_interrupt(_: *mut ()) {
    let regs = unsafe { &*pac::POWER::ptr() };

    if regs.events_usbdetected.read().bits() != 0 {
        regs.events_usbdetected.reset();
        info!("Vbus detected, enabling USB...");
        if USB_COMMANDS.try_send(DeviceCommand::Enable).is_err() {
            warn!("Failed to send enable command to USB channel");
        }
    }

    if regs.events_usbremoved.read().bits() != 0 {
        regs.events_usbremoved.reset();
        info!("Vbus removed, disabling USB...");
        if USB_COMMANDS.try_send(DeviceCommand::Disable).is_err() {
            warn!("Failed to send disable command to USB channel");
        };
    }
}

#[embassy::main]
async fn main(_spawner: Spawner, p: Peripherals) {
    let clock: pac::CLOCK = unsafe { mem::transmute(()) };
    let power: pac::POWER = unsafe { mem::transmute(()) };

    info!("Enabling ext hfosc...");
    clock.tasks_hfclkstart.write(|w| unsafe { w.bits(1) });
    while clock.events_hfclkstarted.read().bits() != 1 {}

    // Create the driver, from the HAL.
    let irq = interrupt::take!(USBD);
    let driver = Driver::new(p.USBD, irq);

    // Create embassy-usb Config
    let mut config = Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("Tactile Engineering");
    config.product = Some("Testy");
    config.serial_number = Some("12345678");
    config.max_power = 100;
    config.max_packet_size_0 = 64;
    config.supports_remote_wakeup = true;
    config.start_enabled = false;

    // Create embassy-usb DeviceBuilder using the driver and config.
    // It needs some buffers for building the descriptors.
    let mut device_descriptor = [0; 256];
    let mut config_descriptor = [0; 256];
    let mut bos_descriptor = [0; 256];
    let mut control_buf = [0; 16];
    let request_handler = MyRequestHandler {};
    let device_state_handler = MyDeviceStateHandler::new();

    let mut state = State::<8, 1>::new();

    let mut builder = UsbDeviceBuilder::new_with_channel(
        driver,
        config,
        &mut device_descriptor,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut control_buf,
        Some(&device_state_handler),
        &USB_COMMANDS,
    );

    // Create classes on the builder.
    let hid = HidClass::with_output_ep(
        &mut builder,
        &mut state,
        KeyboardReport::desc(),
        Some(&request_handler),
        60,
        64,
    );

    // Build the builder.
    let mut usb = builder.build();

    // Run the USB device.
    let usb_fut = usb.run();

    let mut button = Input::new(p.P0_11.degrade(), Pull::Up);

    let (mut hid_in, hid_out) = hid.split();

    // Do stuff with the class!
    let in_fut = async {
        loop {
            button.wait_for_low().await;
            info!("PRESSED");

            if SUSPENDED.load(Ordering::Acquire) {
                info!("Triggering remote wakeup");
                USB_COMMANDS.send(DeviceCommand::RemoteWakeup);
            }

            let report = KeyboardReport {
                keycodes: [4, 0, 0, 0, 0, 0],
                leds: 0,
                modifier: 0,
                reserved: 0,
            };
            match hid_in.serialize(&report).await {
                Ok(()) => {}
                Err(e) => warn!("Failed to send report: {:?}", e),
            };

            button.wait_for_high().await;
            info!("RELEASED");
            let report = KeyboardReport {
                keycodes: [0, 0, 0, 0, 0, 0],
                leds: 0,
                modifier: 0,
                reserved: 0,
            };
            match hid_in.serialize(&report).await {
                Ok(()) => {}
                Err(e) => warn!("Failed to send report: {:?}", e),
            };
        }
    };

    let out_fut = async {
        hid_out.run(false, &request_handler).await;
    };

    let power_irq = interrupt::take!(POWER_CLOCK);
    power_irq.set_handler(on_power_interrupt);
    power_irq.unpend();
    power_irq.enable();

    power
        .intenset
        .write(|w| w.usbdetected().set().usbremoved().set());

    // Run everything concurrently.
    // If we had made everything `'static` above instead, we could do this using separate tasks instead.
    join(usb_fut, join(in_fut, out_fut)).await;
}

struct MyRequestHandler {}

impl RequestHandler for MyRequestHandler {
    fn get_report(&self, id: ReportId, _buf: &mut [u8]) -> Option<usize> {
        info!("Get report for {:?}", id);
        None
    }

    fn set_report(&self, id: ReportId, data: &[u8]) -> OutResponse {
        info!("Set report for {:?}: {=[u8]}", id, data);
        OutResponse::Accepted
    }

    fn set_idle(&self, id: Option<ReportId>, dur: Duration) {
        info!("Set idle rate for {:?} to {:?}", id, dur);
    }

    fn get_idle(&self, id: Option<ReportId>) -> Option<Duration> {
        info!("Get idle rate for {:?}", id);
        None
    }
}

struct MyDeviceStateHandler {
    configured: AtomicBool,
}

impl MyDeviceStateHandler {
    fn new() -> Self {
        MyDeviceStateHandler {
            configured: AtomicBool::new(false),
        }
    }
}

impl DeviceStateHandler for MyDeviceStateHandler {
    fn reset(&self) {
        self.configured.store(false, Ordering::Relaxed);
        info!("Bus reset, the Vbus current limit is 100mA");
    }

    fn addressed(&self, addr: u8) {
        self.configured.store(false, Ordering::Relaxed);
        info!("USB address set to: {}", addr);
    }

    fn configured(&self, configured: bool) {
        self.configured.store(configured, Ordering::Relaxed);
        if configured {
            info!(
                "Device configured, it may now draw up to the configured current limit from Vbus."
            )
        } else {
            info!("Device is no longer configured, the Vbus current limit is 100mA.");
        }
    }

    fn suspended(&self, suspended: bool) {
        if suspended {
            info!("Device suspended, the Vbus current limit is 500µA (or 2.5mA for high-power devices with remote wakeup enabled).");
            SUSPENDED.store(true, Ordering::Release);
        } else {
            SUSPENDED.store(false, Ordering::Release);
            if self.configured.load(Ordering::Relaxed) {
                info!(
                    "Device resumed, it may now draw up to the configured current limit from Vbus"
                );
            } else {
                info!("Device resumed, the Vbus current limit is 100mA");
            }
        }
    }

    fn disabled(&self) {
        self.configured.store(false, Ordering::Relaxed);
        info!("Device disabled");
    }
}
