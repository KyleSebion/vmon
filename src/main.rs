use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::PinDriver;
use esp_idf_hal::peripherals::Peripherals;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    let peripherals = Peripherals::take().unwrap();
    
    let mut led = PinDriver::output(peripherals.pins.gpio8).unwrap();
    let _ = std::thread::Builder::new().stack_size(2000).spawn(|| {
        loop {
            log::info!("Nothing");
            FreeRtos::delay_ms(500);
        }
    });
    let _ = std::thread::Builder::new().stack_size(2000).spawn(move || {
        loop {
            log::info!("LED Toggle");
            led.toggle().unwrap();
            FreeRtos::delay_ms(100);
        }
    });
}
