use esp_idf_hal::peripherals::Peripherals;
use esp_idf_hal::delay::Delay;
use esp_idf_hal::gpio::PinDriver;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    let peripherals = Peripherals::take().unwrap();
    let delay = Delay::default();
    let mut led = PinDriver::output(peripherals.pins.gpio8).unwrap();
    loop {
        log::info!("Hello, world!");
        led.toggle().unwrap();
        delay.delay_ms(100);
    }
}
// use esp_idf_hal::{delay::FreeRtos, gpio::PinDriver, ;

// fn main() {
    
//     // Initialize Pin 8 as an output to drive the LED
//     let mut led_pin = PinDriver::output(peripherals.pins.gpio8).unwrap();

//     // Loop forever blinking the LED on/off every 500ms
//     loop {
//         // Inverse logic to turn LED on
//         led_pin.set_low().unwrap();
//         println!("LED ON");
//         FreeRtos::delay_ms(1000);

//         led_pin.set_high().unwrap();
//         println!("LED OFF");
//         FreeRtos::delay_ms(1000);
//     }
// }