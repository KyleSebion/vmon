use esp_idf_hal::gpio::PinDriver;
use esp_idf_hal::peripherals::Peripherals;
use std::thread::sleep;
use std::thread::Builder as Thread;
use std::time::Duration;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    let peripherals = Peripherals::take().unwrap();
    let gpio8 = peripherals.pins.gpio8;

    let t1 = Thread::new().stack_size(2000).spawn(|| loop {
        log::info!("Nothing");
        sleep(Duration::from_millis(500));
    });
    let t2 = Thread::new().stack_size(2000).spawn(move || {
        let mut led = PinDriver::output(gpio8).unwrap();
        loop {
            log::info!("LED Toggle");
            led.toggle().unwrap();
            sleep(Duration::from_millis(100));
        }
    });
    for t in [t1, t2] {
        t.unwrap().join().unwrap();
    }
}
