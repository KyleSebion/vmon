#![expect(dead_code)]
use anyhow::Ok;
use anyhow::Result;
use embedded_svc::http::Method as HttpMethod;
use esp_idf_hal::delay::TickType;
use esp_idf_hal::delay::TickType_t;
use esp_idf_hal::units::FromValueType;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::i2c::I2cConfig;
use esp_idf_svc::hal::i2c::I2cDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::http::server::Configuration as HttpConf;
use esp_idf_svc::http::server::EspHttpServer;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::esp_deep_sleep;
use esp_idf_svc::sys::esp_sleep_get_wakeup_cause;
use esp_idf_svc::sys::esp_timer_get_time;
use esp_idf_svc::sys::esp_vfs_fat_info;
use esp_idf_svc::sys::esp_vfs_fat_mount_config_t;
use esp_idf_svc::sys::esp_vfs_fat_spiflash_mount_rw_wl;
use esp_idf_svc::sys::rwdt_shim::feed_rtc_wdt;
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

struct LockedFile {
    locker: LazyLock<Mutex<File>>,
}
impl LockedFile {
    const DATA_FILE_PATH: &str = "/storage/data.csv";
    pub const fn new_data() -> LockedFile {
        LockedFile {
            locker: LazyLock::new(|| {
                Mutex::new(
                    OpenOptions::new()
                        .read(true)
                        .append(true)
                        .create(true)
                        .open(Self::DATA_FILE_PATH)
                        .unwrap_or_else(|_| {
                            panic!("failed to open file: {}", Self::DATA_FILE_PATH)
                        }),
                )
            }),
        }
    }
    const SETTINGS_FILE_PATH: &str = "/storage/setting.json";
    pub const fn new_settings() -> LockedFile {
        LockedFile {
            locker: LazyLock::new(|| {
                Mutex::new(
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .truncate(false)
                        .create(true)
                        .open(Self::SETTINGS_FILE_PATH)
                        .unwrap_or_else(|_| {
                            panic!("failed to open file: {}", Self::SETTINGS_FILE_PATH)
                        }),
                )
            }),
        }
    }
    fn lock(&self) -> Result<MutexGuard<'_, File>> {
        self.locker
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))
    }
    fn append_data(&self, d: &str) -> Result<()> {
        const STOR_MIN_FREE: u64 = 512 * 1024;
        if get_storage_free_space()? < STOR_MIN_FREE {
            log::warn!("append_data canceled due to lack of minimum free space");
            return Ok(());
        }
        let mut f = self.lock()?;
        if f.metadata()?.len() == 0 {
            writeln!(f, "uptime_ms,rtc_ts,w,v,a")?;
        }
        writeln!(f, "{d}")?;
        f.sync_all()?;
        Ok(())
    }
    fn clear_data(&self) -> Result<()> {
        let f = self.lock()?;
        f.set_len(0)?;
        f.sync_all()?;
        Ok(())
    }
    fn read_data(&self) -> Result<String> {
        let mut f = self.lock()?;
        f.rewind()?;
        let mut s = String::new();
        f.read_to_string(&mut s)?;
        Ok(s)
    }
}
static DATA_FILE: LockedFile = LockedFile::new_data();
static SETTINGS_FILE: LockedFile = LockedFile::new_settings();

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
fn feed_watchdog() {
    unsafe {
        feed_rtc_wdt();
    }
}

fn get_smoothed<const I: usize>(val: f64) -> f64 {
    const SMOOTH_ARRAY_COUNT: usize = 3;
    const SMOOTH_COUNT: usize = 8;
    #[link_section = ".rtc.data"]
    static mut SMOOTH_BUF_IS: [usize; SMOOTH_ARRAY_COUNT] = [usize::MAX; SMOOTH_ARRAY_COUNT];
    #[link_section = ".rtc.data"]
    static mut SMOOTH_BUFS: [[f64; SMOOTH_COUNT]; SMOOTH_ARRAY_COUNT] =
        [[0_f64; SMOOTH_COUNT]; SMOOTH_ARRAY_COUNT];
    unsafe {
        let smooth_buf = &mut SMOOTH_BUFS[I];
        let smooth_buf_i = &mut SMOOTH_BUF_IS[I];
        if *smooth_buf_i == usize::MAX {
            smooth_buf.fill(val);
            *smooth_buf_i = 0;
        } else {
            smooth_buf[*smooth_buf_i] = val;
            *smooth_buf_i = (*smooth_buf_i + 1) % SMOOTH_COUNT;
        }
        let s = smooth_buf.iter().sum::<f64>();
        s / SMOOTH_COUNT as f64
    }
}

struct RtcDateTime {
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
}
impl RtcDateTime {
    fn new(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Self {
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }
}
struct DS3231 {
    addr: u8,
    timeout: TickType_t,
}
impl DS3231 {
    fn dec_to_bcd(d: u8) -> u8 {
        ((d / 10) << 4) | (d % 10)
    }
    fn bcd_to_dec(b: u8) -> u8 {
        (b >> 4) * 10 + (b & 0x0F)
    }
    fn new(addr: u8) -> Self {
        Self {
            addr,
            timeout: TickType::new_millis(100).0,
        }
    }
    fn set_rtc(&mut self, i2c: &mut I2cDriver, dt: &RtcDateTime) -> Result<()> {
        let year = (dt.year - 2000) as u8;
        let data = [
            0x00,
            Self::dec_to_bcd(dt.second),
            Self::dec_to_bcd(dt.minute),
            Self::dec_to_bcd(dt.hour),
            Self::dec_to_bcd(0), // Day of week (not used)
            Self::dec_to_bcd(dt.day),
            Self::dec_to_bcd(dt.month),
            Self::dec_to_bcd(year),
        ];
        i2c.write(self.addr, &data, self.timeout)?;
        Ok(())
    }
    fn read_rtc(&mut self, i2c: &mut I2cDriver) -> Result<RtcDateTime> {
        i2c.write(self.addr, &[0x00], self.timeout)?;
        let mut buf = [0u8; 7];
        i2c.read(self.addr, &mut buf, self.timeout)?;
        let second = Self::bcd_to_dec(buf[0] & 0x7F);
        let minute = Self::bcd_to_dec(buf[1]);
        let hour = Self::bcd_to_dec(buf[2] & 0x3F);
        let day = Self::bcd_to_dec(buf[4]);
        let month = Self::bcd_to_dec(buf[5] & 0x1F);
        let year = 2000 + Self::bcd_to_dec(buf[6]) as u16;
        Ok(RtcDateTime::new(year, month, day, hour, minute, second))
    }
    fn read_rtc_str(&mut self, i2c: &mut I2cDriver) -> Result<String> {
        let RtcDateTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
        } = self.read_rtc(i2c)?;
        Ok(format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
        ))
    }
}
struct INA219 {
    addr: u8,
    timeout: TickType_t,
    r_shunt: f64,              // Ω
    max_expected_current: f64, // A
    current_lsb: f64,
    power_lsb: f64,
    calibration: u16,
    conf: u16,
}
impl INA219 {
    const REG_CONF: u8 = 0x00;
    const REG_SHUNT_V: u8 = 0x01;
    const REG_BUS_V: u8 = 0x02;
    const REG_POWER_W: u8 = 0x03;
    const REG_CURRENT_A: u8 = 0x04;
    const REG_CALIBRATE: u8 = 0x05;
    const INTERNAL_FIXED_VALUE: f64 = 0.04096;
    const SHUNT_VOLTAGE_LSB: f64 = 0.000010; // 10 μV
    const BUS_VOLTAGE_LSB: f64 = 0.004; // 4 mV
    fn new(addr: u8, r_shunt: f64, max_expected_current: f64, conf: u16) -> Self {
        let current_lsb = max_expected_current / 2_f64.powi(15);
        Self {
            addr,
            timeout: TickType::new_millis(100).0,
            r_shunt,
            max_expected_current,
            current_lsb,
            power_lsb: 20_f64 * current_lsb,
            calibration: (Self::INTERNAL_FIXED_VALUE / (current_lsb * r_shunt)) as u16,
            conf,
        }
    }
    fn read_u16(&mut self, i2c: &mut I2cDriver, reg: u8) -> Result<u16> {
        i2c.write(self.addr, &[reg], self.timeout)?;
        let mut buf = [0u8; 2];
        i2c.read(self.addr, &mut buf, self.timeout)?;
        Ok(u16::from_be_bytes(buf))
    }
    fn read_i16(&mut self, i2c: &mut I2cDriver, reg: u8) -> Result<i16> {
        self.read_u16(i2c, reg).map(|v| v as i16)
    }
    fn write_u16(&mut self, i2c: &mut I2cDriver, reg: u8, v: u16) -> Result<()> {
        let mut buf = [reg, 0, 0];
        buf[1..].copy_from_slice(&v.to_be_bytes());
        i2c.write(self.addr, &buf, self.timeout)?;
        Ok(())
    }
    fn write_conf(&mut self, i2c: &mut I2cDriver) -> Result<()> {
        self.write_u16(i2c, Self::REG_CONF, self.conf)
    }
    fn write_calibration(&mut self, i2c: &mut I2cDriver) -> Result<()> {
        self.write_u16(i2c, Self::REG_CALIBRATE, self.calibration)
    }
    fn read_shunt_v(&mut self, i2c: &mut I2cDriver) -> Result<f64> {
        self.read_i16(i2c, Self::REG_SHUNT_V)
            .map(|v| v as f64 * Self::SHUNT_VOLTAGE_LSB)
    }
    fn read_bus_v(&mut self, i2c: &mut I2cDriver) -> Result<f64> {
        self.read_u16(i2c, Self::REG_BUS_V)
            .map(|v| (v >> 3) as f64 * Self::BUS_VOLTAGE_LSB)
    }
    fn read_w(&mut self, i2c: &mut I2cDriver) -> Result<f64> {
        self.read_u16(i2c, Self::REG_POWER_W)
            .map(|v| v as f64 * self.power_lsb)
    }
    fn read_a(&mut self, i2c: &mut I2cDriver) -> Result<f64> {
        self.read_i16(i2c, Self::REG_CURRENT_A)
            .map(|v| v as f64 * self.current_lsb)
    }
    fn read_v(&mut self, i2c: &mut I2cDriver) -> Result<f64> {
        let sv = self.read_shunt_v(i2c)?;
        let bv = self.read_bus_v(i2c)?;
        Ok(if sv.is_sign_negative() { bv } else { sv + bv })
    }
}
struct I2cDevices<'a> {
    i2c: I2cDriver<'a>,
    ds3231: DS3231,
    ina219: INA219,
}
impl<'a> I2cDevices<'a> {
    fn new(i2c: I2cDriver<'a>, ds3231: DS3231, ina219: INA219) -> Result<Self> {
        let mut s = Self {
            i2c,
            ds3231,
            ina219,
        };
        let i2c = &mut s.i2c;
        s.ina219.write_conf(i2c)?;
        s.ina219.write_calibration(i2c)?;
        Ok(s)
    }
    fn set_ds3231_rtc(&mut self, dt: &RtcDateTime) -> Result<()> {
        self.ds3231.set_rtc(&mut self.i2c, dt)
    }
    fn read_ds3231_rtc_str(&mut self) -> Result<String> {
        self.ds3231.read_rtc_str(&mut self.i2c)
    }
    fn read_ina219_w(&mut self) -> Result<f64> {
        self.ina219.read_w(&mut self.i2c)
    }
    fn read_ina219_v(&mut self) -> Result<f64> {
        self.ina219.read_v(&mut self.i2c)
    }
    fn read_ina219_a(&mut self) -> Result<f64> {
        self.ina219.read_a(&mut self.i2c)
    }
}

fn record_measurements(i2c: &mut I2cDevices, v_cb: fn(f64)) -> Result<()> {
    let uptime_ms = uptime_usec() / 1000;
    let rtc_ts = i2c.read_ds3231_rtc_str()?;
    let w = get_smoothed::<0>(i2c.read_ina219_w()?);
    let v = get_smoothed::<1>(i2c.read_ina219_v()?);
    let a = get_smoothed::<2>(i2c.read_ina219_a()?);
    let line = format!("{uptime_ms},{rtc_ts},{w:.2},{v:.2},{a:.3}");
    log::info!("{line}");
    // DATA_FILE.append_data(&line)?;
    v_cb(v);
    Ok(())
}

fn main() -> Result<()> {
    const LOW_V: f64 = 12.2;
    const LOW_V_SLEEP_USEC: u32 = 60 * 1000 * 1000;
    const RECORD_SLEEP_USEC: u32 = 2 * 1000 * 1000;
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    log::set_max_level(log::LevelFilter::Debug);
    feed_watchdog();
    mount_storage()?;
    let peripherals = Peripherals::take()?;
    let mut i2c = I2cDevices::new(
        I2cDriver::new(
            peripherals.i2c0,
            peripherals.pins.gpio4,
            peripherals.pins.gpio3,
            &I2cConfig::new().baudrate(400.kHz().into()), //TODO 100?
        )?,
        DS3231::new(0x68),
        INA219::new(0x41, 0.1, 3.2, 0x3FFF), // 0x3FFF based on https://www.ti.com/lit/ds/symlink/ina219.pdf
    )?;
    let low_v_cb = |v| {
        if v <= LOW_V {
            reset_then_sleep(LOW_V_SLEEP_USEC.into());
        }
    };

    if woke_from_sleep() {
        record_measurements(&mut i2c, low_v_cb)?;
        reset_then_sleep(RECORD_SLEEP_USEC.into());
    }

    let mut led = PinDriver::output(peripherals.pins.gpio8)?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    let mut wifi = EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?;
    let conf = WiFiConf::AccessPoint(AccessPointConfiguration {
        ssid: "ESP2"
            .try_into()
            .map_err(|_| anyhow::anyhow!("ssid error"))?,
        ssid_hidden: false,
        channel: 1,
        secondary_channel: None,
        protocols: Protocol::P802D11BGN.into(),
        auth_method: AuthMethod::WPA2Personal,
        password: "kspass1234"
            .try_into()
            .map_err(|_| anyhow::anyhow!("password error"))?,
        max_connections: 10,
    });
    wifi.set_configuration(&conf)?;
    wifi.start()?;
    let mut http_server = EspHttpServer::new(&HttpConf::default())?;
    http_server.fn_handler("/", HttpMethod::Get, |rq| {
        let mut rs = rq.into_ok_response()?;
        rs.write(format!("ts {}", uptime_usec()).as_bytes())?;
        Ok(())
    })?;
    loop {
        feed_watchdog();
        if let Err(e) = led.set_low() {
            log::warn!("led on error: {e}");
        }
        if let Err(e) = record_measurements(&mut i2c, low_v_cb) {
            log::error!("record_measurements error: {e}");
        }
        if let Err(e) = led.set_high() {
            log::warn!("led off error: {e}");
        }
        FreeRtos::delay_ms(RECORD_SLEEP_USEC / 1000); // switch to use std::thread::sleep;use std::time::Duration;sleep(Duration::from_micros(SLEEP_USEC)); at end
    }
}
