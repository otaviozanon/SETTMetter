//! Window plumbing.
//!
//! - A hidden top-level "controller" window receives broadcasts (TaskbarCreated after an
//!   Explorer restart, theme changes, device changes), owns the timer and the context menu.
//! - The visible "widget" is a layered child window of the taskbar (Shell_TrayWnd), placed
//!   just left of the notification area. It dies with Explorer and gets recreated.
//! - A worker thread scans devices and posts WM_APP_UPDATE to the controller.

mod menu;
mod tray;

use crate::device::{Reading, Status, Tracker};
use crate::{areson, bluetooth, corsair, corsair_slipstream, delux, device, diag, headsetcontrol, hid_battery, history, init_com, logitech, mchose, mchose_mouse, notify, playstation, razer, render, settings, steelseries, sysmonitor, wide, xbox};
use std::cell::RefCell;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, SetProcessWorkingSetSize};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_APP_UPDATE: u32 = WM_APP + 1;
const WM_APP_MENU: u32 = WM_APP + 2;
const TIMER_PLACE: usize = 1;
/// WM_POWERBROADCAST: resumed from sleep/hibernate.
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
/// Win11 secondary taskbars draw the clock with no HWND and have no TrayNotifyWnd, so the
/// widget reserves this width (at 100% DPI) instead of measuring one.
const CLOCK_RESERVE: i32 = 88;

/// HID++ is cheap to poll. Bluetooth reads wake the device's radio and HeadsetControl spawns
/// a process, so poll those less often.
const POLL_LOGITECH: Duration = Duration::from_secs(15);
const POLL_BLUETOOTH: Duration = Duration::from_secs(60);

static READINGS: Mutex<Option<Vec<Reading>>> = Mutex::new(None);
static SYS_SNAPSHOT: Mutex<Option<crate::sysmonitor::Snapshot>> = Mutex::new(None);
static WAKE: OnceLock<Sender<()>> = OnceLock::new();

struct App {
    controller: HWND,
    widget: HWND,
    tooltip: HWND,
    taskbar_created: u32,
    light: bool,
    /// Rendered widget size; (0, 0) when there is nothing to show.
    size: (i32, i32),
    /// Taskbar layout the current bitmap was rendered for.
    layout: Option<render::Layout>,
}

thread_local! {
    static APP: RefCell<App> = const {
        RefCell::new(App {
            controller: null_mut(),
            widget: null_mut(),
            tooltip: null_mut(),
            taskbar_created: 0,
            light: false,
            size: (0, 0),
            layout: None,
        })
    };
}

/// Short borrows only; never hold one across a Win32 call that can dispatch messages.
fn with<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|a| f(&mut a.borrow_mut()))
}

pub fn run() {
    unsafe {
        let mutex_name = wide(r"Local\SETTMetter");
        CreateMutexW(null(), 0, mutex_name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return; // already running
        }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        settings::ensure_autostart();
        settings::register_app_id();
        diag::start();

        let hinst = GetModuleHandleW(null());
        let ctrl_class = wide("SETTMetterController");
        register(hinst, &ctrl_class, controller_proc);
        register(hinst, &wide("SETTMetterWidget"), widget_proc);

        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        let controller = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            ctrl_class.as_ptr(),
            ctrl_class.as_ptr(),
            WS_POPUP,
            0, 0, 0, 0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        with(|a| {
            a.controller = controller;
            a.taskbar_created = taskbar_created;
            a.light = settings::light_theme();
        });

        let (tx, rx) = mpsc::channel();
        let _ = WAKE.set(tx);
        let controller_addr = controller as usize;
        // 64 KiB stacks instead of Rust's 2 MiB default:
        // neither loop recurses or keeps large locals, so the extra reserved address space
        // and committed guard pages were pure waste.
        const STACK: usize = 64 * 1024;
        let _ = std::thread::Builder::new().stack_size(STACK).spawn(move || worker(controller_addr, rx));
        let _ = std::thread::Builder::new().stack_size(STACK).spawn(move || sysmonitor_worker(controller_addr));

        ensure_widget();
        SetTimer(controller, TIMER_PLACE, 1000, None);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn register(hinst: HINSTANCE, class: &[u16], proc_: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT) {
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(proc_),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: null_mut(),
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: null_mut(),
        lpszMenuName: null(),
        lpszClassName: class.as_ptr(),
    };
    RegisterClassW(&wc);
}

/// Debug builds: `--demo` shows fixed fake readings covering every visual state.
pub static DEMO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn demo_readings() -> Vec<Reading> {
    use crate::device::Kind;
    let r = |name: &str, kind, percent, charging, stale| Reading {
        id: name.into(),
        name: name.into(),
        kind,
        status: Status::Battery { percent, charging, stale },
    };
    vec![
        r("Headset", Kind::Headset, 64, true, false),
        r("Earbuds", Kind::Earbuds, 12, false, false),
        r("Mouse", Kind::Mouse, 87, false, true),
        r("Keyboard", Kind::Keyboard, 45, false, false),
    ]
}

fn worker(controller: usize, rx: Receiver<()>) {
    if DEMO.load(std::sync::atomic::Ordering::Relaxed) {
        *READINGS.lock().unwrap() = Some(demo_readings());
        unsafe { PostMessageW(controller as HWND, WM_APP_UPDATE, 0, 0) };
        return;
    }
    init_com();
    let mut api = match hidapi::HidApi::new() {
        Ok(api) => api,
        Err(e) => {
            diag::event("hid", format!("hidapi failed to start: {e}"));
            return;
        }
    };
    let mut logi = logitech::Logitech::new();
    let mut delux = delux::Delux::default();
    let mut areson = areson::Areson::default();
    let mut mchose = mchose::Mchose::default();
    let mut mchose_mice = mchose_mouse::MchoseMice::default();
    let mut corsair = corsair::Corsair::default();
    let mut corsair_slipstream = corsair_slipstream::CorsairSlipstream::default();
    let mut hid_battery = hid_battery::HidBattery::default();
    let mut razer = razer::Razer::default();
    let mut steelseries = steelseries::SteelSeries::default();
    let mut tracker = Tracker::default();
    let mut alerts = notify::LowBatteryAlerts::default();
    let mut bt = Vec::new();
    let mut last_bt: Option<Instant> = None;
    let mut forced = true;

    loop {
        let _ = api.refresh_devices();
        let mut obs = logi.scan(&api);
        obs.extend(playstation::scan(&api));
        obs.extend(delux.scan(&api));
        obs.extend(areson.scan(&api));
        obs.extend(mchose.scan(&api));
        obs.extend(mchose_mice.scan(&api));
        obs.extend(corsair.scan(&api));
        obs.extend(razer.scan(&api));
        obs.extend(steelseries.scan(&api));
        obs.extend(corsair_slipstream.scan(&api));
        obs.extend(hid_battery.scan(&api));
        obs.extend(xbox::scan());
        if forced || last_bt.is_none_or(|t| t.elapsed() >= POLL_BLUETOOTH) {
            bt = bluetooth::scan();
            if let Some(exe) = headsetcontrol::find() {
                bt.extend(headsetcontrol::scan(&exe, |pid| device::read_natively(&obs, pid)));
            }
            last_bt = Some(Instant::now());
        }
        // Re-feed the last Bluetooth results between Bluetooth scans so they don't look gone.
        obs.extend(bt.iter().cloned());
        let readings = tracker.update(obs);
        alerts.check(&readings);
        *READINGS.lock().unwrap() = Some(readings);
        unsafe { PostMessageW(controller as HWND, WM_APP_UPDATE, 0, 0) };
        trim_working_set();

        forced = match rx.recv_timeout(POLL_LOGITECH) {
            Ok(()) => {
                // Device changes come in bursts; let things settle, then scan once.
                std::thread::sleep(Duration::from_millis(1500));
                while rx.try_recv().is_ok() {}
                true
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => return,
        };
    }
}

/// Polls CPU/RAM/GPU/VRAM usage and CPU/GPU/SSD temperatures once a second, and redraws on every
/// sample (usage changes fast enough to want it).
fn sysmonitor_worker(controller: usize) {
    if DEMO.load(Ordering::Relaxed) {
        return;
    }
    init_com(); // needed on this thread for the WinRT toast temperature alerts use
    let mut mon = sysmonitor::SysMonitor::new();
    let mut temp_alerts = notify::TempAlerts::default();
    loop {
        let hidden_gpu = settings::hidden_gpu_sensors();
        let raw = mon.sample(&hidden_gpu);
        let gpu_temps: Vec<_> = raw.gpus.iter().map(|g| g.temp).collect();
        temp_alerts.check(raw.cpu_temp, &gpu_temps);
        history::push(&raw);
        let snap = raw.apply(settings::sensor_mask(), &hidden_gpu);
        *SYS_SNAPSHOT.lock().unwrap() = Some(snap);
        unsafe { PostMessageW(controller as HWND, WM_APP_UPDATE, 0, 0) };
        trim_working_set();
        std::thread::sleep(Duration::from_millis(settings::refresh_interval_ms()));
    }
}

fn sys_snapshot() -> Option<sysmonitor::Snapshot> {
    SYS_SNAPSHOT.lock().unwrap().clone()
}

/// The app is idle between scans, so hand its pages back to Windows until the next one.
/// They move to the standby list and come back cheaply when touched.
fn trim_working_set() {
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
}

fn wake() {
    if let Some(tx) = WAKE.get() {
        let _ = tx.send(());
    }
}

fn readings() -> Vec<Reading> {
    READINGS.lock().unwrap().clone().unwrap_or_default()
}

/// Taskbar holding the widget: the one on the screen picked in the menu, or the primary one
/// when no screen was picked or the picked screen is gone (unplugged).
unsafe fn find_tray() -> HWND {
    let primary = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
    let wanted = settings::screen();
    if wanted == 0 {
        return primary;
    }
    taskbars()
        .into_iter()
        .find(|(number, _)| *number == wanted)
        .map_or(primary, |(_, tray)| tray)
}

/// Every taskbar on screen, with the Windows display number of its monitor. Secondary taskbars
/// only exist while "Show my taskbar on all displays" is on (Settings > Personalization > Taskbar).
unsafe fn taskbars() -> Vec<(u32, HWND)> {
    let mut trays = Vec::new();
    EnumWindows(Some(collect_tray_window), &mut trays as *mut _ as LPARAM);
    let mut screens: Vec<_> = trays
        .into_iter()
        .filter_map(|tray| Some((display_number(MonitorFromWindow(tray, MONITOR_DEFAULTTONEAREST))?, tray)))
        .collect();
    screens.sort_by_key(|(number, _)| *number);
    screens
}

unsafe extern "system" fn collect_tray_window(hwnd: HWND, data: LPARAM) -> BOOL {
    let mut class = [0u16; 32];
    let len = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32).max(0) as usize;
    let name = String::from_utf16_lossy(&class[..len]);
    if name == "Shell_TrayWnd" || name == "Shell_SecondaryTrayWnd" {
        (*(data as *mut Vec<HWND>)).push(hwnd);
    }
    1
}

/// Windows display number from a monitor handle, e.g. `1` for `\\.\DISPLAY1` (the number shown
/// in Settings > Display).
unsafe fn display_number(hmon: HMONITOR) -> Option<u32> {
    let mut info: MONITORINFOEXW = zeroed();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(hmon, &mut info as *mut _ as *mut MONITORINFO) == 0 {
        return None;
    }
    let len = info.szDevice.iter().position(|c| *c == 0).unwrap_or(info.szDevice.len());
    let name = String::from_utf16_lossy(&info.szDevice[..len]);
    name.strip_prefix(r"\\.\DISPLAY")?.parse().ok()
}

/// Screens whose taskbar can host the widget, with whether each is the primary monitor.
pub(super) unsafe fn screens() -> Vec<(u32, bool)> {
    let primary = MonitorFromWindow(FindWindowW(wide("Shell_TrayWnd").as_ptr(), null()), MONITOR_DEFAULTTONEAREST);
    taskbars()
        .into_iter()
        .map(|(number, tray)| (number, MonitorFromWindow(tray, MONITOR_DEFAULTTONEAREST) == primary))
        .collect()
}

/// Moves the widget (and its tooltip) to another screen's taskbar.
pub(super) unsafe fn select_screen(number: u32) {
    settings::set_screen(number);
    destroy_widget();
    ensure_widget();
}

unsafe fn destroy_widget() {
    let (widget, tooltip) = with(|a| {
        let (widget, tooltip) = (a.widget, a.tooltip);
        a.widget = null_mut();
        a.tooltip = null_mut();
        a.size = (0, 0);
        a.layout = None;
        (widget, tooltip)
    });
    if !tooltip.is_null() {
        DestroyWindow(tooltip);
    }
    if !widget.is_null() {
        DestroyWindow(widget);
    }
}

unsafe fn ensure_widget() {
    let tray = find_tray();
    let widget = with(|a| a.widget);
    // Still attached to the right taskbar? A taskbar restart or another screen recreates it.
    if !widget.is_null() && IsWindow(widget) != 0 && GetParent(widget) == tray {
        return;
    }
    destroy_widget();
    if tray.is_null() {
        return; // Explorer not up yet; the timer retries.
    }
    let class = wide("SETTMetterWidget");
    let w = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class.as_ptr(),
        null(),
        WS_CHILD | WS_CLIPSIBLINGS,
        0, 0, 1, 1,
        tray,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    let controller = with(|a| a.controller);
    let tip = create_tooltip(controller, w);
    with(|a| {
        a.widget = w;
        a.tooltip = tip;
        a.size = (0, 0);
        a.layout = None;
    });
    redraw();
}

/// Hover tooltip listing each device's full name and status.
unsafe fn create_tooltip(owner: HWND, widget: HWND) -> HWND {
    let icc = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_WIN95_CLASSES };
    InitCommonControlsEx(&icc);
    let tip = CreateWindowExW(
        WS_EX_TOPMOST,
        wide("tooltips_class32").as_ptr(),
        null(),
        WS_POPUP | TTS_NOPREFIX | TTS_ALWAYSTIP,
        CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT,
        owner,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if tip.is_null() {
        return tip;
    }
    let mut text = wide("");
    let mut ti = tool_info(widget, owner, &mut text);
    SendMessageW(tip, TTM_ADDTOOLW, 0, &mut ti as *mut _ as LPARAM);
    SendMessageW(tip, TTM_SETMAXTIPWIDTH, 0, 600); // enables multi-line text
    tip
}

unsafe fn tool_info(widget: HWND, owner: HWND, text: &mut [u16]) -> TTTOOLINFOW {
    let mut ti: TTTOOLINFOW = zeroed();
    ti.cbSize = size_of::<TTTOOLINFOW>() as u32;
    ti.uFlags = TTF_IDISHWND | TTF_SUBCLASS;
    ti.hwnd = owner;
    ti.uId = widget as usize;
    ti.lpszText = text.as_mut_ptr();
    ti
}

unsafe fn set_tooltip_text(lines: &[String]) {
    let (tip, widget, controller) = with(|a| (a.tooltip, a.widget, a.controller));
    if tip.is_null() {
        return;
    }
    let mut text = wide(&lines.join("\r\n"));
    let ti = tool_info(widget, controller, &mut text);
    SendMessageW(tip, TTM_UPDATETIPTEXTW, 0, &ti as *const _ as LPARAM);
}

unsafe fn redraw() {
    let (widget, light) = with(|a| (a.widget, a.light));
    if widget.is_null() || IsWindow(widget) == 0 {
        return;
    }
    let tray = find_tray();
    if tray.is_null() {
        return;
    }
    let layout = taskbar_layout(tray);
    let dpi = GetDpiForWindow(tray).max(96);
    let hidden = settings::hidden_devices();
    // Disconnected devices drop off the bar (they stay listed in the right-click menu).
    let shown: Vec<_> = readings()
        .into_iter()
        .filter(|r| !hidden.contains(&r.id) && r.status != Status::NotConnected)
        .collect();
    let mut lines: Vec<String> = shown.iter().map(|r| format!("{}: {}", r.name, r.status)).collect();
    let items: Vec<_> = shown.iter().map(|r| (r.kind, r.status)).collect();
    let sys = sys_snapshot();
    if let Some(s) = &sys {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        if let Some((name, pct)) = &s.top_cpu_process {
            lines.push(format!("Top CPU: {name} ({pct:.0}%)"));
        }
        if let Some((name, mb)) = &s.top_ram_process {
            lines.push(format!("Top RAM: {name} ({mb:.0} MB)"));
        }
        if let Some((name, pct)) = &s.top_gpu_process {
            lines.push(format!("Top GPU: {name} ({pct:.0}%)"));
        }
    }
    set_tooltip_text(&lines);
    let all_hidden = items.is_empty() && sys.as_ref().is_none_or(|s| s.all_hidden());
    tray::set_tray_icon(all_hidden);
    let size = if items.is_empty() && sys.is_none() {
        (0, 0)
    } else {
        render::draw(widget, &items, sys.as_ref(), settings::icon_mask(), dpi, layout, light)
    };
    with(|a| {
        a.size = size;
        a.layout = Some(layout);
    });
    place();
}

/// A taskbar docked left/right is taller than wide; lay the widget out as a column there.
unsafe fn taskbar_layout(tray: HWND) -> render::Layout {
    let mut tr: RECT = zeroed();
    GetWindowRect(tray, &mut tr);
    let (w, h) = (tr.right - tr.left, tr.bottom - tr.top);
    if h > w {
        render::Layout::Column { width: w }
    } else {
        render::Layout::Row { height: h }
    }
}

/// Keeps the widget next to the notification area (left of it on a horizontal taskbar, above
/// it on a vertical one) and above the taskbar's XAML content.
pub(super) unsafe fn place() {
    let (widget, (width, height), rendered) = with(|a| (a.widget, a.size, a.layout));
    if widget.is_null() || IsWindow(widget) == 0 {
        return;
    }
    let tray = find_tray();
    if tray.is_null() {
        return;
    }
    // Taskbar moved to another edge or resized: the bitmap was drawn for the old shape.
    if rendered != Some(taskbar_layout(tray)) {
        redraw(); // sets the new layout, then calls back here
        return;
    }
    if width == 0 {
        ShowWindow(widget, SW_HIDE); // nothing to show
        return;
    }
    let mut tr: RECT = zeroed();
    GetWindowRect(tray, &mut tr);
    let mut nr: RECT = zeroed();
    let notify = FindWindowExW(tray, null_mut(), wide("TrayNotifyWnd").as_ptr(), null());
    // Win11 secondary taskbars have no TrayNotifyWnd; there the clock is rightmost, so keep the
    // widget left of it too.
    let clock = FindWindowExW(tray, null_mut(), wide("SystemTray.OmniButton").as_ptr(), null());
    let has_notify = !notify.is_null() && GetWindowRect(notify, &mut nr) != 0 && nr.right > nr.left;
    let has_clock = !has_notify && !clock.is_null() && GetWindowRect(clock, &mut nr) != 0 && nr.right > nr.left;
    let dpi = GetDpiForWindow(tray).max(96) as i32;
    let margin = 4 * dpi / 96;
    let offset = settings::widget_offset() * dpi / 96;
    let (x, y) = match rendered {
        Some(render::Layout::Column { .. }) => {
            let bottom = if has_notify || has_clock { nr.top } else { tr.bottom - CLOCK_RESERVE * dpi / 96 };
            (0, bottom - tr.top - height - margin - offset)
        }
        _ => {
            let right = if has_notify || has_clock { nr.left } else { tr.right - CLOCK_RESERVE * dpi / 96 };
            (right - tr.left - width - margin - offset, 0)
        }
    };

    let mut cur: RECT = zeroed();
    GetWindowRect(widget, &mut cur);
    let moved = cur.left - tr.left != x
        || cur.top - tr.top != y
        || cur.right - cur.left != width
        || cur.bottom - cur.top != height;
    let on_top = GetWindow(tray, GW_CHILD) == widget;
    if moved || !on_top || IsWindowVisible(widget) == 0 {
        SetWindowPos(widget, HWND_TOP, x, y, width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}

unsafe extern "system" fn controller_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let taskbar_created = with(|a| a.taskbar_created);
    match msg {
        WM_APP_UPDATE => redraw(),
        WM_TIMER => {
            ensure_widget();
            place();
        }
        WM_SETTINGCHANGE => {
            with(|a| a.light = settings::light_theme());
            redraw();
        }
        WM_DEVICECHANGE => {
            wake();
            return 1;
        }
        WM_POWERBROADCAST => {
            if wp == PBT_APMRESUMEAUTOMATIC {
                wake(); // PC woke from sleep: devices may have changed meanwhile
            }
            return 1;
        }
        WM_APP_MENU => menu::show_menu(hwnd),
        WM_DESTROY => PostQuitMessage(0),
        m if m == taskbar_created && m != 0 => {
            // Explorer restarted: the old widget died with the old taskbar.
            with(|a| a.widget = null_mut());
            ensure_widget();
        }
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    0
}

unsafe extern "system" fn widget_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_RBUTTONUP => {
            let controller = with(|a| a.controller);
            PostMessageW(controller, WM_APP_MENU, 0, 0);
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
