use anyhow::Result;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::PinDriver;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::esp_deep_sleep;
use esp_idf_svc::sys::esp_sleep_get_wakeup_cause;
use esp_idf_svc::sys::esp_timer_get_time;
use esp_idf_svc::sys::esp_vfs_fat_info;
use esp_idf_svc::sys::esp_vfs_fat_mount_config_t;
use esp_idf_svc::sys::esp_vfs_fat_spiflash_mount_rw_wl;
use esp_idf_svc::sys::wl_handle_t;
use esp_idf_svc::wifi::AccessPointConfiguration;
use esp_idf_svc::wifi::AuthMethod;
use esp_idf_svc::wifi::Configuration as WiFiConf;
use esp_idf_svc::wifi::EspWifi;
use esp_idf_svc::wifi::Protocol;
use std::ffi::CStr;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::thread::Builder as Thread;

const STOR_PATH: &CStr = c"/storage";
fn mount_storage() -> Result<()> {
    const STOR_LBL: &CStr = c"storage";
    let mount_config = esp_vfs_fat_mount_config_t {
        format_if_mount_failed: true,
        max_files: 4,
        allocation_unit_size: 4096,
        disk_status_check_enable: false,
        use_one_fat: false,
    };
    let mut wl_handle: wl_handle_t = 0;
    let res = unsafe {
        esp_vfs_fat_spiflash_mount_rw_wl(
            STOR_PATH.as_ptr(),
            STOR_LBL.as_ptr(),
            &mount_config,
            &mut wl_handle,
        )
    };
    if res != 0 {
        anyhow::bail!("esp_vfs_fat_spiflash_mount_rw_wl failed; esp_err_t = {res}");
    }
    Ok(())
}
fn get_storage_free_space() -> Result<u64> {
    let mut total = 0;
    let mut free = 0;
    let res = unsafe { esp_vfs_fat_info(STOR_PATH.as_ptr(), &mut total, &mut free) };
    if res != 0 {
        anyhow::bail!("esp_vfs_fat_info failed; esp_err_t = {res}");
    }
    Ok(free)
}

const DATA_FILE_PATH: &str = "/storage/data.csv";
static DATA_FILE: LazyLock<Mutex<File>> = LazyLock::new(|| {
    Mutex::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(DATA_FILE_PATH)
            .unwrap_or_else(|_| panic!("failed to open file: {DATA_FILE_PATH}")),
    )
});
fn get_lock<'a>() -> Result<MutexGuard<'a, File>> {
    DATA_FILE
        .lock()
        .map_err(|e| anyhow::anyhow!("get_lock error: {:?}", e))
}
fn append_data(d: &str) -> anyhow::Result<()> {
    const STOR_MIN_FREE: u64 = 512 * 1024;
    if get_storage_free_space()? < STOR_MIN_FREE {
        log::warn!("append_data canceled due to lack of minimum free space");
        return Ok(());
    }
    let mut f = get_lock()?;
    if f.metadata()?.len() == 0 {
        writeln!(f, "ts,vin_volts,adc_volts,smoothed,oversampled,rtc_ts")?;
    }
    writeln!(f, "{d}")?;
    f.sync_all()?;
    Ok(())
}
fn clear_data() -> anyhow::Result<()> {
    let f = get_lock()?;
    f.set_len(0)?;
    f.sync_all()?;
    Ok(())
}
fn read_data() -> anyhow::Result<String> {
    let mut f = get_lock()?;
    f.rewind()?;
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    Ok(s)
}

fn reset_then_sleep(usec: u64) -> ! {
    unsafe { esp_deep_sleep(usec) }
}
fn woke_from_sleep() -> bool {
    !matches!(
        unsafe { esp_sleep_get_wakeup_cause() },
        esp_idf_svc::sys::esp_sleep_source_t_ESP_SLEEP_WAKEUP_UNDEFINED
    )
}
fn uptime_usec() -> i64 {
    unsafe { esp_timer_get_time() }
}

fn record_voltage() {}

const DEEP_SLEEP_USEC: u64 = 5 * 1000 * 1000;
fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    log::set_max_level(log::LevelFilter::Debug);
    mount_storage()?;
    let peripherals = Peripherals::take()?;
    let mut led = PinDriver::output(peripherals.pins.gpio8)?;

    if woke_from_sleep() {
        record_voltage();
        reset_then_sleep(DEEP_SLEEP_USEC);
    }

    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    let mut wifi = EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?;
    let conf = WiFiConf::AccessPoint(AccessPointConfiguration {
        ssid: "ESP2".try_into().unwrap(),
        ssid_hidden: false,
        channel: 1,
        secondary_channel: None,
        protocols: Protocol::P802D11BGN.into(),
        auth_method: AuthMethod::WPA2Personal,
        password: "kspass1234".try_into().unwrap(),
        max_connections: 10,
    });
    wifi.set_configuration(&conf)?;
    wifi.start()?;

    let t1 = Thread::new().stack_size(2000).spawn(|| loop {
        log::info!("Nothing");
        FreeRtos::delay_ms(500);
    })?;

    let t2 = Thread::new()
        .stack_size(2000)
        .spawn(move || -> anyhow::Result<()> {
            loop {
                log::info!("LED Toggle");
                led.toggle()?;
                FreeRtos::delay_ms(100); // switch to use std::thread::sleep;use std::time::Duration;sleep(Duration::from_millis(100)); at end
            }
        })?;

    for t in [t1, t2] {
        t.join()
            .map_err(|e| anyhow::anyhow!("thread panicked: {:?}", e))??;
    }
    Ok(())
}
