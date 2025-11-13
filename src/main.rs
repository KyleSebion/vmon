use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::PinDriver;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::AccessPointConfiguration;
use esp_idf_svc::wifi::AuthMethod;
use esp_idf_svc::wifi::Configuration;
use esp_idf_svc::wifi::EspWifi;
use esp_idf_svc::wifi::Protocol;
use std::thread::Builder as Thread;
// cargo remove esp-idf-sys if not needed

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    log::set_max_level(log::LevelFilter::Debug);
    let peripherals = Peripherals::take().unwrap();

    let sys_loop = EspSystemEventLoop::take().unwrap();
    let nvs = EspDefaultNvsPartition::take().unwrap();
    let mut wifi = EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs)).unwrap();
    let conf = Configuration::AccessPoint(AccessPointConfiguration {
        ssid: "ESP2".try_into().unwrap(),
        ssid_hidden: false,
        channel: 1,
        secondary_channel: None,
        protocols: Protocol::P802D11BGN.into(),
        auth_method: AuthMethod::WPA2Personal,
        password: "kspass1234".try_into().unwrap(),
        max_connections: 10,
    });
    wifi.set_configuration(&conf).unwrap();
    wifi.start().unwrap();

    let t1 = Thread::new().stack_size(2000).spawn(|| loop {
        log::info!("Nothing");
        FreeRtos::delay_ms(2000);
    });

    let gpio8 = peripherals.pins.gpio8;
    let t2 = Thread::new().stack_size(2000).spawn(move || {
        let mut led = PinDriver::output(gpio8).unwrap();
        loop {
            log::info!("LED Toggle");
            led.toggle().unwrap();
            FreeRtos::delay_ms(1000); // switch to use std::thread::sleep;use std::time::Duration;sleep(Duration::from_millis(100)); at end
        }
    });

    for t in [t1, t2] {
        t.unwrap().join().unwrap();
    }
}
