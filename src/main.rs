use anyhow::Ok as AOk;
use anyhow::Result;
use embedded_svc::http::Headers;
use embedded_svc::http::Method as HttpMethod;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::fs::littlefs::Littlefs;
use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::delay::TickType_t;
use esp_idf_svc::hal::gpio::OutputPin;
use esp_idf_svc::hal::i2c::I2cConfig;
use esp_idf_svc::hal::i2c::I2cDriver;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::hal::peripheral::Peripheral;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::rmt::RmtChannel;
use esp_idf_svc::hal::units::FromValueType;
use esp_idf_svc::http::server::Configuration as HttpConf;
use esp_idf_svc::http::server::EspHttpServer;
use esp_idf_svc::io::vfs::MountedLittlefs;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::esp_deep_sleep;
use esp_idf_svc::sys::esp_littlefs_info;
use esp_idf_svc::sys::esp_restart;
use esp_idf_svc::sys::esp_sleep_get_wakeup_cause;
use esp_idf_svc::sys::esp_timer_get_time;
use esp_idf_svc::sys::rwdt_shim::feed_rtc_wdt;
use esp_idf_svc::wifi::AccessPointConfiguration;
use esp_idf_svc::wifi::AuthMethod;
use esp_idf_svc::wifi::Configuration as WiFiConf;
use esp_idf_svc::wifi::EspWifi;
use esp_idf_svc::wifi::Protocol;
use serde::Deserialize;
use serde::Serialize;
use std::cell::OnceCell;
use std::ffi::CStr;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::sync::mpsc::channel;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::thread::sleep;
use std::time::Duration;
use std::time::Instant;
use ws2812_esp32_rmt_driver::Ws2812Esp32RmtDriver;

#[derive(Serialize, Deserialize)]
struct Settings {
    wifi_pass: String,
    wifi_ssid: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            wifi_pass: "kspass1234".to_string(),
            wifi_ssid: "ESP2".to_string(),
        }
    }
}

const STOR_LBL_CSTR: &CStr = c"storage";
const STOR_LBL_STR: &str = "storage";
const STOR_PATH: &str = "/storage";
const DATA_FILE_PATH: &str = "/storage/data.csv";
const SETTINGS_FILE_PATH: &str = "/storage/settings.json";
fn try_mount_storage(fmt: bool) -> Result<MountedLittlefs<Littlefs<()>>> {
    let mut littlefs: Littlefs<()> = unsafe { Littlefs::new_partition(STOR_LBL_STR) }?;
    if fmt {
        littlefs.format()?;
    }
    let mounted = MountedLittlefs::mount(littlefs, STOR_PATH)?;
    AOk(mounted)
}
fn mount_storage() -> Result<MountedLittlefs<Littlefs<()>>> {
    match try_mount_storage(false) {
        Ok(mounted) => AOk(mounted),
        Err(e) => {
            log::info!("mount failed: {e}; formatting");
            try_mount_storage(true)
        }
    }
}
#[derive(Serialize, Deserialize)]
struct StorageSpaceInfo {
    total: usize,
    free: usize,
    min_allowed_free: usize,
}
const STOR_MIN_FREE: usize = 512 * 1024;
fn get_storage_space_info() -> Result<StorageSpaceInfo> {
    let mut total = 0;
    let mut used = 0;
    let res = unsafe { esp_littlefs_info(STOR_LBL_CSTR.as_ptr(), &mut total, &mut used) };
    if res != 0 {
        anyhow::bail!("esp_littlefs_info failed; esp_err_t = {res}");
    }
    AOk(StorageSpaceInfo {
        total,
        free: total - used,
        min_allowed_free: STOR_MIN_FREE,
    })
}
fn is_free_space_ok() -> Result<()> {
    let i = get_storage_space_info()?;
    if i.free < i.min_allowed_free {
        anyhow::bail!("not enough free space")
    } else {
        AOk(())
    }
}

struct LastLine {
    l: LazyLock<Mutex<String>>,
}
impl LastLine {
    const fn new() -> Self {
        Self {
            l: LazyLock::new(|| Mutex::new(String::new())),
        }
    }
    fn get(&self) -> Result<String> {
        anyhow_lock(&self.l, "LastLine get").and_then(|s| AOk(s.clone()))
    }
    fn set(&self, s: &str) -> Result<()> {
        let mut l = anyhow_lock(&self.l, "LastLine set")?;
        l.clear();
        l.push_str(s);
        AOk(())
    }
}
struct DataFile {}
impl DataFile {
    const PATH: &str = DATA_FILE_PATH;
    const HEADER: &str = "rtc_ts,w,v,a,uptime_ms";
    const fn new() -> Self {
        Self {}
    }
    fn open_file(&self, o: &mut OpenOptions) -> File {
        o.open(Self::PATH)
            .unwrap_or_else(|_| panic!("failed to open file: {}", Self::PATH))
    }
    fn get_file_append(&self) -> File {
        self.open_file(OpenOptions::new().append(true).create(true))
    }
    fn get_file_read(&self) -> File {
        self.get_file_append(); // to create if it doesn't exist
        self.open_file(OpenOptions::new().read(true))
    }
    fn len(&self) -> Result<u64> {
        let f = self.get_file_read();
        let len = f.metadata()?.len();
        AOk(len)
    }
    fn append_line_raw(&self, l: &str) -> Result<()> {
        let mut f = self.get_file_append();
        writeln!(f, "{l}")?;
        f.sync_all()?;
        AOk(())
    }
    fn write_header_if_needed(&self) -> Result<()> {
        if self.len()? == 0 {
            self.append_line_raw(Self::HEADER)?;
        }
        AOk(())
    }
    fn append_data(&self, d: &str) -> Result<()> {
        if is_free_space_ok().is_err() {
            log::warn!("append_data canceled due to lack of minimum free space");
            return AOk(());
        }
        self.write_header_if_needed()?;
        self.append_line_raw(d)?;
        LAST_LINE.set(d)?;
        AOk(())
    }
    fn clear_data(&self) -> Result<()> {
        fs::remove_file(Self::PATH)?;
        AOk(())
    }
}
struct LockedDataFile {
    locker: LazyLock<Mutex<DataFile>>,
}
impl LockedDataFile {
    const fn new() -> Self {
        Self {
            locker: LazyLock::new(|| Mutex::new(DataFile::new())),
        }
    }
    fn lock(&self) -> Result<MutexGuard<'_, DataFile>> {
        anyhow_lock(&self.locker, "LockedDataFile lock")
    }
    fn append_data(&self, d: &str) -> Result<()> {
        self.lock().and_then(|f| f.append_data(d))
    }
    fn clear_data(&self) -> Result<()> {
        self.lock().and_then(|f| f.clear_data())
    }
}
struct SettingsFile {}
impl SettingsFile {
    const PATH: &str = SETTINGS_FILE_PATH;
    const fn new() -> Self {
        Self {}
    }
    fn set_str(&self, s: &str) -> Result<()> {
        fs::write(Self::PATH, s)?;
        AOk(())
    }
    fn set_default_if_needed(&self, force: bool) -> Result<()> {
        if !fs::exists(Self::PATH)? || force {
            let s = serde_json::to_string(&Settings::default())?;
            self.set_str(&s)?;
        }
        AOk(())
    }
    fn set(&self, s: &Settings) -> Result<()> {
        let s = serde_json::to_string(s)?;
        self.set_str(&s)?;
        AOk(())
    }
    fn get_str_from_utf8(&self) -> Result<String> {
        self.set_default_if_needed(false)?;
        let r = fs::read(Self::PATH)?;
        let s = String::from_utf8(r)?;
        AOk(s)
    }
    fn get_str(&self) -> Result<String> {
        let s = match self.get_str_from_utf8() {
            Ok(s) => s,
            Err(e) => {
                log::error!("{} is bad; error: {}; resetting to defaults", Self::PATH, e);
                self.set_default_if_needed(true)?;
                self.get_str_from_utf8()?
            }
        };
        AOk(s)
    }
    fn get(&self) -> Result<Settings> {
        let str = self.get_str()?;
        let s = match serde_json::from_str(&str) {
            Ok(s) => s,
            Err(e) => {
                log::error!(
                    "'{}' from {} is bad; error: {}; resetting to defaults",
                    str,
                    Self::PATH,
                    e
                );
                self.set_default_if_needed(true)?;
                serde_json::from_str(&self.get_str()?)?
            }
        };
        AOk(s)
    }
}
struct LockedSettingsFile {
    locker: LazyLock<Mutex<SettingsFile>>,
}
impl LockedSettingsFile {
    const fn new() -> Self {
        Self {
            locker: LazyLock::new(|| Mutex::new(SettingsFile::new())),
        }
    }
    fn lock(&self) -> Result<MutexGuard<'_, SettingsFile>> {
        anyhow_lock(&self.locker, "LockedSettingsFile lock")
    }
    fn set(&self, s: &Settings) -> Result<()> {
        self.lock().and_then(|f| f.set(s))
    }
    fn get(&self) -> Result<Settings> {
        self.lock().and_then(|f| f.get())
    }
}
static LAST_LINE: LastLine = LastLine::new();
static DATA_FILE: LockedDataFile = LockedDataFile::new();
static SETTINGS_FILE: LockedSettingsFile = LockedSettingsFile::new();

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
fn restart() {
    unsafe {
        esp_restart();
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

#[derive(Serialize, Deserialize)]
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
        AOk(())
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
        AOk(RtcDateTime::new(year, month, day, hour, minute, second))
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
        AOk(format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
        ))
    }
}
struct INA219 {
    addr: u8,
    timeout: TickType_t,
    _r_shunt: f64,              // Ω
    _max_expected_current: f64, // A
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
            _r_shunt: r_shunt,
            _max_expected_current: max_expected_current,
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
        AOk(u16::from_be_bytes(buf))
    }
    fn read_i16(&mut self, i2c: &mut I2cDriver, reg: u8) -> Result<i16> {
        self.read_u16(i2c, reg).map(|v| v as i16)
    }
    fn write_u16(&mut self, i2c: &mut I2cDriver, reg: u8, v: u16) -> Result<()> {
        let mut buf = [reg, 0, 0];
        buf[1..].copy_from_slice(&v.to_be_bytes());
        i2c.write(self.addr, &buf, self.timeout)?;
        AOk(())
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
        AOk(if sv.is_sign_negative() { bv } else { sv + bv })
    }
}
struct I2cDevices {
    i2c: I2cDriver<'static>,
    ds3231: DS3231,
    ina219: INA219,
}
impl I2cDevices {
    fn new(i2c: I2cDriver<'static>, ds3231: DS3231, ina219: INA219) -> Result<Self> {
        let mut s = Self {
            i2c,
            ds3231,
            ina219,
        };
        let i2c = &mut s.i2c;
        s.ina219.write_conf(i2c)?;
        s.ina219.write_calibration(i2c)?;
        AOk(s)
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

fn anyhow_lock<'a, T>(v: &'a Mutex<T>, err_prefix: &'static str) -> Result<MutexGuard<'a, T>> {
    v.lock()
        .map_err(|e| anyhow::anyhow!("{err_prefix} anyhow_lock error: {e}"))
}
fn record_measurements(i2c: &Mutex<I2cDevices>) -> Result<f64> {
    let mut i2c = anyhow_lock(i2c, "record_measurements i2c")?;
    let uptime_ms = uptime_usec() / 1000;
    let rtc_ts = i2c.read_ds3231_rtc_str()?;
    let w = get_smoothed::<0>(i2c.read_ina219_w()?);
    let v = get_smoothed::<1>(i2c.read_ina219_v()?);
    let a = get_smoothed::<2>(i2c.read_ina219_a()?);
    let line = format!("{rtc_ts},{w:.2},{v:.2},{a:.3},{uptime_ms}");
    log::info!("{line}");
    DATA_FILE.append_data(&line)?;
    AOk(v)
}
fn setup_wifi<'a>(modem: Modem) -> Result<EspWifi<'a>> {
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    let mut wifi = EspWifi::new(modem, sys_loop, Some(nvs))?;
    let s = SETTINGS_FILE.get()?;
    let conf = WiFiConf::AccessPoint(AccessPointConfiguration {
        ssid: s
            .wifi_ssid
            .as_str()
            .try_into()
            .map_err(|_| anyhow::anyhow!("ssid error"))?,
        ssid_hidden: false,
        channel: 1,
        secondary_channel: None,
        protocols: Protocol::P802D11BGN.into(),
        auth_method: AuthMethod::WPA2Personal,
        password: s
            .wifi_pass
            .as_str()
            .try_into()
            .map_err(|_| anyhow::anyhow!("password error"))?,
        max_connections: 10,
    });
    wifi.set_configuration(&conf)?;
    wifi.start()?;
    AOk(wifi)
}
fn setup_http<'a>(i2c: Arc<Mutex<I2cDevices>>, tx: Sender<Msg>) -> Result<EspHttpServer<'a>> {
    use embedded_svc::io::Read;
    let get_status_fn_i2c = i2c.clone();
    let set_rtc_fn_i2c = i2c.clone();
    let get_status_fn_tx = tx.clone();
    let restart_fn_tx = tx.clone();
    let set_settings_fn_tx = tx.clone();
    let mut http_server = EspHttpServer::new(&HttpConf::default())?;
    http_server.fn_handler("/", HttpMethod::Get, |rq| {
        let mut rs = rq.into_ok_response()?;
        rs.write(include_bytes!("../web/index.html"))?;
        AOk(())
    })?;
    http_server.fn_handler("/restart", HttpMethod::Get, move |rq| {
        let mut rs = rq.into_ok_response()?;
        rs.write(b"Restarting")?;
        restart_fn_tx.send(Msg::Restart)?;
        AOk(())
    })?;
    #[derive(Serialize, Deserialize)]
    struct Status {
        uptime_usec: i64,
        storage_space_info: StorageSpaceInfo,
        rtc_ts: String,
        last_line: String,
    }
    http_server.fn_handler("/get_status", HttpMethod::Get, move |rq| {
        let mut rs = rq.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
        let mut i2c = anyhow_lock(&get_status_fn_i2c, "get_status i2c")?;
        let s = Status {
            uptime_usec: uptime_usec(),
            storage_space_info: get_storage_space_info()?,
            rtc_ts: i2c.read_ds3231_rtc_str()?,
            last_line: LAST_LINE.get()?,
        };
        let s = serde_json::to_string(&s)?;
        rs.write(s.as_bytes())?;
        get_status_fn_tx.send(Msg::KeepAlive)?;
        AOk(())
    })?;
    http_server.fn_handler("/set_rtc", HttpMethod::Post, move |mut rq| {
        let (h, b) = rq.split();
        let clen = h.content_len().unwrap_or(0) as usize;
        let mut buf = vec![0u8; clen];
        b.read_exact(&mut buf)?;
        let dt = match serde_json::from_slice(&buf) {
            Ok(dt) => dt,
            Err(e) => {
                let mut rs = rq.into_response(400, Some("Bad Request"), &[])?;
                rs.write(format!("Invalid JSON: {e}").as_bytes())?;
                return AOk(());
            }
        };
        let mut rs = rq.into_ok_response()?;
        let mut i2c = anyhow_lock(&set_rtc_fn_i2c, "set_rtc i2c")?;
        i2c.set_ds3231_rtc(&dt)?;
        rs.write(b"RTC updated")?;
        AOk(())
    })?;
    http_server.fn_handler("/get_data", HttpMethod::Get, |rq| {
        let mut rs = rq.into_response(200, Some("OK"), &[("Content-Type", "text/plain")])?;
        let mut buf = vec![0; 64 * 1024];
        let f = DATA_FILE.lock()?;
        let mut f = f.get_file_read();
        loop {
            let bytes_read = f.read(&mut buf)?;
            if bytes_read == 0 {
                break;
            }
            rs.write(&buf[0..bytes_read])?;
        }
        AOk(())
    })?;
    http_server.fn_handler("/clear_data", HttpMethod::Get, |rq| {
        let mut rs = rq.into_ok_response()?;
        DATA_FILE.clear_data()?;
        rs.write(b"Cleared data")?;
        AOk(())
    })?;
    http_server.fn_handler("/get_settings", HttpMethod::Get, |rq| {
        let mut rs = rq.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
        let s = SETTINGS_FILE.get()?;
        let s = serde_json::to_string(&s)?;
        rs.write(s.as_bytes())?;
        AOk(())
    })?;
    http_server.fn_handler("/set_settings", HttpMethod::Post, move |mut rq| {
        let (h, b) = rq.split();
        let clen = h.content_len().unwrap_or(0) as usize;
        let mut buf = vec![0u8; clen];
        b.read_exact(&mut buf)?;
        let mut s = match serde_json::from_slice::<Settings>(&buf) {
            Ok(s) => s,
            Err(e) => {
                let mut rs = rq.into_response(400, Some("Bad Request"), &[])?;
                rs.write(format!("Invalid JSON: {e}").as_bytes())?;
                return AOk(());
            }
        };
        s.wifi_pass = s.wifi_pass.trim().to_string();
        s.wifi_ssid = s.wifi_ssid.trim().to_string();
        let mut rs = rq.into_ok_response()?;
        SETTINGS_FILE.set(&s)?;
        rs.write(b"Settings updated; Restarting")?;
        set_settings_fn_tx.send(Msg::Restart)?;
        AOk(())
    })?;
    AOk(http_server)
}
enum Msg {
    Restart,
    KeepAlive,
}
struct LaterVars<'a> {
    n: Instant,
    rx: Receiver<Msg>,
    led: Ws2812Esp32RmtDriver<'a>,
    _w: EspWifi<'a>,
    _h: EspHttpServer<'a>,
}
impl<'a> LaterVars<'a> {
    const HI_POWER_MODE_DUR: Duration = Duration::from_mins(2);
    const LED_STATES: [[u8; 3]; 3] = [[0, 0, 0], [0, 0x20, 0], [0, 0, 0x20]];
    fn set_led_state_log_error(&mut self, state: usize) {
        if let Err(e) = self.led.write_blocking(Self::LED_STATES[state].into_iter()) {
            log::warn!("error set led {state}: {e}");
        }
    }
    fn set_led_state_0(&mut self) {
        self.set_led_state_log_error(0);
    }
    fn set_led_state_1(&mut self) {
        self.set_led_state_log_error(1);
    }
    fn set_led_state_2(&mut self) {
        self.set_led_state_log_error(2);
    }
    fn handle_msgs(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Restart => {
                    restart();
                }
                Msg::KeepAlive => {
                    self.reset_high_power_mode_timer();
                }
            }
        }
    }
    fn reset_high_power_mode_timer(&mut self) {
        self.n = Instant::now();
    }
    fn should_end_high_power_mode(&self) -> bool {
        self.n.elapsed() >= Self::HI_POWER_MODE_DUR
    }
}
enum Iter<'a> {
    First,
    NotFirst(LaterVars<'a>),
}
impl<'a> Iter<'a> {
    fn if_notfirst_take_or_else(self, mut op: impl FnMut() -> Result<Self>) -> Result<Self> {
        if let Self::First = self {
            op()
        } else {
            AOk(self)
        }
    }
    fn if_not_first(&mut self, mut op: impl FnMut(&mut LaterVars<'a>)) {
        if let Self::NotFirst(vars) = self {
            op(vars);
        }
    }
    fn if_notfirst_led_state_0(&mut self) {
        self.if_not_first(|vars| vars.set_led_state_0());
    }
    fn if_notfirst_led_state_1(&mut self) {
        self.if_not_first(|vars| vars.set_led_state_1());
    }
    fn if_notfirst_led_state_2(&mut self) {
        self.if_not_first(|vars| vars.set_led_state_2());
    }
    fn if_notfirst_handle_msgs(&mut self) {
        self.if_not_first(|vars| vars.handle_msgs());
    }
    fn if_notfirst_reset_high_power_mode_timer(&mut self) {
        self.if_not_first(|vars| vars.reset_high_power_mode_timer());
    }
    fn if_notfirst_if_continue_high_power_mode_or_else(&mut self, mut op: impl FnMut()) {
        self.if_not_first(|vars| {
            if vars.should_end_high_power_mode() {
                op();
            }
        });
    }
}
struct Sleeper {
    t0: OnceCell<Instant>,
    min_sleep: Duration,
    max_sleep: Duration,
}
impl Sleeper {
    const fn new(min_sleep: Duration, max_sleep: Duration) -> Self {
        Self {
            t0: OnceCell::new(),
            min_sleep,
            max_sleep,
        }
    }
    fn set_t0_now_sub_if_unset(&mut self, sub: Duration) {
        let _ = self.t0.set(Instant::now() - sub);
    }
    fn get_remaining(&mut self, d: Duration) -> Duration {
        let t0 = self.t0.take().unwrap_or_else(Instant::now);
        d.saturating_sub(t0.elapsed())
            .max(self.min_sleep)
            .min(self.max_sleep)
    }
    fn sleep_up_to(&mut self, d: Duration) {
        let r = self.get_remaining(d);
        sleep(r);
    }
    fn reset_then_sleep_up_to(&mut self, d: Duration) {
        let r = self.get_remaining(d);
        reset_then_sleep(
            u64::try_from(r.as_micros())
                .expect("failure casting remaining duration to u64 in reset_then_sleep_up_to"),
        );
    }
}
struct SleeperWithPresets {
    sleeper: Sleeper,
    short_sleep_dur: Duration,
    low_power_dur: Duration,
    very_low_power_dur: Duration,
}
impl SleeperWithPresets {
    fn new(
        min_sleep: Duration,
        max_sleep: Duration,
        short_sleep_dur: Duration,
        low_power_dur: Duration,
        very_low_power_dur: Duration,
    ) -> Self {
        Self {
            sleeper: Sleeper::new(min_sleep, max_sleep),
            short_sleep_dur,
            low_power_dur,
            very_low_power_dur,
        }
    }
    fn set_t0_now_sub_if_unset(&mut self, sub: Duration) {
        self.sleeper.set_t0_now_sub_if_unset(sub);
    }
    fn short_sleep(&mut self) {
        self.sleeper.sleep_up_to(self.short_sleep_dur);
    }
    fn enter_low_power(&mut self) {
        self.sleeper.reset_then_sleep_up_to(self.low_power_dur);
    }
    fn enter_very_low_power(&mut self) {
        self.sleeper.reset_then_sleep_up_to(self.very_low_power_dur);
    }
}
fn enter_very_low_power<'a>(iter: &mut Iter<'a>, sleeper: &mut SleeperWithPresets) {
    iter.if_notfirst_led_state_0();
    sleeper.enter_very_low_power();
}
fn enter_low_power<'a>(iter: &mut Iter<'a>, sleeper: &mut SleeperWithPresets) {
    iter.if_notfirst_led_state_0();
    sleeper.enter_low_power();
}
fn init_led<'a, C: RmtChannel>(
    channel: impl Peripheral<P = C> + 'a,
    pin: impl Peripheral<P = impl OutputPin> + 'a,
) -> Result<Option<Ws2812Esp32RmtDriver<'a>>> {
    let mut led = Ws2812Esp32RmtDriver::new(channel, pin)?;
    led.write_blocking([0, 0, 0].into_iter())?;
    AOk(Some(led))
}
fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    log::set_max_level(log::LevelFilter::Debug);
    const HI_V: f64 = 13.0;
    const LO_V: f64 = 12.2;
    const MAX_SLEEP_DUR: Duration = Duration::from_secs(60);
    const LO_V_SLEEP_DUR: Duration = Duration::from_secs(60);
    const RECORD_SLEEP_DUR: Duration = Duration::from_secs(10);
    const MIN_SLEEP_DUR: Duration = Duration::from_secs(5);
    let mut sleeper = SleeperWithPresets::new(
        MIN_SLEEP_DUR,
        MAX_SLEEP_DUR,
        RECORD_SLEEP_DUR,
        RECORD_SLEEP_DUR,
        LO_V_SLEEP_DUR,
    );
    sleeper.set_t0_now_sub_if_unset(Duration::from_micros(uptime_usec() as u64));
    feed_watchdog();
    let _storage = mount_storage()?;
    let peripherals = Peripherals::take()?;
    let mut led = init_led(peripherals.rmt.channel0, peripherals.pins.gpio8)?;
    let i2c = I2cDevices::new(
        I2cDriver::new(
            peripherals.i2c0,
            peripherals.pins.gpio3,
            peripherals.pins.gpio2,
            &I2cConfig::new().baudrate(400.kHz().into()),
        )?,
        DS3231::new(0x68),
        INA219::new(0x41, 0.1, 3.2, 0x3FFF), // 0x3FFF based on https://www.ti.com/lit/ds/symlink/ina219.pdf
    )?;
    let i2c = Arc::new(Mutex::new(i2c));
    let woke_from_sleep = woke_from_sleep();
    let mut wifi_modem = Some(peripherals.modem);
    let mut iter = Iter::First;
    let mut mk_notfirst = || {
        let (tx, rx) = channel();
        let _w = setup_wifi(wifi_modem.take().expect("wifi_modem is taken once"))?;
        let _h = setup_http(i2c.clone(), tx)?;
        let n = Instant::now();
        let led = led.take().expect("led is taken once");
        let mut vars = LaterVars { rx, led, _w, _h, n };
        vars.set_led_state_2();
        AOk(Iter::NotFirst(vars))
    };
    loop {
        sleeper.set_t0_now_sub_if_unset(Duration::ZERO);
        feed_watchdog();
        iter.if_notfirst_led_state_1();
        match record_measurements(&i2c) {
            Err(e) => {
                log::error!("record_measurements error: {e}");
                enter_very_low_power(&mut iter, &mut sleeper);
            }
            Ok(v) => {
                if v <= LO_V {
                    enter_very_low_power(&mut iter, &mut sleeper);
                } else if v <= HI_V && woke_from_sleep {
                    enter_low_power(&mut iter, &mut sleeper);
                } else if v > HI_V {
                    iter.if_notfirst_reset_high_power_mode_timer();
                }
            }
        }
        iter.if_notfirst_led_state_2();
        iter.if_notfirst_handle_msgs();
        iter.if_notfirst_if_continue_high_power_mode_or_else(|| sleeper.enter_low_power());
        iter = iter.if_notfirst_take_or_else(&mut mk_notfirst)?;
        sleeper.short_sleep();
    }
}
