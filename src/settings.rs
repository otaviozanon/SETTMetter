//! Registry-backed settings: start-with-Windows and the taskbar theme (plus the Windows
//! version, for diagnostics).

use crate::wide;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::Mutex;
use windows::core::{BSTR, Interface, VARIANT};
use windows::Win32::Foundation::VARIANT_FALSE;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::TaskScheduler::{
    IExecAction, ILogonTrigger, ITaskService, ITaskFolder, TaskScheduler, TASK_ACTION_EXEC, TASK_CREATE_OR_UPDATE,
    TASK_LOGON_INTERACTIVE_TOKEN, TASK_RUNLEVEL_HIGHEST, TASK_TRIGGER_LOGON,
};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::*;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APP_KEY: &str = r"Software\SETTMetter";
const RUN_VALUE: &str = "SETTMetter";
const TASK_NAME: &str = "SETTMetter";

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
    screen: u32,
    widget_offset: i32,
    widget_spacing: i32,
    mute_vk: Option<u32>,
    mute_mods: u32,
    show_mic: bool,
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
            screen: read_dword(APP_KEY, "Screen").unwrap_or(0),
            widget_offset: read_dword(APP_KEY, "WidgetOffset").map(|v| (v as i32).clamp(-800, 800)).unwrap_or(0),
            widget_spacing: read_dword(APP_KEY, "WidgetSpacing")
                .map(|v| (v as i32).clamp(SPACING_MIN, SPACING_MAX))
                .unwrap_or(100),
            // Break is the default so a fresh install has a working mute key; 0xFFFF means the
            // user removed the binding.
            mute_vk: match read_dword(APP_KEY, "MuteVk") {
                None => Some(0x13),
                Some(0xFFFF) => None,
                Some(vk) if vk <= 0xFF => Some(vk),
                Some(_) => None,
            },
            mute_mods: read_dword(APP_KEY, "MuteMods").unwrap_or(0) & 0x0F,
            show_mic: read_dword(APP_KEY, "ShowMic") != Some(0),
        });
    }
    guard.clone().unwrap()
}

fn invalidate_cache() {
    *CACHE.lock().unwrap() = None;
}

/// The system-wide mic mute shortcut: `(virtual key, modifier mask)`, or `None` when unbound.
/// Break is the default, matching Discord's own mute keybind.
pub fn mute_shortcut() -> Option<(u32, u32)> {
    let cache = cache();
    cache.mute_vk.map(|vk| (vk, cache.mute_mods))
}

pub fn set_mute_shortcut(shortcut: Option<(u32, u32)>) {
    match shortcut {
        Some((vk, mods)) => {
            write_dword(APP_KEY, "MuteVk", vk & 0xFF);
            write_dword(APP_KEY, "MuteMods", mods & 0x0F);
        }
        None => write_dword(APP_KEY, "MuteVk", 0xFFFF),
    }
    invalidate_cache();
}

/// Whether the microphone glyph is drawn in the widget. When off, its click shortcuts are gone
/// with it.
pub fn show_mic() -> bool {
    cache().show_mic
}

pub fn set_show_mic(on: bool) {
    write_dword(APP_KEY, "ShowMic", u32::from(on));
    invalidate_cache();
}

pub fn light_theme() -> bool {
    read_dword(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "SystemUsesLightTheme") == Some(1)
}

/// The logon task is the only autostart that works here: Windows blocks elevated programs in
/// the per-user Run key and Startup folder, and the manifest requires administrator. A task
/// registered with the highest run level starts the app without a UAC prompt.
pub fn autostart_enabled() -> bool {
    let Some(folder) = task_folder() else { return false };
    let Ok(exe) = std::env::current_exe() else { return false };
    // A task left over from an older path (the exe was moved) would start nothing.
    task_exe(&folder).is_some_and(|task_exe| task_exe.as_str() == exe.to_string_lossy().as_ref())
}

/// Path the existing task runs, or None when there is no enabled task.
fn task_exe(folder: &ITaskFolder) -> Option<String> {
    unsafe {
        let task = folder.GetTask(&BSTR::from(TASK_NAME)).ok()?;
        if !task.Enabled().ok()?.as_bool() {
            return None;
        }
        let action = task.Definition().ok()?.Actions().ok()?.get_Item(1).ok()?.cast::<IExecAction>().ok()?;
        let mut path = BSTR::new();
        action.Path(&mut path).ok()?;
        Some(path.to_string())
    }
}

pub fn set_autostart(on: bool) {
    if on {
        if !register_task() {
            crate::diag::event("autostart", "could not register the logon task");
        }
    } else if let Some(folder) = task_folder() {
        let _ = unsafe { folder.DeleteTask(&BSTR::from(TASK_NAME), 0) };
    }
    drop_legacy_autostart();
}

/// Older versions registered in the Run key, which Windows never actually started for this
/// elevated app. Removed so it stops showing up as a dead entry in Task Manager.
pub fn drop_legacy_autostart() {
    let (k, v) = (wide(RUN_KEY), wide(RUN_VALUE));
    unsafe {
        RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr());
    }
}

/// Requires COM initialized on the calling thread (`crate::init_com` in `widget::run`).
fn task_folder() -> Option<ITaskFolder> {
    unsafe {
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER).ok()?;
        let empty = VARIANT::default();
        service.Connect(&empty, &empty, &empty, &empty).ok()?;
        service.GetFolder(&BSTR::from("\\")).ok()
    }
}

fn register_task() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let exe = exe.to_string_lossy().into_owned();
    unsafe {
        let register = || -> windows::core::Result<()> {
            let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
            let empty = VARIANT::default();
            service.Connect(&empty, &empty, &empty, &empty)?;
            let folder = service.GetFolder(&BSTR::from("\\"))?;
            let def = service.NewTask(0)?;
            let principal = def.Principal()?;
            principal.SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)?;
            principal.SetRunLevel(TASK_RUNLEVEL_HIGHEST)?;
            let settings = def.Settings()?;
            // Defaults would skip the launch on battery and kill a tray app after three days.
            settings.SetDisallowStartIfOnBatteries(VARIANT_FALSE)?;
            settings.SetStopIfGoingOnBatteries(VARIANT_FALSE)?;
            settings.SetExecutionTimeLimit(&BSTR::from("PT0S"))?;
            let trigger: ILogonTrigger = def.Triggers()?.Create(TASK_TRIGGER_LOGON)?.cast()?;
            trigger.SetStartBoundary(&BSTR::from("2024-01-01T00:00:00"))?;
            let action: IExecAction = def.Actions()?.Create(TASK_ACTION_EXEC)?.cast()?;
            action.SetPath(&BSTR::from(exe.as_str()))?;
            if let Some(dir) = std::path::Path::new(&exe).parent() {
                action.SetWorkingDirectory(&BSTR::from(dir.to_string_lossy().as_ref()))?;
            }
            let empty = VARIANT::default();
            folder.RegisterTaskDefinition(
                &BSTR::from(TASK_NAME),
                &def,
                TASK_CREATE_OR_UPDATE.0,
                &empty,
                &empty,
                TASK_LOGON_INTERACTIVE_TOKEN,
                &empty,
            )?;
            Ok(())
        };
        register().is_ok()
    }
}

pub fn alerts_enabled() -> bool {
    read_dword(APP_KEY, "LowBatteryAlerts") != Some(0)
}

/// Whether the CPU-temperature driver prompt was already shown (either answer), so it appears
/// once instead of on every launch. The menu entry stays as the way back.
pub fn pawnio_prompted() -> bool {
    read_dword(APP_KEY, "PawnIOPrompted") == Some(1)
}

pub fn set_pawnio_prompted() {
    write_dword(APP_KEY, "PawnIOPrompted", 1);
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
/// All on by default.
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

/// Sample/refresh interval in milliseconds for the system monitor: 1000, 2000 (default) or 5000.
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

/// Windows display number (`\\.\DISPLAY1` = 1) of the monitor whose taskbar holds the widget.
/// 0 means the primary taskbar; `widget::find_tray` also falls back to it when the selected
/// screen is gone (unplugged).
pub fn screen() -> u32 {
    cache().screen
}

pub fn set_screen(number: u32) {
    write_dword(APP_KEY, "Screen", number);
    invalidate_cache();
}

/// Manual horizontal (or vertical, on a side-docked taskbar) offset in px, applied on top of
/// the automatic placement: positive moves the widget away from the notification area.
pub fn widget_offset() -> i32 {
    cache().widget_offset
}

pub fn set_widget_offset(offset: i32) {
    write_dword(APP_KEY, "WidgetOffset", offset.clamp(-800, 800) as u32);
    invalidate_cache();
}

/// Gaps between the segments of the taskbar widget, in percent of the default rhythm.
pub const SPACING_MIN: i32 = 50;
pub const SPACING_MAX: i32 = 200;

pub fn widget_spacing() -> i32 {
    cache().widget_spacing
}

pub fn set_widget_spacing(percent: i32) {
    let percent = percent.clamp(SPACING_MIN, SPACING_MAX);
    write_dword(APP_KEY, "WidgetSpacing", percent as u32);
    invalidate_cache();
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
