//! Registry-backed settings: start-with-Windows and the taskbar theme (plus the Windows
//! version, for diagnostics).

use crate::wide;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::*;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APP_KEY: &str = r"Software\SETTMetter";
const RUN_VALUE: &str = "SETTMetter";

/// Cache of the settings read on every redraw/menu build (`hidden_devices`, `sensor_mask`,
/// `hidden_gpu_sensors`, `icon_mask`, `refresh_interval_ms`), so the sysmonitor/redraw hot path
/// (ticking every 1-5 s) doesn't open and read an HKCU key each time. Cleared by every setter,
/// so the next read after a menu toggle always sees the fresh value.
#[derive(Clone, Default)]
struct Cache {
    hidden_devices: Vec<String>,
    sensor_mask: u32,
    hidden_gpu_sensors: Vec<String>,
    icon_mask: u32,
    refresh_interval_ms: u64,
    cpu_alert_threshold: u32,
    gpu_alert_threshold: u32,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn cache() -> Cache {
    let mut guard = CACHE.lock().unwrap();
    if guard.is_none() {
        *guard = Some(Cache {
            hidden_devices: read_hidden_devices(),
            sensor_mask: read_sensor_mask(),
            hidden_gpu_sensors: read_hidden_gpu_sensors(),
            icon_mask: read_icon_mask(),
            refresh_interval_ms: read_refresh_interval_ms(),
            cpu_alert_threshold: read_temp_threshold("CpuAlertThreshold"),
            gpu_alert_threshold: read_temp_threshold("GpuAlertThreshold"),
        });
    }
    guard.clone().unwrap()
}

fn invalidate_cache() {
    *CACHE.lock().unwrap() = None;
}

pub fn light_theme() -> bool {
    read_dword(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "SystemUsesLightTheme") == Some(1)
}

pub fn autostart_enabled() -> bool {
    let (k, v) = (wide(RUN_KEY), wide(RUN_VALUE));
    let mut size = 0u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut size)
            == ERROR_SUCCESS
    }
}

pub fn set_autostart(on: bool) {
    let (k, v) = (wide(RUN_KEY), wide(RUN_VALUE));
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let data = wide(&format!("\"{}\"", exe.display()));
            RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), REG_SZ, data.as_ptr() as *const c_void, (data.len() * 2) as u32);
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr());
        }
    }
    write_dword(APP_KEY, "AutostartOptOut", u32::from(!on));
}

/// Release builds register themselves to start with Windows (and refresh the path if the
/// exe moved), unless the user turned it off from the menu.
pub fn ensure_autostart() {
    if cfg!(debug_assertions) || read_dword(APP_KEY, "AutostartOptOut") == Some(1) {
        return;
    }
    set_autostart(true);
}

pub fn alerts_enabled() -> bool {
    read_dword(APP_KEY, "LowBatteryAlerts") != Some(0)
}

pub fn set_alerts(on: bool) {
    write_dword(APP_KEY, "LowBatteryAlerts", u32::from(on));
}

const NOTIFICATION_ICON: &[u8] = include_bytes!("../assets/icon-64.png");

/// Registers the AppUserModelID so Windows accepts our toast notifications, with our icon
/// (written to %LOCALAPPDATA%\SETTMetter\icon.png, since Windows needs a file).
pub fn register_app_id() {
    let key = format!(r"Software\Classes\AppUserModelId\{}", crate::notify::APP_ID);
    write_string(&key, "DisplayName", "SETTMetter");
    if let Some(dir) = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("SETTMetter")) {
        let icon = dir.join("icon.png");
        if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&icon, NOTIFICATION_ICON).is_ok() {
            write_string(&key, "IconUri", &icon.to_string_lossy());
        }
    }
}

fn write_string(key: &str, name: &str, value: &str) {
    let (k, n, data) = (wide(key), wide(name), wide(value));
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_SZ, data.as_ptr() as *const c_void, (data.len() * 2) as u32);
    }
}

/// Device ids the user unchecked in the menu.
pub fn hidden_devices() -> Vec<String> {
    cache().hidden_devices
}

fn read_hidden_devices() -> Vec<String> {
    read_string(APP_KEY, "Hidden").map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default()
}

pub fn toggle_hidden(id: &str) {
    let mut hidden = read_hidden_devices();
    match hidden.iter().position(|h| h == id) {
        Some(i) => {
            hidden.remove(i);
        }
        None => hidden.push(id.to_string()),
    }
    let (k, n, data) = (wide(APP_KEY), wide("Hidden"), wide(&hidden.join("\n")));
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_SZ, data.as_ptr() as *const c_void, (data.len() * 2) as u32);
    }
    invalidate_cache();
}

/// Bitmask of which system-monitor sensors are shown (see `sysmonitor::ALL_SENSORS` for bits).
/// All on by default, like TraySense's `g_mask = (1u << M_N) - 1`.
pub fn sensor_mask() -> u32 {
    cache().sensor_mask
}

fn read_sensor_mask() -> u32 {
    read_dword(APP_KEY, "SensorMask").unwrap_or(crate::sysmonitor::ALL_SENSORS) & crate::sysmonitor::ALL_SENSORS
}

pub fn set_sensor_mask(mask: u32) {
    write_dword(APP_KEY, "SensorMask", mask & crate::sysmonitor::ALL_SENSORS);
    invalidate_cache();
}

pub fn toggle_sensor(bit: u32) {
    let mask = sensor_mask() ^ bit;
    if mask != 0 {
        set_sensor_mask(mask);
    }
}

/// Per-adapter GPU/VRAM sensor ids the user unchecked in the menu, e.g. `"gpu0"`, `"vram1"`,
/// for notebooks/desktops with more than one discrete GPU (each adapter gets its own checkbox
/// instead of one toggle for every GPU).
pub fn hidden_gpu_sensors() -> Vec<String> {
    cache().hidden_gpu_sensors
}

fn read_hidden_gpu_sensors() -> Vec<String> {
    read_string(APP_KEY, "HiddenGpuSensors").map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default()
}

pub fn toggle_gpu_sensor(id: &str) {
    let mut hidden = read_hidden_gpu_sensors();
    match hidden.iter().position(|h| h == id) {
        Some(i) => {
            hidden.remove(i);
        }
        None => hidden.push(id.to_string()),
    }
    set_hidden_gpu_sensors(hidden);
}

/// Replaces the whole hidden-GPU-sensor list in one write, used by the quick presets to put
/// every adapter's usage/temperature/VRAM checkboxes into a known state at once.
pub fn set_hidden_gpu_sensors(hidden: Vec<String>) {
    let (k, n, data) = (wide(APP_KEY), wide("HiddenGpuSensors"), wide(&hidden.join("\n")));
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_SZ, data.as_ptr() as *const c_void, (data.len() * 2) as u32);
    }
    invalidate_cache();
}

/// Bitmask of which group icons (CPU/GPU/RAM/VRAM/SSD) are drawn next to their values.
/// All on by default. See `sysmonitor::ALL_ICONS`.
pub fn icon_mask() -> u32 {
    cache().icon_mask
}

fn read_icon_mask() -> u32 {
    read_dword(APP_KEY, "IconMask").unwrap_or(crate::sysmonitor::ALL_ICONS) & crate::sysmonitor::ALL_ICONS
}

pub fn set_icon_mask(mask: u32) {
    write_dword(APP_KEY, "IconMask", mask & crate::sysmonitor::ALL_ICONS);
    invalidate_cache();
}

pub fn toggle_icon(bit: u32) {
    set_icon_mask(icon_mask() ^ bit);
}

/// Sample/refresh interval in milliseconds for the system monitor: 1000, 2000 (default) or
/// 5000, same choices as TraySense's "Refresh rate" submenu.
pub fn refresh_interval_ms() -> u64 {
    cache().refresh_interval_ms
}

fn read_refresh_interval_ms() -> u64 {
    match read_dword(APP_KEY, "RefreshInterval") {
        Some(v @ (1000 | 2000 | 5000)) => v as u64,
        _ => 2000,
    }
}

pub fn set_refresh_interval(ms: u32) {
    write_dword(APP_KEY, "RefreshInterval", ms);
    invalidate_cache();
}

/// CPU temperature alert threshold in \u00b0C. Default 85, matching the GPU default so both
/// start aligned; each is independently configurable from the menu.
pub fn cpu_alert_threshold() -> u32 {
    cache().cpu_alert_threshold
}

pub fn set_cpu_alert_threshold(celsius: u32) {
    write_dword(APP_KEY, "CpuAlertThreshold", celsius);
    invalidate_cache();
}

/// GPU temperature alert threshold in \u00b0C. Default 85, same rationale as `cpu_alert_threshold`.
pub fn gpu_alert_threshold() -> u32 {
    cache().gpu_alert_threshold
}

pub fn set_gpu_alert_threshold(celsius: u32) {
    write_dword(APP_KEY, "GpuAlertThreshold", celsius);
    invalidate_cache();
}

fn read_temp_threshold(name: &str) -> u32 {
    read_dword(APP_KEY, name).filter(|&v| (40..=110).contains(&v)).unwrap_or(85)
}

/// E.g. "Windows 11 24H2 build 26100.2033", for diagnostics.
pub fn windows_version() -> String {
    const KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let build: u32 = read_string_in(HKEY_LOCAL_MACHINE, KEY, "CurrentBuild").and_then(|b| b.parse().ok()).unwrap_or(0);
    let ubr = read_dword_in(HKEY_LOCAL_MACHINE, KEY, "UBR").unwrap_or(0);
    let release = read_string_in(HKEY_LOCAL_MACHINE, KEY, "DisplayVersion").unwrap_or_default();
    // ProductName still says "Windows 10" on Windows 11; the build tells them apart.
    let name = if build >= 22000 { "Windows 11" } else { "Windows 10" };
    format!("{name} {release} build {build}.{ubr}")
}

fn read_string(key: &str, name: &str) -> Option<String> {
    read_string_in(HKEY_CURRENT_USER, key, name)
}

fn read_string_in(root: HKEY, key: &str, name: &str) -> Option<String> {
    let (k, n) = (wide(key), wide(name));
    let mut size = 0u32;
    unsafe {
        if RegGetValueW(root, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut size) != ERROR_SUCCESS {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        if RegGetValueW(root, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr() as *mut c_void, &mut size) != ERROR_SUCCESS {
            return None;
        }
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

fn read_dword(key: &str, name: &str) -> Option<u32> {
    read_dword_in(HKEY_CURRENT_USER, key, name)
}

fn read_dword_in(root: HKEY, key: &str, name: &str) -> Option<u32> {
    let (k, n) = (wide(key), wide(name));
    let (mut value, mut size) = (0u32, 4u32);
    let r = unsafe {
        RegGetValueW(
            root,
            k.as_ptr(),
            n.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut value as *mut u32 as *mut c_void,
            &mut size,
        )
    };
    (r == ERROR_SUCCESS).then_some(value)
}

fn write_dword(key: &str, name: &str, value: u32) {
    let (k, n) = (wide(key), wide(name));
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_DWORD, &value as *const u32 as *const c_void, 4);
    }
}
