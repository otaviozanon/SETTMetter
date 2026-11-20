//! Right-click context menu: construction, command dispatch and the actions it triggers
//! (diagnostics copy, device probing).

use super::{readings, redraw, sys_snapshot, wake, with};
use crate::{diag, notify, probe, settings, sysmonitor, wide};
use std::mem::zeroed;
use std::ptr::null;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CMD_REFRESH: usize = 1;
const CMD_AUTOSTART: usize = 2;
const CMD_EXIT: usize = 3;
const CMD_ALERTS: usize = 4;
const CMD_COPY_DIAGNOSTICS: usize = 5;
const CMD_PROBE: usize = 6;
const CMD_HISTORY: usize = 7;
const CMD_DEVICE_BASE: usize = 100;
/// 7 entries (one per `sysmonitor::SENSOR_NAMES`), well clear of CMD_DEVICE_BASE's range.
const CMD_SENSOR_BASE: usize = 500;
const CMD_INTERVAL_1S: usize = 600;
const CMD_INTERVAL_2S: usize = 601;
const CMD_INTERVAL_5S: usize = 602;
/// 5 entries (one per `sysmonitor::ICON_NAMES`).
const CMD_ICON_BASE: usize = 650;
/// Up to 16 discrete GPUs get their own "GPU usage"/"GPU temperature"/"VRAM usage" checkbox.
const CMD_GPU_SENSOR_BASE: usize = 700;
const CMD_VRAM_SENSOR_BASE: usize = 720;
const CMD_GPU_TEMP_SENSOR_BASE: usize = 740;
/// One entry per `THRESHOLD_CHOICES` for the CPU alert-threshold submenu.
const CMD_CPU_THRESHOLD_BASE: usize = 760;
/// One entry per `THRESHOLD_CHOICES` for the GPU alert-threshold submenu.
const CMD_GPU_THRESHOLD_BASE: usize = 780;
/// One entry per `PRESETS` for the quick-preset submenu.
const CMD_PRESET_BASE: usize = 800;
/// One entry per screen, keyed by its Windows display number (1-based), for the screen submenu.
const CMD_SCREEN_BASE: usize = 900;

/// \u00b0C choices offered in the CPU/GPU temperature-alert submenus.
const THRESHOLD_CHOICES: [u32; 7] = [60, 70, 75, 80, 85, 90, 95];

/// What a preset does with each detected GPU's usage/temperature/VRAM checkboxes.
#[derive(Clone, Copy)]
enum GpuPreset {
    /// Show usage and temperature, hide VRAM.
    UsageAndTemp,
    /// Show usage only.
    UsageOnly,
    /// Show temperature only.
    TempOnly,
}

impl GpuPreset {
    /// Builds the `HiddenGpuSensors` list (see `settings::set_hidden_gpu_sensors`) that puts
    /// every adapter from `0..gpu_count` into this preset's state.
    fn hidden_list(self, gpu_count: usize) -> Vec<String> {
        let mut hidden = Vec::new();
        for i in 0..gpu_count {
            match self {
                GpuPreset::UsageAndTemp => hidden.push(format!("vram{i}")),
                GpuPreset::UsageOnly => {
                    hidden.push(format!("gputemp{i}"));
                    hidden.push(format!("vram{i}"));
                }
                GpuPreset::TempOnly => {
                    hidden.push(format!("gpu{i}"));
                    hidden.push(format!("vram{i}"));
                }
            }
        }
        hidden
    }
}

/// (label, sensor mask, icon mask, GPU preset) quick presets, applied together from one click
/// instead of unchecking sensors/icons one by one. CPU/RAM/GPU only — SSD and VRAM stay hidden
/// in every preset; uncheck them manually in "System sensors" if you want them.
const PRESETS: [(&str, u32, u32, GpuPreset); 3] = [
    ("Full (usage + temperatures)", sysmonitor::BIT_CPU | sysmonitor::BIT_CPU_TEMP | sysmonitor::BIT_RAM, sysmonitor::ALL_ICONS, GpuPreset::UsageAndTemp),
    ("Usage only", sysmonitor::BIT_CPU | sysmonitor::BIT_RAM, sysmonitor::ALL_ICONS, GpuPreset::UsageOnly),
    ("Temperatures only", sysmonitor::BIT_CPU_TEMP, sysmonitor::ALL_ICONS, GpuPreset::TempOnly),
];

pub(super) unsafe fn show_menu(hwnd: HWND) {
    let devices = readings();
    let hidden = settings::hidden_devices();
    let menu = CreatePopupMenu();
    let mut labels = Vec::new(); // keep the UTF-16 buffers alive while the menu exists

    for (i, r) in devices.iter().enumerate() {
        let label = wide(&format!("{}  —  {}", r.name, r.status));
        let check = if hidden.contains(&r.id) { MF_UNCHECKED } else { MF_CHECKED };
        AppendMenuW(menu, MF_STRING | check, CMD_DEVICE_BASE + i, label.as_ptr());
        labels.push(label);
    }
    if devices.is_empty() {
        let label = wide("No devices found");
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, label.as_ptr());
        labels.push(label);
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());

    // System monitor: one checkable item per sensor. CPU/RAM usage and CPU/SSD temperature are
    // single global toggles; GPU usage, GPU temperature and VRAM usage get one checkbox per
    // discrete adapter (see `hidden_gpu_sensors` in settings.rs), since a system can have more
    // than one GPU. Temperature items show "(no sensor found)" when the
    // corresponding sensor isn't available.
    let checked = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    let mask = settings::sensor_mask();
    let snap = sys_snapshot();
    let hidden_gpu = settings::hidden_gpu_sensors();
    let gpu_count = snap.as_ref().map(|s| s.gpus.len()).unwrap_or(0);
    let multi_gpu = gpu_count > 1;

    for (idx, (name, bit)) in sysmonitor::SENSOR_NAMES.iter().enumerate() {
        let has_sensor = match *bit {
            sysmonitor::BIT_CPU_TEMP => snap.as_ref().and_then(|s| s.cpu_temp).is_some(),
            sysmonitor::BIT_DISK_TEMP => snap.as_ref().and_then(|s| s.disk_temp).is_some(),
            _ => true, // usage sensors are always available
        };
        let suffix = if (mask & bit) != 0 && !has_sensor { "  (no sensor found)" } else { "" };
        let label = wide(&format!("{name}{suffix}"));
        AppendMenuW(menu, MF_STRING | checked(mask & bit != 0), CMD_SENSOR_BASE + idx, label.as_ptr());
        labels.push(label);
        // Insert the per-adapter GPU items right after "CPU temperature", matching the order
        // CPU usage / CPU temperature / GPU usage / GPU temperature / RAM usage / VRAM usage /
        // SSD temperature used on the taskbar.
        if *bit == sysmonitor::BIT_CPU_TEMP {
            for i in 0..gpu_count {
                let name = if multi_gpu { format!("GPU{} usage", i + 1) } else { "GPU usage".to_string() };
                let on = !hidden_gpu.iter().any(|h| h == &format!("gpu{i}"));
                let label = wide(&name);
                AppendMenuW(menu, MF_STRING | checked(on), CMD_GPU_SENSOR_BASE + i, label.as_ptr());
                labels.push(label);
            }
            for i in 0..gpu_count {
                let has_gpu_temp = snap.as_ref().is_some_and(|s| s.gpus.get(i).is_some_and(|g| g.temp.is_some()));
                let on = !hidden_gpu.iter().any(|h| h == &format!("gputemp{i}"));
                let base = if multi_gpu { format!("GPU{} temperature", i + 1) } else { "GPU temperature".to_string() };
                let suffix = if on && !has_gpu_temp { "  (no sensor found)" } else { "" };
                let label = wide(&format!("{base}{suffix}"));
                AppendMenuW(menu, MF_STRING | checked(on), CMD_GPU_TEMP_SENSOR_BASE + i, label.as_ptr());
                labels.push(label);
            }
        }
        if *bit == sysmonitor::BIT_RAM {
            for i in 0..gpu_count {
                let name = if multi_gpu { format!("VRAM{} usage", i + 1) } else { "VRAM usage".to_string() };
                let on = !hidden_gpu.iter().any(|h| h == &format!("vram{i}"));
                let label = wide(&name);
                AppendMenuW(menu, MF_STRING | checked(on), CMD_VRAM_SENSOR_BASE + i, label.as_ptr());
                labels.push(label);
            }
        }
    }
    // One checkable item per group icon (CPU/GPU/RAM/VRAM/SSD), toggling just the glyph drawn
    // next to that group's values on the taskbar.
    let icon_mask = settings::icon_mask();
    let icon_menu = CreatePopupMenu();
    let mut icon_labels = Vec::new();
    for (name, bit) in sysmonitor::ICON_NAMES {
        let label = wide(name);
        AppendMenuW(icon_menu, MF_STRING | checked(icon_mask & bit != 0), CMD_ICON_BASE + icon_index(bit), label.as_ptr());
        icon_labels.push(label);
    }
    let icons_label = wide("Icons");
    AppendMenuW(menu, MF_POPUP, icon_menu as usize, icons_label.as_ptr());
    labels.extend(icon_labels);
    labels.push(icons_label);
    AppendMenuW(menu, MF_SEPARATOR, 0, null());

    let interval = settings::refresh_interval_ms();
    let rate_menu = CreatePopupMenu();
    let (r1, r2, r5) = (wide("1 second"), wide("2 seconds"), wide("5 seconds (lowest power)"));
    AppendMenuW(rate_menu, MF_STRING | checked(interval == 1000), CMD_INTERVAL_1S, r1.as_ptr());
    AppendMenuW(rate_menu, MF_STRING | checked(interval == 2000), CMD_INTERVAL_2S, r2.as_ptr());
    AppendMenuW(rate_menu, MF_STRING | checked(interval == 5000), CMD_INTERVAL_5S, r5.as_ptr());
    let rate_label = wide("Refresh rate");
    AppendMenuW(menu, MF_POPUP, rate_menu as usize, rate_label.as_ptr());
    labels.extend([r1, r2, r5, rate_label]);

    // Which monitor's taskbar hosts the widget. Only screens with a taskbar are listed; secondary
    // ones need "Show my taskbar on all displays" in Windows Settings. Numbers match the Windows
    // display numbers (Settings > Display).
    let screen_menu = CreatePopupMenu();
    let mut screen_labels = Vec::new();
    let screens = super::screens();
    let current = settings::screen();
    let selected = if screens.iter().any(|(number, _)| *number == current) {
        current
    } else {
        screens.iter().find(|(_, primary)| *primary).map(|(number, _)| *number).unwrap_or(0)
    };
    for (number, _) in &screens {
        let label = wide(&format!("Screen {number}"));
        AppendMenuW(screen_menu, MF_STRING | checked(*number == selected), CMD_SCREEN_BASE + *number as usize, label.as_ptr());
        screen_labels.push(label);
    }
    if screens.is_empty() {
        let label = wide("No taskbar found");
        AppendMenuW(screen_menu, MF_STRING | MF_GRAYED, 0, label.as_ptr());
        screen_labels.push(label);
    }
    let screen_label = wide("Screen");
    AppendMenuW(menu, MF_POPUP, screen_menu as usize, screen_label.as_ptr());
    labels.extend(screen_labels);
    labels.push(screen_label);

    // Quick presets: set sensor_mask + icon_mask together in one click.
    let preset_menu = CreatePopupMenu();
    let mut preset_labels = Vec::new();
    for (i, (name, _, _, _)) in PRESETS.iter().enumerate() {
        let label = wide(name);
        AppendMenuW(preset_menu, MF_STRING, CMD_PRESET_BASE + i, label.as_ptr());
        preset_labels.push(label);
    }
    let preset_label = wide("Presets");
    AppendMenuW(menu, MF_POPUP, preset_menu as usize, preset_label.as_ptr());
    labels.extend(preset_labels);
    labels.push(preset_label);

    // Temperature alert thresholds: independent submenu per sensor (CPU/GPU), each offering
    // the same fixed \u00b0C choices, with the current threshold checked.
    let cpu_threshold = settings::cpu_alert_threshold();
    let cpu_t_menu = CreatePopupMenu();
    let mut cpu_t_labels = Vec::new();
    for (i, t) in THRESHOLD_CHOICES.iter().enumerate() {
        let label = wide(&format!("{t}\u{b0}C"));
        AppendMenuW(cpu_t_menu, MF_STRING | checked(*t == cpu_threshold), CMD_CPU_THRESHOLD_BASE + i, label.as_ptr());
        cpu_t_labels.push(label);
    }
    let cpu_t_label = wide("CPU alert threshold");
    AppendMenuW(menu, MF_POPUP, cpu_t_menu as usize, cpu_t_label.as_ptr());
    labels.extend(cpu_t_labels);
    labels.push(cpu_t_label);

    let gpu_threshold = settings::gpu_alert_threshold();
    let gpu_t_menu = CreatePopupMenu();
    let mut gpu_t_labels = Vec::new();
    for (i, t) in THRESHOLD_CHOICES.iter().enumerate() {
        let label = wide(&format!("{t}\u{b0}C"));
        AppendMenuW(gpu_t_menu, MF_STRING | checked(*t == gpu_threshold), CMD_GPU_THRESHOLD_BASE + i, label.as_ptr());
        gpu_t_labels.push(label);
    }
    let gpu_t_label = wide("GPU alert threshold");
    AppendMenuW(menu, MF_POPUP, gpu_t_menu as usize, gpu_t_label.as_ptr());
    labels.extend(gpu_t_labels);
    labels.push(gpu_t_label);
    AppendMenuW(menu, MF_SEPARATOR, 0, null());

    let (refresh, autostart, exit) = (wide("Refresh now"), wide("Start with Windows"), wide("Exit"));
    let alerts = wide("Low battery alerts");
    let copy_label = wide("Copy diagnostics");
    let probe_label = wide("Probe an unsupported device (60 s)");
    AppendMenuW(menu, MF_STRING, CMD_REFRESH, refresh.as_ptr());
    AppendMenuW(menu, MF_STRING | checked(settings::alerts_enabled()), CMD_ALERTS, alerts.as_ptr());
    AppendMenuW(menu, MF_STRING | checked(settings::autostart_enabled()), CMD_AUTOSTART, autostart.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, CMD_COPY_DIAGNOSTICS, copy_label.as_ptr());
    let probing = if PROBING.load(Ordering::Relaxed) { MF_GRAYED } else { 0 };
    AppendMenuW(menu, MF_STRING | probing, CMD_PROBE, probe_label.as_ptr());
    let history_label = wide("Show usage history graph");
    AppendMenuW(menu, MF_STRING, CMD_HISTORY, history_label.as_ptr());
    AppendMenuW(menu, MF_STRING, CMD_EXIT, exit.as_ptr());

    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN, pt.x, pt.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);

    match cmd as usize {
        0 => {}
        CMD_REFRESH => wake(),
        CMD_AUTOSTART => settings::set_autostart(!settings::autostart_enabled()),
        CMD_ALERTS => settings::set_alerts(!settings::alerts_enabled()),
        CMD_COPY_DIAGNOSTICS => copy_diagnostics(hwnd),
        CMD_PROBE => start_probe(),
        CMD_HISTORY => crate::history::toggle_open(),
        CMD_EXIT => {
            super::tray::set_tray_icon(false);
            let widget = with(|a| a.widget);
            if !widget.is_null() {
                DestroyWindow(widget);
            }
            DestroyWindow(hwnd);
        }
        CMD_INTERVAL_1S => settings::set_refresh_interval(1000),
        CMD_INTERVAL_2S => settings::set_refresh_interval(2000),
        CMD_INTERVAL_5S => settings::set_refresh_interval(5000),
        c if (CMD_SENSOR_BASE..CMD_SENSOR_BASE + sysmonitor::SENSOR_NAMES.len()).contains(&c) => {
            if let Some((_, bit)) = sysmonitor::SENSOR_NAMES.get(c - CMD_SENSOR_BASE) {
                settings::toggle_sensor(*bit);
                redraw();
            }
        }
        c if (CMD_GPU_SENSOR_BASE..CMD_GPU_SENSOR_BASE + 16).contains(&c) => {
            settings::toggle_gpu_sensor(&format!("gpu{}", c - CMD_GPU_SENSOR_BASE));
            redraw();
        }
        c if (CMD_VRAM_SENSOR_BASE..CMD_VRAM_SENSOR_BASE + 16).contains(&c) => {
            settings::toggle_gpu_sensor(&format!("vram{}", c - CMD_VRAM_SENSOR_BASE));
            redraw();
        }
        c if (CMD_GPU_TEMP_SENSOR_BASE..CMD_GPU_TEMP_SENSOR_BASE + 16).contains(&c) => {
            settings::toggle_gpu_sensor(&format!("gputemp{}", c - CMD_GPU_TEMP_SENSOR_BASE));
            redraw();
        }
        c if (CMD_ICON_BASE..CMD_ICON_BASE + sysmonitor::ICON_NAMES.len()).contains(&c) => {
            if let Some((_, bit)) = sysmonitor::ICON_NAMES.get(c - CMD_ICON_BASE) {
                settings::toggle_icon(*bit);
                redraw();
            }
        }
        c if (CMD_CPU_THRESHOLD_BASE..CMD_CPU_THRESHOLD_BASE + THRESHOLD_CHOICES.len()).contains(&c) => {
            settings::set_cpu_alert_threshold(THRESHOLD_CHOICES[c - CMD_CPU_THRESHOLD_BASE]);
        }
        c if (CMD_GPU_THRESHOLD_BASE..CMD_GPU_THRESHOLD_BASE + THRESHOLD_CHOICES.len()).contains(&c) => {
            settings::set_gpu_alert_threshold(THRESHOLD_CHOICES[c - CMD_GPU_THRESHOLD_BASE]);
        }
        c if (CMD_PRESET_BASE..CMD_PRESET_BASE + PRESETS.len()).contains(&c) => {
            let (_, sensor_mask, icon_mask, gpu_preset) = PRESETS[c - CMD_PRESET_BASE];
            settings::set_sensor_mask(sensor_mask);
            settings::set_icon_mask(icon_mask);
            let gpu_count = sys_snapshot().map(|s| s.gpus.len()).unwrap_or(0);
            settings::set_hidden_gpu_sensors(gpu_preset.hidden_list(gpu_count));
            redraw();
        }
        c if (CMD_SCREEN_BASE..CMD_SCREEN_BASE + 32).contains(&c) => {
            super::select_screen((c - CMD_SCREEN_BASE) as u32);
        }
        c if c >= CMD_DEVICE_BASE => {
            if let Some(r) = devices.get(c - CMD_DEVICE_BASE) {
                settings::toggle_hidden(&r.id);
                redraw();
            }
        }
        _ => {}
    }
}

/// Position of `bit` in `sysmonitor::ICON_NAMES`, for the menu command id.
fn icon_index(bit: u32) -> usize {
    sysmonitor::ICON_NAMES.iter().position(|(_, b)| *b == bit).unwrap_or(0)
}

static PROBING: AtomicBool = AtomicBool::new(false);

/// Runs the device probe in the background, then opens its file in Notepad.
fn start_probe() {
    if PROBING.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(|| {
        crate::init_com();
        let _ = notify::show(
            "Probing devices for 60 seconds",
            "Use the device as usual but don't type. A text file opens when it's done: paste it into a device report on GitHub.",
        );
        match probe::run_to_file(probe::LISTEN) {
            Ok(path) => {
                let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
            }
            Err(e) => {
                diag::event("probe", format!("failed: {e}"));
                let _ = notify::show("Probe failed", &e);
            }
        }
        PROBING.store(false, Ordering::Relaxed);
    });
}

/// Puts the diagnostics log on the clipboard and confirms with a toast.
unsafe fn copy_diagnostics(hwnd: HWND) {
    let (title, body) = match diag::report() {
        Some(text) if set_clipboard(hwnd, &text) => ("Diagnostics copied", "Paste them into your device report on GitHub."),
        Some(_) => ("Couldn't copy diagnostics", "Another app is using the clipboard. Try again."),
        None => ("No diagnostics yet", "Nothing has been logged so far."),
    };
    // Toasts are WinRT, and COM is only initialized on the worker thread.
    std::thread::spawn(move || {
        crate::init_com();
        let _ = notify::show(title, body);
    });
}

unsafe fn set_clipboard(hwnd: HWND, text: &str) -> bool {
    const CF_UNICODETEXT: u32 = 13;
    let units: Vec<u16> = text.replace('\n', "\r\n").encode_utf16().chain(std::iter::once(0)).collect();
    if OpenClipboard(hwnd) == 0 {
        return false;
    }
    let mut ok = false;
    let mem = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2);
    if !mem.is_null() {
        let dst = GlobalLock(mem) as *mut u16;
        if !dst.is_null() {
            std::ptr::copy_nonoverlapping(units.as_ptr(), dst, units.len());
            GlobalUnlock(mem);
            EmptyClipboard();
            ok = !SetClipboardData(CF_UNICODETEXT, mem).is_null();
        }
        if !ok {
            GlobalFree(mem); // on success the clipboard owns it
        }
    }
    CloseClipboard();
    ok
}
