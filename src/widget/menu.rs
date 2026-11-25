//! Right-click context menu: construction, command dispatch and the actions it triggers
//! (diagnostics copy, device probing).

use super::{readings, redraw, sys_snapshot, wake};
use crate::audio::model::AppAction;
use crate::{diag, notify, probe, settings, sysmonitor, update, wide};
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
const CMD_PAWNIO: usize = 8;
const CMD_UPDATE: usize = 9;
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
/// Manual placement: shift the widget 16 px away from the notification area, back toward it,
/// or reset.
const CMD_OFFSET_LEFT: usize = 950;
const CMD_OFFSET_RIGHT: usize = 951;
const CMD_OFFSET_RESET: usize = 952;

/// \u00b0C choices offered in the CPU/GPU temperature-alert submenus.
const THRESHOLD_CHOICES: [u32; 7] = [60, 70, 75, 80, 85, 90, 95];

/// What a preset does with each detected GPU's usage/temperature/VRAM checkboxes.
#[derive(Clone, Copy)]
pub(super) enum GpuPreset {
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
pub(super) const PRESETS: [(&str, u32, u32, GpuPreset); 3] = [
    ("Full (usage + temperatures)", sysmonitor::BIT_CPU | sysmonitor::BIT_CPU_TEMP | sysmonitor::BIT_RAM, sysmonitor::ALL_ICONS, GpuPreset::UsageAndTemp),
    ("Usage only", sysmonitor::BIT_CPU | sysmonitor::BIT_RAM, sysmonitor::ALL_ICONS, GpuPreset::UsageOnly),
    ("Temperatures only", sysmonitor::BIT_CPU_TEMP, sysmonitor::ALL_ICONS, GpuPreset::TempOnly),
];

/// Applies a quick preset to every sensor/icon/GPU toggle at once. Shared with the settings
/// window, so both UIs do exactly the same thing.
pub(super) fn apply_preset(index: usize) {
    let Some((_, sensor_mask, icon_mask, gpu_preset)) = PRESETS.get(index).copied() else {
        return;
    };
    settings::set_sensor_mask(sensor_mask);
    settings::set_icon_mask(icon_mask);
    let gpu_count = sys_snapshot().map(|s| s.gpus.len()).unwrap_or(0);
    settings::set_hidden_gpu_sensors(gpu_preset.hidden_list(gpu_count));
}

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
    let sensor_menu = CreatePopupMenu();
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
        AppendMenuW(sensor_menu, MF_STRING | checked(mask & bit != 0), CMD_SENSOR_BASE + idx, label.as_ptr());
        labels.push(label);
        // Insert the per-adapter GPU items right after "CPU temperature", matching the order
        // CPU usage / CPU temperature / GPU usage / GPU temperature / RAM usage / VRAM usage /
        // SSD temperature used on the taskbar.
        if *bit == sysmonitor::BIT_CPU_TEMP {
            for i in 0..gpu_count {
                let name = if multi_gpu { format!("GPU{} usage", i + 1) } else { "GPU usage".to_string() };
                let on = !hidden_gpu.iter().any(|h| h == &format!("gpu{i}"));
                let label = wide(&name);
                AppendMenuW(sensor_menu, MF_STRING | checked(on), CMD_GPU_SENSOR_BASE + i, label.as_ptr());
                labels.push(label);
            }
            for i in 0..gpu_count {
                let has_gpu_temp = snap.as_ref().is_some_and(|s| s.gpus.get(i).is_some_and(|g| g.temp.is_some()));
                let on = !hidden_gpu.iter().any(|h| h == &format!("gputemp{i}"));
                let base = if multi_gpu { format!("GPU{} temperature", i + 1) } else { "GPU temperature".to_string() };
                let suffix = if on && !has_gpu_temp { "  (no sensor found)" } else { "" };
                let label = wide(&format!("{base}{suffix}"));
                AppendMenuW(sensor_menu, MF_STRING | checked(on), CMD_GPU_TEMP_SENSOR_BASE + i, label.as_ptr());
                labels.push(label);
            }
        }
        if *bit == sysmonitor::BIT_RAM {
            for i in 0..gpu_count {
                let name = if multi_gpu { format!("VRAM{} usage", i + 1) } else { "VRAM usage".to_string() };
                let on = !hidden_gpu.iter().any(|h| h == &format!("vram{i}"));
                let label = wide(&name);
                AppendMenuW(sensor_menu, MF_STRING | checked(on), CMD_VRAM_SENSOR_BASE + i, label.as_ptr());
                labels.push(label);
            }
        }
    }
    let sensors_label = wide("Sensors");
    AppendMenuW(menu, MF_POPUP, sensor_menu as usize, sensors_label.as_ptr());
    labels.push(sensors_label);

    // Display: what the widget draws and where it sits.
    let display_menu = CreatePopupMenu();
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
    AppendMenuW(display_menu, MF_POPUP, icon_menu as usize, icons_label.as_ptr());
    labels.extend(icon_labels);
    labels.push(icons_label);
    AppendMenuW(display_menu, MF_SEPARATOR, 0, null());

    let interval = settings::refresh_interval_ms();
    let rate_menu = CreatePopupMenu();
    let (r1, r2, r5) = (wide("1 second"), wide("2 seconds"), wide("5 seconds (lowest power)"));
    AppendMenuW(rate_menu, MF_STRING | checked(interval == 1000), CMD_INTERVAL_1S, r1.as_ptr());
    AppendMenuW(rate_menu, MF_STRING | checked(interval == 2000), CMD_INTERVAL_2S, r2.as_ptr());
    AppendMenuW(rate_menu, MF_STRING | checked(interval == 5000), CMD_INTERVAL_5S, r5.as_ptr());
    let rate_label = wide("Refresh rate");
    AppendMenuW(display_menu, MF_POPUP, rate_menu as usize, rate_label.as_ptr());
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
    AppendMenuW(display_menu, MF_POPUP, screen_menu as usize, screen_label.as_ptr());
    labels.extend(screen_labels);
    labels.push(screen_label);

    // Manual nudge for when the automatic spot (left of the clock) covers something.
    let offset = settings::widget_offset();
    let position_menu = CreatePopupMenu();
    let offset_label = wide(&format!("Offset: {offset} px"));
    let left_label = wide("Move left (+16 px)");
    let right_label = wide("Move right (-16 px)");
    let reset_label = wide("Reset offset");
    AppendMenuW(position_menu, MF_STRING | MF_GRAYED, 0, offset_label.as_ptr());
    AppendMenuW(position_menu, MF_STRING, CMD_OFFSET_LEFT, left_label.as_ptr());
    AppendMenuW(position_menu, MF_STRING, CMD_OFFSET_RIGHT, right_label.as_ptr());
    AppendMenuW(position_menu, MF_STRING, CMD_OFFSET_RESET, reset_label.as_ptr());
    let position_label = wide("Position");
    AppendMenuW(display_menu, MF_POPUP, position_menu as usize, position_label.as_ptr());
    labels.extend([offset_label, left_label, right_label, reset_label, position_label]);
    let display_label = wide("Display");
    AppendMenuW(menu, MF_POPUP, display_menu as usize, display_label.as_ptr());
    labels.push(display_label);

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

    // Alerts: the low-battery toggle plus one threshold submenu per sensor (CPU/GPU), each
    // offering the same fixed \u00b0C choices with the current threshold checked.
    let alert_menu = CreatePopupMenu();
    let low_battery_label = wide("Low battery alerts");
    AppendMenuW(alert_menu, MF_STRING | checked(settings::alerts_enabled()), CMD_ALERTS, low_battery_label.as_ptr());
    labels.push(low_battery_label);
    AppendMenuW(alert_menu, MF_SEPARATOR, 0, null());
    let cpu_threshold = settings::cpu_alert_threshold();
    let cpu_t_menu = CreatePopupMenu();
    let mut cpu_t_labels = Vec::new();
    for (i, t) in THRESHOLD_CHOICES.iter().enumerate() {
        let label = wide(&format!("{t}\u{b0}C"));
        AppendMenuW(cpu_t_menu, MF_STRING | checked(*t == cpu_threshold), CMD_CPU_THRESHOLD_BASE + i, label.as_ptr());
        cpu_t_labels.push(label);
    }
    let cpu_t_label = wide("CPU alert threshold");
    AppendMenuW(alert_menu, MF_POPUP, cpu_t_menu as usize, cpu_t_label.as_ptr());
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
    AppendMenuW(alert_menu, MF_POPUP, gpu_t_menu as usize, gpu_t_label.as_ptr());
    labels.extend(gpu_t_labels);
    labels.push(gpu_t_label);
    let alerts_label = wide("Alerts");
    AppendMenuW(menu, MF_POPUP, alert_menu as usize, alerts_label.as_ptr());
    labels.push(alerts_label);

    // Audio: device lock, mic mute, enhancements, profiles, listen-to-this-device.
    let audio_targets = build_audio_menu(menu, &mut labels, checked);
    AppendMenuW(menu, MF_SEPARATOR, 0, null());

    let (refresh, autostart, exit) = (wide("Refresh now"), wide("Start with Windows"), wide("Exit"));
    AppendMenuW(menu, MF_STRING, CMD_REFRESH, refresh.as_ptr());
    let history_label = wide("Show usage history graph");
    AppendMenuW(menu, MF_STRING, CMD_HISTORY, history_label.as_ptr());
    AppendMenuW(menu, MF_STRING | checked(settings::autostart_enabled()), CMD_AUTOSTART, autostart.as_ptr());
    let update_label = match update::available() {
        Some(version) => wide(&format!("Update to v{version}")),
        None => wide("Check for updates"),
    };
    AppendMenuW(menu, MF_STRING, CMD_UPDATE, update_label.as_ptr());
    labels.push(update_label);

    // Rarely used, so it stays out of the top level and keeps the menu short.
    let diag_menu = CreatePopupMenu();
    let copy_label = wide("Copy diagnostics");
    let probe_label = wide("Probe an unsupported device (60 s)");
    AppendMenuW(diag_menu, MF_STRING, CMD_COPY_DIAGNOSTICS, copy_label.as_ptr());
    let probing = if PROBING.load(Ordering::Relaxed) { MF_GRAYED } else { 0 };
    AppendMenuW(diag_menu, MF_STRING | probing, CMD_PROBE, probe_label.as_ptr());
    // Only useful while CPU temperature has no source at all; hidden once a reading works.
    if sys_snapshot().is_some_and(|s| s.cpu_temp.is_none()) && settings::sensor_mask() & sysmonitor::BIT_CPU_TEMP != 0 {
        let pawnio_label = wide("Install CPU temperature driver");
        AppendMenuW(diag_menu, MF_STRING, CMD_PAWNIO, pawnio_label.as_ptr());
        labels.push(pawnio_label);
    }
    let diag_label = wide("Diagnostics");
    AppendMenuW(menu, MF_POPUP, diag_menu as usize, diag_label.as_ptr());
    labels.push(diag_label);
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
        CMD_PAWNIO => super::install_pawnio_driver(hwnd as usize),
        CMD_HISTORY => crate::history::toggle_open(),
        CMD_UPDATE => super::update_action(hwnd as usize),
        CMD_EXIT => super::shutdown(hwnd),
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
            apply_preset(c - CMD_PRESET_BASE);
        }
        c if (CMD_SCREEN_BASE..CMD_SCREEN_BASE + 32).contains(&c) => {
            super::select_screen((c - CMD_SCREEN_BASE) as u32);
        }
        CMD_OFFSET_LEFT => {
            settings::set_widget_offset(settings::widget_offset() + 16);
            super::place();
        }
        CMD_OFFSET_RIGHT => {
            settings::set_widget_offset(settings::widget_offset() - 16);
            super::place();
        }
        CMD_OFFSET_RESET => {
            settings::set_widget_offset(0);
            super::place();
        }
        CMD_AUDIO_FLYOUT => super::open_flyout(),
        CMD_AUDIO_MIC => {
            if let Some(engine) = crate::audio::engine() {
                engine.toggle_active_mics_mute();
            }
            redraw();
        }
        CMD_AUDIO_LOCK => apply_audio(AppAction::SetLockEnabled(!audio_state().lock_enabled)),
        CMD_AUDIO_SEPARATE => apply_audio(AppAction::SetSeparateCommsRole(!audio_state().separate_comms_role)),
        CMD_AUDIO_ENH_OFF => {
            for device in audio_state().devices {
                if device.flow == crate::audio::model::DeviceFlow::Output && device.enhancements.any_enabled() {
                    apply_audio(AppAction::SetEnhancementsDisabled { device_id: device.id, disabled: true });
                }
            }
        }
        CMD_AUDIO_ENH_ON => apply_audio(AppAction::ReenableAllEnhancements),
        CMD_AUDIO_ENH_AUTO => apply_audio(AppAction::SetAutoDisableEnhancements(!audio_state().auto_disable_enhancements)),
        CMD_AUDIO_UNUSED_AUTO => apply_audio(AppAction::SetAutoDisableUnusedDevices(!audio_state().auto_disable_unused_devices)),
        CMD_AUDIO_UNUSED_ON => apply_audio(AppAction::ReenableAllDevices),
        CMD_AUDIO_SAVE_PROFILE => {
            let name = format!("Profile {}", audio_state().profiles.len() + 1);
            apply_audio(AppAction::SaveProfile { name });
        }
        c if (CMD_AUDIO_OUT_BASE..CMD_AUDIO_OUT_BASE + AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some(device_id) = audio_targets.outputs.get(c - CMD_AUDIO_OUT_BASE) {
                apply_audio(AppAction::SetDefault { flow: crate::audio::model::DeviceFlow::Output, device_id: device_id.clone() });
            }
        }
        c if (CMD_AUDIO_IN_BASE..CMD_AUDIO_IN_BASE + AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some(device_id) = audio_targets.inputs.get(c - CMD_AUDIO_IN_BASE) {
                apply_audio(AppAction::SetDefault { flow: crate::audio::model::DeviceFlow::Input, device_id: device_id.clone() });
            }
        }
        c if (CMD_AUDIO_OUT_MUTE_BASE..CMD_AUDIO_OUT_MUTE_BASE + AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some(device_id) = audio_targets.outputs.get(c - CMD_AUDIO_OUT_MUTE_BASE) {
                apply_audio(AppAction::ToggleMute { device_id: device_id.clone() });
            }
        }
        c if (CMD_AUDIO_IN_MUTE_BASE..CMD_AUDIO_IN_MUTE_BASE + AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some(device_id) = audio_targets.inputs.get(c - CMD_AUDIO_IN_MUTE_BASE) {
                apply_audio(AppAction::ToggleMute { device_id: device_id.clone() });
            }
        }
        c if (CMD_AUDIO_VOL_UP_BASE..CMD_AUDIO_VOL_UP_BASE + AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some(device_id) = audio_targets.outputs.get(c - CMD_AUDIO_VOL_UP_BASE) {
                adjust_audio_volume(device_id, 10);
            }
        }
        c if (CMD_AUDIO_VOL_DN_BASE..CMD_AUDIO_VOL_DN_BASE + AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some(device_id) = audio_targets.outputs.get(c - CMD_AUDIO_VOL_DN_BASE) {
                adjust_audio_volume(device_id, -10);
            }
        }
        c if (CMD_AUDIO_IN_VOL_UP_BASE..CMD_AUDIO_IN_VOL_UP_BASE + AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some(device_id) = audio_targets.inputs.get(c - CMD_AUDIO_IN_VOL_UP_BASE) {
                adjust_audio_volume(device_id, 10);
            }
        }
        c if (CMD_AUDIO_IN_VOL_DN_BASE..CMD_AUDIO_IN_VOL_DN_BASE + AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some(device_id) = audio_targets.inputs.get(c - CMD_AUDIO_IN_VOL_DN_BASE) {
                adjust_audio_volume(device_id, -10);
            }
        }
        c if (CMD_AUDIO_PIN_BASE..CMD_AUDIO_PIN_BASE + AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some(device_id) = audio_targets.pins.get(c - CMD_AUDIO_PIN_BASE) {
                apply_audio(AppAction::ToggleDevicePin { device_id: device_id.clone() });
            }
        }
        c if (CMD_AUDIO_PROFILE_BASE..CMD_AUDIO_PROFILE_BASE + AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some(profile_id) = audio_targets.profiles.get(c - CMD_AUDIO_PROFILE_BASE) {
                apply_audio(AppAction::ApplyProfile { profile_id: profile_id.clone() });
            }
        }
        c if (CMD_AUDIO_PROFILE_DEL_BASE..CMD_AUDIO_PROFILE_DEL_BASE + AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some(profile_id) = audio_targets.profiles.get(c - CMD_AUDIO_PROFILE_DEL_BASE) {
                apply_audio(AppAction::DeleteProfile { profile_id: profile_id.clone() });
            }
        }
        c if (CMD_AUDIO_LISTEN_BASE..CMD_AUDIO_LISTEN_BASE + 4 * AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some((device_id, target)) = audio_targets.listen.get(c - CMD_AUDIO_LISTEN_BASE) {
                apply_audio(AppAction::SetListenEnabled { device_id: device_id.clone(), enabled: target.is_some() });
                apply_audio(AppAction::SetListenTarget { device_id: device_id.clone(), target_device_id: target.clone() });
            }
        }
        c if (CMD_AUDIO_ENH_DEVICE_BASE..CMD_AUDIO_ENH_DEVICE_BASE + AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some(device_id) = audio_targets.enhancements.get(c - CMD_AUDIO_ENH_DEVICE_BASE) {
                let disabled = audio_state().device(device_id).is_none_or(|d| d.enhancements.any_enabled());
                apply_audio(AppAction::SetEnhancementsDisabled { device_id: device_id.clone(), disabled });
            }
        }
        c if (CMD_AUDIO_HIDDEN_BASE..CMD_AUDIO_HIDDEN_BASE + AUDIO_LIST_LIMIT).contains(&c) => {
            if let Some(device_id) = audio_targets.hidden.get(c - CMD_AUDIO_HIDDEN_BASE) {
                let disabled = audio_state().device(device_id).is_some_and(|d| d.disabled);
                apply_audio(AppAction::SetDeviceDisabled { device_id: device_id.clone(), disabled: !disabled });
            }
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

// Audio commands live above every other range; the `c >= CMD_DEVICE_BASE` arm below must stay
// last because it is a catch-all.
const CMD_AUDIO_MIC: usize = 1000;
const CMD_AUDIO_LOCK: usize = 1001;
const CMD_AUDIO_SEPARATE: usize = 1002;
const CMD_AUDIO_ENH_OFF: usize = 1003;
const CMD_AUDIO_ENH_ON: usize = 1004;
const CMD_AUDIO_ENH_AUTO: usize = 1005;
const CMD_AUDIO_UNUSED_AUTO: usize = 1006;
const CMD_AUDIO_UNUSED_ON: usize = 1007;
const CMD_AUDIO_SAVE_PROFILE: usize = 1008;
const CMD_AUDIO_FLYOUT: usize = 1009;
const CMD_AUDIO_OUT_BASE: usize = 1100;
const CMD_AUDIO_IN_BASE: usize = 1140;
const CMD_AUDIO_OUT_MUTE_BASE: usize = 1180;
const CMD_AUDIO_IN_MUTE_BASE: usize = 1220;
const CMD_AUDIO_PIN_BASE: usize = 1260;
const CMD_AUDIO_PROFILE_BASE: usize = 1320;
const CMD_AUDIO_PROFILE_DEL_BASE: usize = 1360;
const CMD_AUDIO_LISTEN_BASE: usize = 1400;
const CMD_AUDIO_VOL_UP_BASE: usize = 1700;
const CMD_AUDIO_VOL_DN_BASE: usize = 1740;
const CMD_AUDIO_IN_VOL_UP_BASE: usize = 1780;
const CMD_AUDIO_IN_VOL_DN_BASE: usize = 1820;
const CMD_AUDIO_ENH_DEVICE_BASE: usize = 1860;
const CMD_AUDIO_HIDDEN_BASE: usize = 1900;
/// How many entries each of the ranges above can hold.
const AUDIO_LIST_LIMIT: usize = 40;

/// Device/profile ids behind the command ids built by [`build_audio_menu`], indexed by
/// `command - base`.
#[derive(Default)]
struct AudioTargets {
    outputs: Vec<String>,
    inputs: Vec<String>,
    profiles: Vec<String>,
    listen: Vec<(String, Option<String>)>,
    pins: Vec<String>,
    enhancements: Vec<String>,
    hidden: Vec<String>,
}

unsafe fn apply_audio(action: crate::audio::model::AppAction) {
    if let Some(engine) = crate::audio::engine() {
        engine.apply_action(action);
    }
    redraw();
}

fn audio_state() -> crate::audio::model::AppState {
    crate::audio::engine().map(|e| e.snapshot()).unwrap_or_default()
}

unsafe fn adjust_audio_volume(device_id: &str, delta: i16) {
    let current = audio_state().device(device_id).map(|d| d.volume).unwrap_or(0) as i16;
    let volume = (current + delta).clamp(0, 100) as u8;
    apply_audio(AppAction::SetVolume { device_id: device_id.to_string(), volume });
}

/// Appends the "Audio" submenu and returns the id lists its command ids refer to.
unsafe fn build_audio_menu(menu: HMENU, labels: &mut Vec<Vec<u16>>, checked: impl Fn(bool) -> u32) -> AudioTargets {
    use crate::audio::model::DeviceFlow;

    let mut t = AudioTargets::default();
    let Some(engine) = crate::audio::engine() else { return t };
    let state = engine.snapshot();
    let audio = CreatePopupMenu();

    let panel_label = wide("Open audio panel");
    AppendMenuW(audio, MF_STRING, CMD_AUDIO_FLYOUT, panel_label.as_ptr());
    labels.push(panel_label);
    AppendMenuW(audio, MF_SEPARATOR, 0, null());

    let active_mics: Vec<_> = state
        .devices
        .iter()
        .filter(|d| {
            d.flow == DeviceFlow::Input
                && (d.id == state.roles.default_input_id
                    || (state.separate_comms_role && d.id == state.roles.comms_input_id))
        })
        .collect();
    let any_unmuted = active_mics.iter().any(|d| !d.muted);
    let mic_label = wide(if any_unmuted { "Mute microphone" } else { "Unmute microphone" });
    let mic_flags = if active_mics.is_empty() { MF_GRAYED } else { 0 };
    AppendMenuW(audio, MF_STRING | mic_flags, CMD_AUDIO_MIC, mic_label.as_ptr());
    labels.push(mic_label);

    let lock_label = wide("Lock default devices");
    AppendMenuW(audio, MF_STRING | checked(state.lock_enabled), CMD_AUDIO_LOCK, lock_label.as_ptr());
    labels.push(lock_label);
    let separate_label = wide("Separate communications device");
    AppendMenuW(audio, MF_STRING | checked(state.separate_comms_role), CMD_AUDIO_SEPARATE, separate_label.as_ptr());
    labels.push(separate_label);
    AppendMenuW(audio, MF_SEPARATOR, 0, null());

    let outputs: Vec<_> = state.devices.iter().filter(|d| d.flow == DeviceFlow::Output).collect();
    let inputs: Vec<_> = state.devices.iter().filter(|d| d.flow == DeviceFlow::Input).collect();

    let out_menu = CreatePopupMenu();
    for (i, d) in outputs.iter().enumerate().take(AUDIO_LIST_LIMIT) {
        let device_menu = CreatePopupMenu();
        let default_label = wide("Set as default");
        AppendMenuW(device_menu, MF_STRING | checked(d.id == state.roles.default_output_id), CMD_AUDIO_OUT_BASE + i, default_label.as_ptr());
        labels.push(default_label);
        let level_label = wide(&format!("Volume: {}%", d.volume));
        AppendMenuW(device_menu, MF_STRING | MF_GRAYED, 0, level_label.as_ptr());
        labels.push(level_label);
        let up = wide("Volume up 10%");
        let down = wide("Volume down 10%");
        AppendMenuW(device_menu, MF_STRING, CMD_AUDIO_VOL_UP_BASE + i, up.as_ptr());
        AppendMenuW(device_menu, MF_STRING, CMD_AUDIO_VOL_DN_BASE + i, down.as_ptr());
        labels.extend([up, down]);
        let mute_label = wide("Mute");
        AppendMenuW(device_menu, MF_STRING | checked(d.muted), CMD_AUDIO_OUT_MUTE_BASE + i, mute_label.as_ptr());
        labels.push(mute_label);
        let muted = if d.muted { ", muted" } else { "" };
        let label = wide(&format!("{}  ({}%{})", d.name, d.volume, muted));
        AppendMenuW(out_menu, MF_POPUP, device_menu as usize, label.as_ptr());
        labels.push(label);
        t.outputs.push(d.id.clone());
    }
    if outputs.is_empty() {
        let label = wide("No output devices");
        AppendMenuW(out_menu, MF_STRING | MF_GRAYED, 0, label.as_ptr());
        labels.push(label);
    }
    let out_label = wide("Output devices");
    AppendMenuW(audio, MF_POPUP, out_menu as usize, out_label.as_ptr());
    labels.push(out_label);

    let in_menu = CreatePopupMenu();
    for (i, d) in inputs.iter().enumerate().take(AUDIO_LIST_LIMIT) {
        let device_menu = CreatePopupMenu();
        let default_label = wide("Set as default");
        AppendMenuW(device_menu, MF_STRING | checked(d.id == state.roles.default_input_id), CMD_AUDIO_IN_BASE + i, default_label.as_ptr());
        labels.push(default_label);
        let level_label = wide(&format!("Volume: {}%", d.volume));
        AppendMenuW(device_menu, MF_STRING | MF_GRAYED, 0, level_label.as_ptr());
        labels.push(level_label);
        let up = wide("Volume up 10%");
        let down = wide("Volume down 10%");
        AppendMenuW(device_menu, MF_STRING, CMD_AUDIO_IN_VOL_UP_BASE + i, up.as_ptr());
        AppendMenuW(device_menu, MF_STRING, CMD_AUDIO_IN_VOL_DN_BASE + i, down.as_ptr());
        labels.extend([up, down]);
        let mute_label = wide("Mute");
        AppendMenuW(device_menu, MF_STRING | checked(d.muted), CMD_AUDIO_IN_MUTE_BASE + i, mute_label.as_ptr());
        labels.push(mute_label);
        let muted = if d.muted { ", muted" } else { "" };
        let label = wide(&format!("{}  ({}%{})", d.name, d.volume, muted));
        AppendMenuW(in_menu, MF_POPUP, device_menu as usize, label.as_ptr());
        labels.push(label);
        t.inputs.push(d.id.clone());
    }
    if inputs.is_empty() {
        let label = wide("No input devices");
        AppendMenuW(in_menu, MF_STRING | MF_GRAYED, 0, label.as_ptr());
        labels.push(label);
    }
    let in_label = wide("Input devices");
    AppendMenuW(audio, MF_POPUP, in_menu as usize, in_label.as_ptr());
    labels.push(in_label);

    // Pins keep a device enabled while "disable unused devices" is on.
    let pin_menu = CreatePopupMenu();
    for (i, d) in state.devices.iter().enumerate().take(AUDIO_LIST_LIMIT) {
        let label = wide(&d.name);
        AppendMenuW(pin_menu, MF_STRING | checked(d.pinned), CMD_AUDIO_PIN_BASE + i, label.as_ptr());
        labels.push(label);
        t.pins.push(d.id.clone());
    }
    let pin_label = wide("Pinned devices");
    AppendMenuW(audio, MF_POPUP, pin_menu as usize, pin_label.as_ptr());
    labels.push(pin_label);

    // Enhancements: all-off/all-on plus a per-output-device checkbox (checked = disabled).
    let enh_menu = CreatePopupMenu();
    let off_label = wide("Disable all");
    let on_label = wide("Re-enable all");
    let auto_enh_label = wide("Auto-disable for new devices");
    AppendMenuW(enh_menu, MF_STRING, CMD_AUDIO_ENH_OFF, off_label.as_ptr());
    AppendMenuW(enh_menu, MF_STRING, CMD_AUDIO_ENH_ON, on_label.as_ptr());
    AppendMenuW(enh_menu, MF_STRING | checked(state.auto_disable_enhancements), CMD_AUDIO_ENH_AUTO, auto_enh_label.as_ptr());
    labels.extend([off_label, on_label, auto_enh_label]);
    let per_device = CreatePopupMenu();
    for (i, d) in outputs.iter().enumerate().take(AUDIO_LIST_LIMIT) {
        let label = wide(&d.name);
        AppendMenuW(per_device, MF_STRING | checked(!d.enhancements.any_enabled()), CMD_AUDIO_ENH_DEVICE_BASE + i, label.as_ptr());
        labels.push(label);
        t.enhancements.push(d.id.clone());
    }
    let per_device_label = wide("Per device");
    AppendMenuW(enh_menu, MF_POPUP, per_device as usize, per_device_label.as_ptr());
    labels.push(per_device_label);
    let enh_label = wide("Audio enhancements");
    AppendMenuW(audio, MF_POPUP, enh_menu as usize, enh_label.as_ptr());
    labels.push(enh_label);

    // Hide unused devices: auto toggle, bulk re-enable, and a per-device checkbox.
    let unused_menu = CreatePopupMenu();
    let auto_unused_label = wide("Auto-disable unused devices");
    let reenable_label = wide("Re-enable all devices");
    AppendMenuW(unused_menu, MF_STRING | checked(state.auto_disable_unused_devices), CMD_AUDIO_UNUSED_AUTO, auto_unused_label.as_ptr());
    AppendMenuW(unused_menu, MF_STRING, CMD_AUDIO_UNUSED_ON, reenable_label.as_ptr());
    labels.extend([auto_unused_label, reenable_label]);
    for (i, d) in state.devices.iter().enumerate().take(AUDIO_LIST_LIMIT) {
        let label = wide(&d.name);
        AppendMenuW(unused_menu, MF_STRING | checked(d.disabled), CMD_AUDIO_HIDDEN_BASE + i, label.as_ptr());
        labels.push(label);
        t.hidden.push(d.id.clone());
    }
    let unused_label = wide("Unused devices");
    AppendMenuW(audio, MF_POPUP, unused_menu as usize, unused_label.as_ptr());
    labels.push(unused_label);

    // Listen to this device: one submenu per input, None / default output / every output.
    let listen_menu = CreatePopupMenu();
    for d in &inputs {
        let current = d.listen.as_ref().filter(|l| l.enabled).and_then(|l| l.target_device_id.clone());
        let device_menu = CreatePopupMenu();
        let none = wide("None");
        AppendMenuW(device_menu, MF_STRING | checked(current.is_none()), CMD_AUDIO_LISTEN_BASE + t.listen.len(), none.as_ptr());
        labels.push(none);
        t.listen.push((d.id.clone(), None));
        let default_out = wide("Default output");
        AppendMenuW(device_menu, MF_STRING | checked(current.as_deref() == Some(crate::audio::model::LISTEN_TARGET_DEFAULT_OUTPUT)), CMD_AUDIO_LISTEN_BASE + t.listen.len(), default_out.as_ptr());
        labels.push(default_out);
        t.listen.push((d.id.clone(), Some(crate::audio::model::LISTEN_TARGET_DEFAULT_OUTPUT.to_string())));
        for out in &outputs {
            let label = wide(&out.name);
            AppendMenuW(device_menu, MF_STRING | checked(current.as_deref() == Some(out.id.as_str())), CMD_AUDIO_LISTEN_BASE + t.listen.len(), label.as_ptr());
            labels.push(label);
            t.listen.push((d.id.clone(), Some(out.id.clone())));
        }
        let label = wide(&d.name);
        AppendMenuW(listen_menu, MF_POPUP, device_menu as usize, label.as_ptr());
        labels.push(label);
    }
    let listen_label = wide("Listen to this device");
    AppendMenuW(audio, MF_POPUP, listen_menu as usize, listen_label.as_ptr());
    labels.push(listen_label);

    // Profiles: apply, save the current setup, delete.
    let profile_menu = CreatePopupMenu();
    for (i, p) in state.profiles.iter().enumerate().take(AUDIO_LIST_LIMIT) {
        let label = wide(&p.name);
        AppendMenuW(profile_menu, MF_STRING | checked(state.active_profile_id.as_deref() == Some(p.id.as_str())), CMD_AUDIO_PROFILE_BASE + i, label.as_ptr());
        labels.push(label);
        t.profiles.push(p.id.clone());
    }
    if !state.profiles.is_empty() {
        AppendMenuW(profile_menu, MF_SEPARATOR, 0, null());
    }
    let save_label = wide("Save current setup");
    AppendMenuW(profile_menu, MF_STRING, CMD_AUDIO_SAVE_PROFILE, save_label.as_ptr());
    labels.push(save_label);
    if !state.profiles.is_empty() {
        let delete_menu = CreatePopupMenu();
        for (i, p) in state.profiles.iter().enumerate().take(AUDIO_LIST_LIMIT) {
            let label = wide(&p.name);
            AppendMenuW(delete_menu, MF_STRING, CMD_AUDIO_PROFILE_DEL_BASE + i, label.as_ptr());
            labels.push(label);
        }
        let delete_label = wide("Delete");
        AppendMenuW(profile_menu, MF_POPUP, delete_menu as usize, delete_label.as_ptr());
        labels.push(delete_label);
    }
    let profile_label = wide("Profiles");
    AppendMenuW(audio, MF_POPUP, profile_menu as usize, profile_label.as_ptr());
    labels.push(profile_label);

    let audio_label = wide("Audio");
    AppendMenuW(menu, MF_POPUP, audio as usize, audio_label.as_ptr());
    labels.push(audio_label);
    t
}

/// Position of `bit` in `sysmonitor::ICON_NAMES`, for the menu command id.
fn icon_index(bit: u32) -> usize {    sysmonitor::ICON_NAMES.iter().position(|(_, b)| *b == bit).unwrap_or(0)
}

static PROBING: AtomicBool = AtomicBool::new(false);

pub(super) fn probing() -> bool {
    PROBING.load(Ordering::Relaxed)
}

/// Runs the device probe in the background, then opens its file in Notepad.
pub(super) fn start_probe() {
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
pub(super) unsafe fn copy_diagnostics(hwnd: HWND) {
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
