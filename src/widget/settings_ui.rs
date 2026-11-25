//! Settings window: the custom-painted, single-style replacement for the classic right-click
//! menu. One generic row model renders every page; a page is data, not layout code.

use std::ffi::c_void;
use std::mem::size_of;

use winaudio::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use winaudio::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, DeleteDC, DeleteObject, EndPaint, GetMonitorInfoW,
    HDC, HFONT, HGDIOBJ, IntersectClipRect, InvalidateRect, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromPoint, PAINTSTRUCT, RestoreDC, SRCCOPY, SaveDC,
};
use winaudio::Win32::System::LibraryLoader::GetModuleHandleW;
use winaudio::Win32::UI::Controls::WM_MOUSELEAVE;
use winaudio::Win32::UI::HiDpi::GetDpiForWindow;
use winaudio::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE,
};
use winaudio::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA,
    GetWindowLongPtrW, HTCLIENT, HWND_TOPMOST, IDC_ARROW, IDC_HAND, IsWindow, LoadCursorW,
    RegisterClassW, SWP_NOACTIVATE, SW_SHOW, SetCursor, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, WA_INACTIVE, WHEEL_DELTA, WM_ACTIVATE,
    WM_CLOSE, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL,
    WM_NCDESTROY, WM_PAINT, WM_SETCURSOR, WNDCLASSW, WS_CLIPCHILDREN, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};
use winaudio::Win32::UI::WindowsAndMessaging::GetCursorPos;
use winaudio::core::PCWSTR;

use crate::ui::paint::{
    Canvas, FontCache, FontRole, IconKind, RectPx, TextAlign, create_glyph_font, draw_text,
    draw_wrapped_text, measure_text_width, px,
};
use crate::ui::theme::{self, Theme};
use crate::ui::{DibBackbuffer, apply_dwm_chrome};

use super::{WM_APP_EXIT, WM_APP_MENU, menu, readings, redraw, sys_snapshot, wake};
use crate::{settings, sysmonitor, update};
use crate::audio::model::AppAction;

const CLASS_NAME: &str = "SETTMetterSettings";
const W_LOGICAL: f32 = 468.0;
const H_LOGICAL: f32 = 540.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Sensors,
    Display,
    Alerts,
    Devices,
    Audio,
    General,
}

const PAGES: [(&str, Page); 6] = [
    ("Sensors", Page::Sensors),
    ("Display", Page::Display),
    ("Alerts", Page::Alerts),
    ("Devices", Page::Devices),
    ("Audio", Page::Audio),
    ("General", Page::General),
];

/// What a row does when clicked; also the identity used by hover and slider drags.
#[derive(Clone, PartialEq, Eq)]
enum Id {
    Close,
    PowerOff,
    Page(Page),
    Preset(usize),
    Sensor(usize),
    GpuUsage(usize),
    GpuTemp(usize),
    GpuVram(usize),
    Icon(usize),
    Interval(u32),
    Screen(u32),
    Offset,
    OffsetReset,
    CpuThreshold,
    GpuThreshold,
    LowBattery,
    Autostart,
    Refresh,
    History,
    Update,
    Pawnio,
    Diagnostics,
    Probe,
    AudioPanel,
    AudioMic,
    MuteShortcut,
    ShowMic,
    AudioLock,
    AudioSeparate,
    AudioAutoEnh,
    AudioAutoUnused,
    AudioReenableAll,
    AudioRestartService,
    Device(String),
    ClassicMenu,
    Exit,
}

/// A page row, before it becomes a rectangle.
enum Row {
    Section(&'static str),
    /// Two independent lists side by side; the row is as tall as the taller column.
    Split {
        left: Vec<Row>,
        right: Vec<Row>,
    },
    Toggle {
        id: Id,
        label: String,
        value: Option<String>,
        checked: bool,
        enabled: bool,
    },
    /// Icon + label with the checkbox on the right; used by the Display icon grid.
    IconCheck {
        id: Id,
        glyph: &'static str,
        label: String,
        checked: bool,
    },
    Radio {
        id: Id,
        label: String,
        value: Option<String>,
        selected: bool,
    },
    Pill {
        options: Vec<(&'static str, Id)>,
        selected: usize,
    },
    Slider {
        id: Id,
        label: String,
        value: i32,
        min: i32,
        max: i32,
        step: i32,
        unit: &'static str,
    },
    Action {
        id: Id,
        label: String,
        danger: bool,
        enabled: bool,
    },
    Note(String),
}

/// A laid-out row: the rectangles the paint and hit-test passes need.
enum RowLayout {
    Section {
        rect: RectPx,
        label: &'static str,
    },
    Toggle {
        id: Id,
        rect: RectPx,
        box_rect: RectPx,
        label: RectPx,
        value: Option<RectPx>,
        text: String,
        value_text: Option<String>,
        checked: bool,
        radio: bool,
        enabled: bool,
    },
    IconCheck {
        id: Id,
        rect: RectPx,
        glyph_rect: RectPx,
        label_rect: RectPx,
        box_rect: RectPx,
        glyph: &'static str,
        text: String,
        checked: bool,
    },
    Pill {
        rect: RectPx,
        options: Vec<(Id, RectPx, &'static str, bool)>,
    },
    Slider {
        id: Id,
        rect: RectPx,
        track: RectPx,
        label: RectPx,
        value_rect: RectPx,
        text: String,
        value: i32,
        unit: &'static str,
        min: i32,
        max: i32,
        step: i32,
    },
    Action {
        id: Id,
        rect: RectPx,
        label: RectPx,
        text: String,
        danger: bool,
        enabled: bool,
    },
    Note {
        rect: RectPx,
        text: String,
    },
}

struct Data {
    controller: isize,
    scale: f32,
    width: i32,
    height: i32,
    theme: Theme,
    dwm_border_ok: bool,
    fonts: FontCache,
    icon_font: HFONT,
    measure_dc: HDC,
    backbuffer: DibBackbuffer,
    page: Page,
    scroll: i32,
    max_scroll: i32,
    content: RectPx,
    tabs: Vec<(Page, RectPx)>,
    close_btn: RectPx,
    power_btn: RectPx,
    /// Power button armed by the first click; the next one exits.
    quit_armed: bool,
    /// Waiting for the next key press to become the mute shortcut.
    capturing_shortcut: bool,
    rows: Vec<RowLayout>,
    hover: Option<Id>,
    hover_tab: Option<Page>,
    hover_close: bool,
    drag: Option<Id>,
    tracking: bool,
}

impl Drop for Data {
    fn drop(&mut self) {
        if !self.measure_dc.is_invalid() {
            unsafe {
                let _ = DeleteDC(self.measure_dc);
            }
        }
        if !self.icon_font.is_invalid() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.icon_font.0));
            }
        }
    }
}

/// Handle kept in the widget's `App`; the window itself lives in its own HWND.
pub(super) struct SettingsWindow(HWND);

impl SettingsWindow {
    pub(super) fn is_alive(&self) -> bool {
        unsafe { IsWindow(Some(self.0)).as_bool() }
    }

    pub(super) fn focus(&self) {
        unsafe {
            let _ = ShowWindow(self.0, SW_SHOW);
            let _ = SetForegroundWindow(self.0);
        }
        self.invalidate();
    }

    pub(super) fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.0), None, false);
        }
    }
}fn register_class() {
    // Idempotent: Windows fails the duplicate register with ERROR_CLASS_ALREADY_EXISTS, harmless.
    let class = crate::audio::win32::to_wide(CLASS_NAME);
    let hinstance = unsafe { GetModuleHandleW(None) }.ok();
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance.map(Into::into).unwrap_or_default(),
        hIcon: Default::default(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        hbrBackground: Default::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class.as_ptr()),
    };
    unsafe {
        let _ = RegisterClassW(&wc);
    }
}

fn window_rect(anchor: (i32, i32), scale: f32) -> (i32, i32, i32, i32) {
    let monitor = unsafe {
        MonitorFromPoint(
            POINT {
                x: anchor.0,
                y: anchor.1,
            },
            MONITOR_DEFAULTTOPRIMARY,
        )
    };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let win_w = px(W_LOGICAL, scale);
    let win_h = px(H_LOGICAL, scale);
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return (anchor.0 - win_w, anchor.1 - win_h, win_w, win_h);
    }
    // Small screens: never taller/wider than the work area. The page list scrolls.
    let work = info.rcWork;
    let margin = px(8.0, scale);
    let win_w = win_w.min(work.right - work.left - margin * 2);
    let win_h = win_h.min(work.bottom - work.top - margin * 2);
    let x = (anchor.0 - win_w)
        .clamp(work.left + margin, (work.right - margin - win_w).max(work.left + margin));
    // Dock against the edge the anchor (the taskbar) is on: bottom taskbars get a panel whose
    // bottom edge sits on the taskbar, top taskbars the mirror image.
    let y = if anchor.1 >= work.top + (work.bottom - work.top) / 2 {
        work.bottom - margin - win_h
    } else {
        work.top + margin
    };
    (x, y, win_w, win_h)
}

/// Opens the window at the cursor, or brings the existing one forward.
pub(super) fn open() {
    // Keep `App` unborrowed across the Win32 calls below: focusing a window dispatches messages
    // that come back into the widget procs, which borrow it again.
    if let Some(window) = super::with(|a| a.settings.take())
        && window.is_alive()
    {
        window.focus();
        super::with(|a| a.settings = Some(window));
        return;
    }
    let Some(window) = create() else { return };
    super::with(|a| a.settings = Some(window));
}

fn create() -> Option<SettingsWindow> {
    register_class();

    let mut cursor = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut cursor);
    }
    let dpi = unsafe {
        let base = super::with(|a| if a.widget.is_null() { a.controller } else { a.widget });
        GetDpiForWindow(HWND(base))
    }
    .max(96);
    let scale = f32::from(dpi as u16) / 96.0;
    let (x, y, w, h) = window_rect((cursor.x, cursor.y), scale);

    let class = crate::audio::win32::to_wide(CLASS_NAME);
    let hinstance = unsafe { GetModuleHandleW(None) }.ok();
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(class.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_CLIPCHILDREN,
            x,
            y,
            w,
            h,
            None,
            None,
            hinstance.map(Into::into),
            None,
        )
    }
    .ok()?;

    let measure_dc = unsafe { CreateCompatibleDC(None) };
    let theme = theme::current();
    let dwm_ok = apply_dwm_chrome(hwnd, theme.border_strong);

    let mut data = Box::new(Data {
        controller: super::with(|a| a.controller) as isize,
        scale,
        width: w,
        height: h,
        theme,
        dwm_border_ok: dwm_ok,
        fonts: FontCache::new(scale),
        icon_font: create_glyph_font("Segoe Fluent Icons", 13.0, scale),
        measure_dc,
        backbuffer: DibBackbuffer::empty(),
        page: Page::Sensors,
        scroll: 0,
        max_scroll: 0,
        content: RectPx::default(),
        tabs: Vec::new(),
        close_btn: RectPx::default(),
        power_btn: RectPx::default(),
        quit_armed: false,
        capturing_shortcut: false,
        rows: Vec::new(),
        hover: None,
        hover_tab: None,
        hover_close: false,
        drag: None,
        tracking: false,
    });
    data.relayout();

    let ptr = Box::into_raw(data);
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    Some(SettingsWindow(hwnd))
}

// ---------------------------------------------------------------- layout

impl Data {
    fn relayout(&mut self) {
        let s = self.scale;
        let pad = px(14.0, s);
        let tab_h = px(30.0, s);
        let tab_top = px(48.0, s);
        let gap = px(4.0, s);
        let inner_w = self.width - pad * 2;

        let cell = (inner_w - gap * (PAGES.len() as i32 - 1)) / PAGES.len() as i32;
        self.tabs.clear();
        for (i, (_, page)) in PAGES.iter().enumerate() {
            let left = pad + (cell + gap) * i as i32;
            self.tabs
                .push((*page, RectPx::from_xywh(left, tab_top, cell, tab_h)));
        }

        let close_size = px(26.0, s);
        self.close_btn = RectPx::from_xywh(
            self.width - pad - close_size,
            px(10.0, s),
            close_size,
            px(24.0, s),
        );
        self.power_btn = RectPx::from_xywh(
            self.close_btn.left - px(4.0, s) - close_size,
            px(10.0, s),
            close_size,
            px(24.0, s),
        );

        let scroll_w = px(8.0, s);
        self.content = RectPx::new(
            pad,
            tab_top + tab_h + px(12.0, s),
            self.width - pad - scroll_w,
            self.height - pad,
        );

        let (_, total) = layout_rows(page_rows(self.page, self.capturing_shortcut), self.content, s, 0, self.measure_dc, &self.fonts);
        let max_scroll = (total - self.content.height()).max(0);
        self.max_scroll = max_scroll;
        self.scroll = self.scroll.clamp(0, max_scroll);
        let (rows, _) = layout_rows(
            page_rows(self.page, self.capturing_shortcut),
            self.content,
            s,
            self.scroll,
            self.measure_dc,
            &self.fonts,
        );
        self.rows = rows;
    }

    fn scroll_by(&mut self, delta: i32) {
        self.scroll = (self.scroll + delta).clamp(0, self.max_scroll);
        self.relayout();
    }

    fn hit(&self, x: i32, y: i32) -> Option<Id> {
        if self.close_btn.contains(x, y) {
            return Some(Id::Close);
        }
        if self.power_btn.contains(x, y) {
            return Some(Id::PowerOff);
        }
        for (page, rect) in &self.tabs {
            if rect.contains(x, y) {
                return Some(Id::Page(*page));
            }
        }
        if !self.content.contains(x, y) {
            return None;
        }
        for row in &self.rows {
            match row {
                RowLayout::Toggle {
                    id,
                    rect,
                    enabled,
                    ..
                } if *enabled && rect.contains(x, y) => {
                    return Some(id.clone());
                }
                RowLayout::Action {
                    id,
                    rect,
                    enabled,
                    ..
                } if *enabled && rect.contains(x, y) => {
                    return Some(id.clone());
                }
                RowLayout::Slider { id, rect, .. } if rect.contains(x, y) => {
                    return Some(id.clone());
                }
                RowLayout::IconCheck { id, rect, .. } if rect.contains(x, y) => {
                    return Some(id.clone());
                }
                RowLayout::Pill { options, .. } => {
                    for (id, rect, _, _) in options {
                        if rect.contains(x, y) {
                            return Some(id.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }
}

fn layout_rows(
    items: Vec<Row>,
    content: RectPx,
    s: f32,
    scroll: i32,
    dc: HDC,
    fonts: &FontCache,
) -> (Vec<RowLayout>, i32) {
    let (rows, end_y) = layout_column(items, content, s, content.top - scroll, dc, fonts);
    let total = (end_y + scroll) - content.top;
    (rows, total)
}

fn layout_column(
    items: Vec<Row>,
    content: RectPx,
    s: f32,
    start_y: i32,
    dc: HDC,
    fonts: &FontCache,
) -> (Vec<RowLayout>, i32) {
    let gap = px(4.0, s);
    let mut rows = Vec::with_capacity(items.len());
    let mut y = start_y;

    for item in items {
        match item {
            Row::Split { left, right } => {
                let split_gap = px(12.0, s);
                let width = (content.width() - split_gap) / 2;
                let left_rect = RectPx::new(
                    content.left,
                    y,
                    content.left + width,
                    content.bottom,
                );
                let right_rect = RectPx::new(
                    content.left + width + split_gap,
                    y,
                    content.right,
                    content.bottom,
                );
                let (mut left_rows, left_end) = layout_column(left, left_rect, s, y, dc, fonts);
                let (mut right_rows, right_end) = layout_column(right, right_rect, s, y, dc, fonts);
                rows.append(&mut left_rows);
                rows.append(&mut right_rows);
                y = left_end.max(right_end);
            }
            Row::Section(label) => {
                let h = px(32.0, s);
                rows.push(RowLayout::Section {
                    rect: RectPx::from_xywh(
                        content.left,
                        y,
                        content.width(),
                        h,
                    ),
                    label,
                });
                y += h;
            }
            Row::Toggle {
                id,
                label,
                value,
                checked,
                enabled,
            } => {
                let h = px(42.0, s);
                let rect = RectPx::from_xywh(content.left, y, content.width(), h);
                let box_size = px(16.0, s);
                let box_rect = RectPx::from_xywh(
                    rect.left + px(8.0, s),
                    rect.top + (h - box_size) / 2,
                    box_size,
                    box_size,
                );
                let text_left = box_rect.right + px(10.0, s);
                let value_w = value
                    .as_ref()
                    .map(|v| {
                        measure_text_width(dc, fonts.get(FontRole::Body11_5Semi), v)
                            + px(4.0, s)
                    })
                    .unwrap_or(0)
                    .min(content.width() / 2);
                let value_rect = (value_w > 0).then(|| {
                    RectPx::new(
                        rect.right - px(10.0, s) - value_w,
                        rect.top,
                        rect.right - px(10.0, s),
                        rect.bottom,
                    )
                });
                let label_right = value_rect.map(|v| v.left - px(8.0, s)).unwrap_or(rect.right - px(10.0, s));
                let label_rect = RectPx::new(text_left, rect.top, label_right, rect.bottom);
                rows.push(RowLayout::Toggle {
                    id,
                    rect,
                    box_rect,
                    label: label_rect,
                    value: value_rect,
                    text: label,
                    value_text: value,
                    checked,
                    radio: false,
                    enabled,
                });
                y += h + gap;
            }
            Row::IconCheck {
                id,
                glyph,
                label,
                checked,
            } => {
                let h = px(40.0, s);
                let rect = RectPx::from_xywh(content.left, y, content.width(), h);
                let glyph_size = px(20.0, s);
                let glyph_rect = RectPx::from_xywh(
                    rect.left + px(8.0, s),
                    rect.top + (h - glyph_size) / 2,
                    glyph_size,
                    glyph_size,
                );
                let box_size = px(16.0, s);
                let box_rect = RectPx::from_xywh(
                    rect.right - px(10.0, s) - box_size,
                    rect.top + (h - box_size) / 2,
                    box_size,
                    box_size,
                );
                let label_rect = RectPx::new(
                    glyph_rect.right + px(10.0, s),
                    rect.top,
                    box_rect.left - px(8.0, s),
                    rect.bottom,
                );
                rows.push(RowLayout::IconCheck {
                    id,
                    rect,
                    glyph_rect,
                    label_rect,
                    box_rect,
                    glyph,
                    text: label,
                    checked,
                });
                y += h + gap;
            }
            Row::Radio {
                id,
                label,
                value,
                selected,
            } => {
                let h = px(38.0, s);
                let rect = RectPx::from_xywh(content.left, y, content.width(), h);
                let box_size = px(14.0, s);
                let box_rect = RectPx::from_xywh(
                    rect.left + px(9.0, s),
                    rect.top + (h - box_size) / 2,
                    box_size,
                    box_size,
                );
                let text_left = box_rect.right + px(10.0, s);
                let value_rect = value.as_ref().map(|v| {
                    let w = (measure_text_width(dc, fonts.get(FontRole::Body11_5Semi), v)
                        + px(4.0, s))
                    .min(content.width() / 2);
                    RectPx::new(rect.right - px(10.0, s) - w, rect.top, rect.right - px(10.0, s), rect.bottom)
                });
                let label_right = value_rect.map(|v| v.left - px(8.0, s)).unwrap_or(rect.right - px(10.0, s));
                rows.push(RowLayout::Toggle {
                    id,
                    rect,
                    box_rect,
                    label: RectPx::new(text_left, rect.top, label_right, rect.bottom),
                    value: value_rect,
                    text: label,
                    value_text: value,
                    checked: selected,
                    radio: true,
                    enabled: true,
                });
                y += h + gap;
            }
            Row::Pill { options, selected } => {
                let h = px(38.0, s);
                let rect = RectPx::from_xywh(content.left, y, content.width(), h);
                let pad = px(4.0, s);
                let count = options.len().max(1) as i32;
                let cell = (rect.width() - pad * 2 - px(4.0, s) * (count - 1)) / count;
                let laid = options
                    .into_iter()
                    .enumerate()
                    .map(|(i, (label, id))| {
                        let left = rect.left + pad + (cell + px(4.0, s)) * i as i32;
                        let r = RectPx::from_xywh(left, rect.top + pad, cell, h - pad * 2);
                        (id, r, label, i == selected)
                    })
                    .collect();
                rows.push(RowLayout::Pill { rect, options: laid });
                y += h + gap;
            }
            Row::Slider {
                id,
                label,
                value,
                min,
                max,
                step,
                unit,
            } => {
                let h = px(56.0, s);
                let rect = RectPx::from_xywh(content.left, y, content.width(), h);
                let label_rect = RectPx::new(
                    rect.left + px(8.0, s),
                    rect.top + px(6.0, s),
                    rect.right - px(80.0, s),
                    rect.top + px(26.0, s),
                );
                let value_rect = RectPx::new(
                    rect.right - px(90.0, s),
                    rect.top + px(6.0, s),
                    rect.right - px(10.0, s),
                    rect.top + px(26.0, s),
                );
                let track = RectPx::from_xywh(
                    rect.left + px(8.0, s),
                    rect.bottom - px(17.0, s),
                    rect.width() - px(16.0, s),
                    px(8.0, s),
                );
                rows.push(RowLayout::Slider {
                    id,
                    rect,
                    track,
                    label: label_rect,
                    value_rect,
                    text: label,
                    value,
                    unit,
                    min,
                    max,
                    step,
                });
                y += h + gap;
            }
            Row::Action {
                id,
                label,
                danger,
                enabled,
            } => {
                let h = px(40.0, s);
                let rect = RectPx::from_xywh(content.left, y, content.width(), h);
                let label_rect = RectPx::new(
                    rect.left + px(10.0, s),
                    rect.top,
                    rect.right - px(10.0, s),
                    rect.bottom,
                );
                rows.push(RowLayout::Action {
                    id,
                    rect,
                    label: label_rect,
                    text: label,
                    danger,
                    enabled,
                });
                y += h + gap;
            }
            Row::Note(text) => {
                let h = px(46.0, s);
                let rect = RectPx::from_xywh(content.left, y, content.width(), h);
                rows.push(RowLayout::Note {
                    rect: RectPx::new(
                        rect.left + px(10.0, s),
                        rect.top,
                        rect.right - px(10.0, s),
                        rect.bottom,
                    ),
                    text,
                });
                y += h + gap;
            }
        }
    }

    (rows, y)
}

// ---------------------------------------------------------------- rows

fn page_rows(page: Page, capturing_shortcut: bool) -> Vec<Row> {
    match page {
        Page::Sensors => sensors_page(),
        Page::Display => display_page(),
        Page::Alerts => alerts_page(),
        Page::Devices => devices_page(),
        Page::Audio => audio_page(capturing_shortcut),
        Page::General => general_page(),
    }
}

fn sensors_page() -> Vec<Row> {
    let snap = sys_snapshot().unwrap_or_default();
    let mask = settings::sensor_mask();
    let hidden_gpu = settings::hidden_gpu_sensors();
    let multi = snap.gpus.len() > 1;
    let mut rows = vec![Row::Section("PRESETS")];

    for (i, (name, _, _, _)) in menu::PRESETS.iter().enumerate() {
        rows.push(Row::Action {
            id: Id::Preset(i),
            label: name.to_string(),
            danger: false,
            enabled: true,
        });
    }

    let mut system = vec![Row::Section("SYSTEM")];
    for (i, (name, bit)) in sysmonitor::SENSOR_NAMES.iter().enumerate() {
        let value = match *bit {
            sysmonitor::BIT_CPU => snap.cpu_pct.map(|v| format!("{v:.0}%")),
            sysmonitor::BIT_CPU_TEMP => snap.cpu_temp.map(|v| format!("{v:.0} \u{b0}C")),
            sysmonitor::BIT_RAM => snap.ram_pct.map(|v| format!("{v:.0}%")),
            sysmonitor::BIT_DISK_TEMP => snap.disk_temp.map(|v| format!("{v:.0} \u{b0}C")),
            _ => None,
        };
        system.push(Row::Toggle {
            id: Id::Sensor(i),
            label: (*name).to_string(),
            value,
            checked: mask & bit != 0,
            enabled: true,
        });
    }

    let mut graphics = vec![Row::Section("GRAPHICS")];
    for (i, gpu) in snap.gpus.iter().enumerate() {
        let prefix = if multi {
            format!("GPU {} ", i + 1)
        } else {
            "GPU ".to_string()
        };
        graphics.push(Row::Toggle {
            id: Id::GpuUsage(i),
            label: format!("{prefix}usage"),
            value: gpu.pct.map(|v| format!("{v:.0}%")),
            checked: !hidden_gpu.iter().any(|h| h == &format!("gpu{i}")),
            enabled: true,
        });
        graphics.push(Row::Toggle {
            id: Id::GpuTemp(i),
            label: format!("{prefix}temperature"),
            value: gpu.temp.map(|v| format!("{v:.0} \u{b0}C")),
            checked: !hidden_gpu.iter().any(|h| h == &format!("gputemp{i}")),
            enabled: true,
        });
        graphics.push(Row::Toggle {
            id: Id::GpuVram(i),
            label: if multi {
                format!("VRAM {}", i + 1)
            } else {
                "VRAM".to_string()
            },
            value: Some(format!("{:.1} / {:.0} GB", gpu.vram_used_gb, gpu.vram_total_gb)),
            checked: !hidden_gpu.iter().any(|h| h == &format!("vram{i}")),
            enabled: true,
        });
    }

    if snap.gpus.is_empty() {
        rows.append(&mut system);
    } else {
        rows.push(Row::Split {
            left: system,
            right: graphics,
        });
    }

    rows
}

fn display_page() -> Vec<Row> {
    let icon_mask = settings::icon_mask();
    let bit = |index: usize| sysmonitor::ICON_NAMES.get(index).map(|(_, bit)| *bit).unwrap_or(0);
    let entry = |index: usize, glyph: &'static str, label: &str| Row::IconCheck {
        id: Id::Icon(index),
        glyph,
        label: label.to_string(),
        checked: icon_mask & bit(index) != 0,
    };
    // Same glyphs the widget draws on the taskbar (Segoe Fluent Icons).
    let mut rows = vec![
        Row::Section("ICONS"),
        Row::Split {
            left: vec![
                entry(0, "\u{E9D9}", "CPU"),
                entry(2, "\u{EEA0}", "RAM"),
                entry(4, "\u{EDA2}", "SSD"),
            ],
            right: vec![
                entry(1, "\u{E950}", "GPU"),
                entry(3, "\u{E964}", "VRAM"),
            ],
        },
    ];

    rows.push(Row::Section("REFRESH RATE"));
    let interval = settings::refresh_interval_ms();
    let selected = match interval {
        1000 => 0,
        5000 => 2,
        _ => 1,
    };
    rows.push(Row::Pill {
        options: vec![
            ("1 second", Id::Interval(1000)),
            ("2 seconds", Id::Interval(2000)),
            ("5 seconds", Id::Interval(5000)),
        ],
        selected,
    });

    let screens = unsafe { super::screens() };
    rows.push(Row::Section("SCREEN"));
    if screens.is_empty() {
        rows.push(Row::Note("No taskbar found.".to_string()));
    } else {
        let current = settings::screen();
        let selected = if screens.iter().any(|(number, _)| *number == current) {
            current
        } else {
            screens
                .iter()
                .find(|(_, primary)| *primary)
                .map(|(number, _)| *number)
                .unwrap_or(0)
        };
        for (number, primary) in screens {
            rows.push(Row::Radio {
                id: Id::Screen(number),
                label: format!("Screen {number}"),
                value: primary.then(|| "primary".to_string()),
                selected: number == selected,
            });
        }
    }

    rows.push(Row::Section("POSITION"));
    rows.push(Row::Slider {
        id: Id::Offset,
        label: "Offset".to_string(),
        value: settings::widget_offset(),
        min: -800,
        max: 800,
        step: 16,
        unit: " px",
    });
    let offset = settings::widget_offset();
    rows.push(Row::Action {
        id: Id::OffsetReset,
        label: "Reset offset".to_string(),
        danger: false,
        enabled: offset != 0,
    });

    rows
}

fn alerts_page() -> Vec<Row> {
    vec![
        Row::Section("LOW BATTERY"),
        Row::Toggle {
            id: Id::LowBattery,
            label: "Low battery alerts".to_string(),
            value: None,
            checked: settings::alerts_enabled(),
            enabled: true,
        },
        Row::Section("TEMPERATURE ALERTS"),
        Row::Slider {
            id: Id::CpuThreshold,
            label: "CPU alert threshold".to_string(),
            value: settings::cpu_alert_threshold() as i32,
            min: 60,
            max: 95,
            step: 5,
            unit: " \u{b0}C",
        },
        Row::Slider {
            id: Id::GpuThreshold,
            label: "GPU alert threshold".to_string(),
            value: settings::gpu_alert_threshold() as i32,
            min: 60,
            max: 95,
            step: 5,
            unit: " \u{b0}C",
        },
        Row::Note(
            "Alerts fire once when the reading crosses the threshold, and re-arm after it drops 5 \u{b0}C."
                .to_string(),
        ),
    ]
}

fn devices_page() -> Vec<Row> {
    let hidden = settings::hidden_devices();
    let devices = readings();
    let mut rows = vec![Row::Section("SHOWN ON THE TASKBAR")];
    if devices.is_empty() {
        rows.push(Row::Note("No devices found.".to_string()));
        return rows;
    }
    for device in devices {
        rows.push(Row::Toggle {
            id: Id::Device(device.id.clone()),
            label: device.name,
            value: Some(device.status.to_string()),
            checked: !hidden.contains(&device.id),
            enabled: true,
        });
    }
    rows
}

fn audio_page(capturing_shortcut: bool) -> Vec<Row> {
    let state = crate::audio::engine().map(|e| e.snapshot());
    let mut rows = vec![Row::Section("AUDIO PANEL")];
    rows.push(Row::Action {
        id: Id::AudioPanel,
        label: "Open audio panel".to_string(),
        danger: false,
        enabled: state.is_some(),
    });
    let Some(state) = state else {
        rows.push(Row::Note("The audio engine is not running.".to_string()));
        return rows;
    };
    let mic = state.device(&state.roles.default_input_id);
    rows.push(Row::Section("QUICK CONTROLS"));
    rows.push(Row::Toggle {
        id: Id::AudioMic,
        label: "Mute microphone".to_string(),
        value: mic.map(|d| d.name.clone()),
        checked: mic.is_some_and(|d| d.muted),
        enabled: mic.is_some(),
    });
    rows.push(Row::Toggle {
        id: Id::ShowMic,
        label: "Show microphone in the widget".to_string(),
        value: None,
        checked: settings::show_mic(),
        enabled: true,
    });
    rows.push(Row::Action {
        id: Id::MuteShortcut,
        label: if capturing_shortcut {
            "Press a key... (Esc removes)".to_string()
        } else {
            format!(
                "Mute shortcut: {}",
                crate::hotkey::shortcut_label(settings::mute_shortcut())
            )
        },
        danger: false,
        enabled: true,
    });
    rows.push(Row::Note(
        "System-wide. The same key as Discord's mute toggles both; clicking the mic sends it too."
            .to_string(),
    ));
    rows.push(Row::Toggle {
        id: Id::AudioLock,
        label: "Lock default devices".to_string(),
        value: None,
        checked: state.lock_enabled,
        enabled: true,
    });
    rows.push(Row::Toggle {
        id: Id::AudioSeparate,
        label: "Separate communications device".to_string(),
        value: None,
        checked: state.separate_comms_role,
        enabled: true,
    });
    rows.push(Row::Toggle {
        id: Id::AudioAutoEnh,
        label: "Auto-disable audio enhancements".to_string(),
        value: None,
        checked: state.auto_disable_enhancements,
        enabled: true,
    });
    rows.push(Row::Toggle {
        id: Id::AudioAutoUnused,
        label: "Auto-disable unused devices".to_string(),
        value: None,
        checked: state.auto_disable_unused_devices,
        enabled: true,
    });
    rows.push(Row::Action {
        id: Id::AudioReenableAll,
        label: "Re-enable all audio devices".to_string(),
        danger: false,
        enabled: true,
    });
    rows.push(Row::Action {
        id: Id::AudioRestartService,
        label: "Restart Windows audio service".to_string(),
        danger: false,
        enabled: true,
    });
    rows.push(Row::Note(
        "Volumes, profiles, pins, listen routing and per-device controls are in the audio panel."
            .to_string(),
    ));
    rows
}

fn general_page() -> Vec<Row> {
    let mut rows = vec![
        Row::Section("STARTUP"),
        Row::Toggle {
            id: Id::Autostart,
            label: "Start with Windows".to_string(),
            value: None,
            checked: settings::autostart_enabled(),
            enabled: true,
        },
        Row::Section("UPDATES"),
        Row::Action {
            id: Id::Update,
            label: match update::available() {
                Some(version) => format!("Update to v{version}"),
                None => "Check for updates".to_string(),
            },
            danger: false,
            enabled: true,
        },
        Row::Section("TOOLS"),
        Row::Action {
            id: Id::Refresh,
            label: "Refresh devices now".to_string(),
            danger: false,
            enabled: true,
        },
        Row::Action {
            id: Id::History,
            label: "Show usage history graph".to_string(),
            danger: false,
            enabled: true,
        },
        Row::Action {
            id: Id::Diagnostics,
            label: "Copy diagnostics".to_string(),
            danger: false,
            enabled: true,
        },
        Row::Action {
            id: Id::Probe,
            label: "Probe an unsupported device (60 s)".to_string(),
            danger: false,
            enabled: !menu::probing(),
        },
    ];
    if sys_snapshot().is_some_and(|s| s.cpu_temp.is_none())
        && settings::sensor_mask() & sysmonitor::BIT_CPU_TEMP != 0
    {
        rows.push(Row::Action {
            id: Id::Pawnio,
            label: "Install CPU temperature driver".to_string(),
            danger: false,
            enabled: true,
        });
    }
    rows.push(Row::Section("APP"));
    rows.push(Row::Action {
        id: Id::ClassicMenu,
        label: "Classic menu".to_string(),
        danger: false,
        enabled: true,
    });
    rows.push(Row::Action {
        id: Id::Exit,
        label: "Exit SETTMetter".to_string(),
        danger: true,
        enabled: true,
    });
    rows.push(Row::Note(format!(
        "SETTMetter {} \u{b7} {}",
        env!("CARGO_PKG_VERSION"),
        settings::windows_version()
    )));
    rows
}

// ---------------------------------------------------------------- interaction

impl Data {
    fn apply_slider(&mut self, id: &Id, x: i32) {
        for row in &self.rows {
            let RowLayout::Slider {
                id: row_id,
                track,
                min,
                max,
                step,
                ..
            } = row
            else {
                continue;
            };
            if row_id != id {
                continue;
            }
            let value = slider_value(x, *track, *min, *max, *step);
            match id {
                Id::CpuThreshold => {
                    if settings::cpu_alert_threshold() != value as u32 {
                        settings::set_cpu_alert_threshold(value as u32);
                    }
                }
                Id::GpuThreshold => {
                    if settings::gpu_alert_threshold() != value as u32 {
                        settings::set_gpu_alert_threshold(value as u32);
                    }
                }
                Id::Offset if settings::widget_offset() != value => {
                    settings::set_widget_offset(value);
                    unsafe { super::place() };
                }
                _ => {}
            }
            self.relayout();
            return;
        }
    }
}

fn slider_value(x: i32, track: RectPx, min: i32, max: i32, step: i32) -> i32 {
    let width = track.width().max(1);
    let t = f32::from((x - track.left).clamp(0, width) as i16) / width as f32;
    let raw = min as f32 + t * (max - min) as f32;
    let steps = ((raw - min as f32) / step as f32).round() as i32;
    (min + steps * step).clamp(min, max)
}

fn activate(data: &mut Data, id: Id) -> bool {
    let controller = data.controller;
    if id != Id::PowerOff {
        data.quit_armed = false;
    }
    match id {
        Id::Close => return true,
        Id::PowerOff => {
            if data.quit_armed {
                post(controller, WM_APP_EXIT);
                return true;
            }
            data.quit_armed = true;
        }
        Id::Page(page) => {
            if data.page != page {
                data.page = page;
                data.scroll = 0;
                data.hover = None;
                data.relayout();
            }
        }
        Id::Preset(index) => {
            menu::apply_preset(index);
            unsafe { redraw() };
        }
        Id::Sensor(index) => {
            if let Some((_, bit)) = sysmonitor::SENSOR_NAMES.get(index) {
                settings::toggle_sensor(*bit);
                unsafe { redraw() };
            }
        }
        Id::GpuUsage(i) => {
            settings::toggle_gpu_sensor(&format!("gpu{i}"));
            unsafe { redraw() };
        }
        Id::GpuTemp(i) => {
            settings::toggle_gpu_sensor(&format!("gputemp{i}"));
            unsafe { redraw() };
        }
        Id::GpuVram(i) => {
            settings::toggle_gpu_sensor(&format!("vram{i}"));
            unsafe { redraw() };
        }
        Id::Icon(i) => {
            if let Some((_, bit)) = sysmonitor::ICON_NAMES.get(i) {
                settings::toggle_icon(*bit);
                unsafe { redraw() };
            }
        }
        Id::Interval(ms) => {
            settings::set_refresh_interval(ms);
            unsafe { redraw() };
        }
        Id::Screen(number) => unsafe { super::select_screen(number) },
        Id::OffsetReset => {
            settings::set_widget_offset(0);
            unsafe { super::place() };
        }
        Id::Offset | Id::CpuThreshold | Id::GpuThreshold => {}
        Id::LowBattery => settings::set_alerts(!settings::alerts_enabled()),
        Id::Autostart => settings::set_autostart(!settings::autostart_enabled()),
        Id::Refresh => wake(),
        Id::History => crate::history::toggle_open(),
        Id::Update => super::update_action(controller as usize),
        Id::Pawnio => super::install_pawnio_driver(controller as usize),
        Id::Diagnostics => {
            unsafe { menu::copy_diagnostics(controller as *mut c_void) };
            return true;
        }
        Id::Probe => menu::start_probe(),
        Id::AudioPanel => {
            super::open_flyout();
            return true;
        }
        Id::ShowMic => {
            settings::set_show_mic(!settings::show_mic());
            unsafe { redraw() };
        }
        Id::MuteShortcut => {
            data.capturing_shortcut = true;
        }
        Id::AudioMic => {
            if let Some(engine) = crate::audio::engine() {
                engine.toggle_active_mics_mute();
            }
            unsafe { redraw() };
        }
        Id::AudioLock | Id::AudioSeparate | Id::AudioAutoEnh | Id::AudioAutoUnused => {
            let Some(engine) = crate::audio::engine() else {
                return false;
            };
            let state = engine.snapshot();
            let action = match id {
                Id::AudioLock => AppAction::SetLockEnabled(!state.lock_enabled),
                Id::AudioSeparate => AppAction::SetSeparateCommsRole(!state.separate_comms_role),
                Id::AudioAutoEnh => {
                    AppAction::SetAutoDisableEnhancements(!state.auto_disable_enhancements)
                }
                _ => AppAction::SetAutoDisableUnusedDevices(!state.auto_disable_unused_devices),
            };
            engine.apply_action(action);
            unsafe { redraw() };
        }
        Id::AudioRestartService => crate::audio::restart_audio_service(),
        Id::AudioReenableAll => {
            if let Some(engine) = crate::audio::engine() {
                engine.apply_action(AppAction::ReenableAllDevices);
            }
            unsafe { redraw() };
        }
        Id::Device(device_id) => {
            settings::toggle_hidden(&device_id);
            unsafe { redraw() };
        }
        Id::ClassicMenu => {
            post(controller, WM_APP_MENU);
            return true;
        }
        Id::Exit => {
            post(controller, WM_APP_EXIT);
            return true;
        }
    }
    data.relayout();
    false
}

// ---------------------------------------------------------------- painting

fn paint(hwnd: HWND, data: &mut Data) {
    data.relayout();

    let mut ps = PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    if hdc.is_invalid() || data.width <= 0 || data.height <= 0 {
        unsafe {
            let _ = EndPaint(hwnd, &ps);
        }
        return;
    }
    if !data.backbuffer.ensure(hdc, data.width, data.height) {
        unsafe {
            let _ = EndPaint(hwnd, &ps);
        }
        return;
    }

    let (width, height) = (data.width, data.height);
    let mem_dc = data.backbuffer.mem_dc;
    let pixels_ptr = data.backbuffer.pixels.cast::<u8>();
    let byte_len = (width as usize) * (height as usize) * 4;
    let theme = data.theme;

    unsafe {
        let _ = winaudio::Win32::Graphics::Gdi::GdiFlush();
    }
    {
        let pixels = unsafe { std::slice::from_raw_parts_mut(pixels_ptr, byte_len) };
        let mut canvas = Canvas::new(pixels, width, height);
        paint_shapes(&mut canvas, data, &theme);
    }
    paint_text(mem_dc, data, &theme);

    unsafe {
        let _ = BitBlt(hdc, 0, 0, width, height, Some(mem_dc), 0, 0, SRCCOPY);
        let _ = EndPaint(hwnd, &ps);
    }
}

fn paint_shapes(canvas: &mut Canvas<'_>, data: &Data, theme: &Theme) {
    let s = data.scale;
    let bw = s.max(1.0);
    canvas.fill_all(theme.bg);
    if !data.dwm_border_ok {
        canvas.stroke_rect_1px(RectPx::new(0, 0, data.width, data.height), theme.border_strong);
    }

    // Tabs: text, a shared hairline, and an underline on the active one.
    if let (Some(first), Some(last)) = (data.tabs.first(), data.tabs.last()) {
        canvas.hline(
            first.1.left,
            last.1.right,
            first.1.bottom,
            px(1.0, s).max(1),
            theme.divider,
        );
    }
    for (page, rect) in &data.tabs {
        if data.page == *page {
            canvas.fill_rect(
                RectPx::new(
                    rect.left + px(10.0, s),
                    rect.bottom - px(2.0, s),
                    rect.right - px(10.0, s),
                    rect.bottom,
                ),
                theme.text,
            );
        }
    }

    // Close button
    if data.hover_close {
        canvas.fill_round_rect(data.close_btn, 6.0 * s, Some(theme.bg_hover), None);
    }
    canvas.draw_icon_centered(
        IconKind::CloseX,
        data.close_btn,
        11.0 * s,
        if data.hover_close { theme.text } else { theme.text_sec },
    );

    // Power: exits SETTMetter. The first click arms it, the second one quits.
    let power_hover = data.hover == Some(Id::PowerOff);
    let power_fill = if data.quit_armed {
        Some(theme.inv_bg)
    } else if power_hover {
        Some(theme.bg_hover)
    } else {
        None
    };
    if let Some(fill) = power_fill {
        canvas.fill_round_rect(data.power_btn, 6.0 * s, Some(fill), None);
    }
    canvas.draw_icon_centered(
        IconKind::Power,
        data.power_btn,
        12.0 * s,
        if data.quit_armed {
            theme.inv_text
        } else if power_hover {
            theme.text
        } else {
            theme.text_sec
        },
    );

    // Content, clipped so scrolled rows never paint over the header.
    canvas.push_clip(RectPx::new(0, data.content.top - px(4.0, s), data.width, data.height));
    for row in &data.rows {
        match row {
            RowLayout::Section { rect, .. } => {
                canvas.hline(
                    rect.left + px(8.0, s),
                    rect.right - px(8.0, s),
                    rect.bottom - px(1.0, s).max(1),
                    px(1.0, s).max(1),
                    theme.divider,
                );
            }
            RowLayout::Note { .. } => {}
            RowLayout::Toggle {
                id,
                rect,
                box_rect,
                checked,
                radio,
                enabled,
                ..
            } => {
                let hovered = data.hover.as_ref() == Some(id) && *enabled;
                if hovered {
                    canvas.fill_round_rect(*rect, 7.0 * s, Some(theme.bg_hover), None);
                }
                let radius = box_rect.height() as f32 * 0.5;
                if *radio {
                    let border = if *checked {
                        theme.text
                    } else if *enabled {
                        theme.check_border
                    } else {
                        theme.border
                    };
                    canvas.fill_round_rect(
                        *box_rect,
                        radius,
                        Some(theme.bg_recess),
                        Some((border, bw)),
                    );
                    if *checked {
                        let dot = px(6.0, s);
                        let inner = RectPx::from_xywh(
                            box_rect.left + (box_rect.width() - dot) / 2,
                            box_rect.top + (box_rect.height() - dot) / 2,
                            dot,
                            dot,
                        );
                        canvas.fill_round_rect(inner, dot as f32 * 0.5, Some(theme.inv_bg), None);
                    }
                } else {
                    let (fill, border, icon) = if *checked {
                        (Some(theme.inv_bg), theme.inv_bg, Some(theme.inv_text))
                    } else if *enabled {
                        (None, theme.check_border, None)
                    } else {
                        (None, theme.border, None)
                    };
                    canvas.fill_round_rect(*box_rect, 4.0 * s, fill, Some((border, bw)));
                    if let Some(color) = icon {
                        canvas.draw_icon_centered(IconKind::Check, *box_rect, 10.0 * s, color);
                    }
                }
            }
            RowLayout::Pill { rect, options } => {
                canvas.fill_round_rect(*rect, 8.0 * s, Some(theme.bg_recess), None);
                for (_, option, _, selected) in options {
                    if *selected {
                        canvas.fill_round_rect(*option, 5.0 * s, Some(theme.inv_bg), None);
                    }
                }
            }
            RowLayout::Slider {
                id,
                track,
                value,
                min,
                max,
                ..
            } => {
                let t = if *max > *min {
                    (*value - *min) as f32 / (*max - *min) as f32
                } else {
                    0.0
                };
                let knob = px(14.0, s);
                let track_r = track.height() as f32 * 0.5;
                canvas.fill_round_rect(*track, track_r, Some(theme.bg_recess), None);
                let filled_w = ((track.width() as f32) * t.clamp(0.0, 1.0)) as i32;
                if filled_w > 0 {
                    canvas.fill_round_rect(
                        RectPx::from_xywh(track.left, track.top, filled_w, track.height()),
                        track_r,
                        Some(theme.text_muted),
                        None,
                    );
                }
                let center = track.left + filled_w;
                let knob_rect = RectPx::from_xywh(
                    center - knob / 2,
                    track.top + (track.height() - knob) / 2,
                    knob,
                    knob,
                );
                canvas.fill_round_rect(knob_rect, knob as f32 * 0.5, Some(theme.inv_bg), None);
                let _ = id;
            }
            RowLayout::Action {
                id,
                rect,
                danger,
                enabled,
                ..
            } => {
                let hovered = data.hover.as_ref() == Some(id) && *enabled;
                if hovered {
                    let fill = if *danger { theme.inv_bg } else { theme.bg_hover };
                    canvas.fill_round_rect(*rect, 7.0 * s, Some(fill), None);
                }
            }
            RowLayout::IconCheck {
                id,
                rect,
                box_rect,
                checked,
                ..
            } => {
                let hovered = data.hover.as_ref() == Some(id);
                if hovered {
                    canvas.fill_round_rect(*rect, 7.0 * s, Some(theme.bg_hover), None);
                }
                let (fill, border, icon) = if *checked {
                    (Some(theme.inv_bg), theme.inv_bg, Some(theme.inv_text))
                } else {
                    (None, theme.check_border, None)
                };
                canvas.fill_round_rect(*box_rect, 4.0 * s, fill, Some((border, bw)));
                if let Some(color) = icon {
                    canvas.draw_icon_centered(IconKind::Check, *box_rect, 10.0 * s, color);
                }
            }
        }
    }
    canvas.pop_clip();

    if data.max_scroll > 0 {
        let bar_w = px(4.0, s);
        let track = RectPx::new(
            data.content.right + px(2.0, s),
            data.content.top,
            data.content.right + px(2.0, s) + bar_w,
            data.content.bottom,
        );
        canvas.fill_round_rect(track, bar_w as f32 * 0.5, Some(theme.bg_recess), None);
        let visible = data.content.height() as f32 / (data.content.height() + data.max_scroll) as f32;
        let thumb_h = ((track.height() as f32) * visible).max(px(24.0, s) as f32) as i32;
        let span = (track.height() - thumb_h).max(0);
        let pos = if data.max_scroll > 0 {
            (span as f32 * (data.scroll as f32 / data.max_scroll as f32)) as i32
        } else {
            0
        };
        canvas.fill_round_rect(
            RectPx::from_xywh(track.left, track.top + pos, bar_w, thumb_h),
            bar_w as f32 * 0.5,
            Some(theme.text_muted),
            None,
        );
    }
}


fn paint_text(hdc: HDC, data: &Data, theme: &Theme) {
    let s = data.scale;
    let fonts = &data.fonts;

    draw_text(
        hdc,
        fonts.get(FontRole::Title14Semi),
        "SETTMetter",
        RectPx::from_xywh(px(14.0, s), px(10.0, s), px(200.0, s), px(24.0, s)),
        theme.text,
        TextAlign::Left,
        false,
    );

    for (page, rect) in &data.tabs {
        let selected = data.page == *page;
        let hovered = data.hover_tab == Some(*page);
        if let Some((label, _)) = PAGES.iter().find(|(_, p)| p == page) {
            let text_color = if selected {
                theme.text
            } else if hovered {
                theme.text_sec
            } else {
                theme.text_muted
            };
            draw_text(
                hdc,
                fonts.get(if selected {
                    FontRole::Body11_5Semi
                } else {
                    FontRole::Body11_5Med
                }),
                label,
                RectPx::new(rect.left, rect.top, rect.right, rect.bottom - px(4.0, s)),
                text_color,
                TextAlign::Center,
                false,
            );
        }
    }

    let saved = unsafe { SaveDC(hdc) };
    unsafe {
        IntersectClipRect(
            hdc,
            data.content.left,
            data.content.top,
            data.content.right,
            data.content.bottom,
        );
    }

    for row in &data.rows {
        match row {
            RowLayout::Section { rect, label } => {
                if !row_intersects(data, *rect) {
                    continue;
                }
                draw_text(
                    hdc,
                    fonts.get(FontRole::Sub10_5Semi),
                    label,
                    RectPx::new(
                        rect.left + px(8.0, s),
                        rect.top + px(8.0, s),
                        rect.right,
                        rect.bottom,
                    ),
                    theme.text_sec,
                    TextAlign::Left,
                    false,
                );
            }
            RowLayout::Toggle {
                label,
                value,
                text,
                value_text,
                enabled,
                rect,
                ..
            } => {
                if !row_intersects(data, *rect) {
                    continue;
                }
                draw_text(
                    hdc,
                    fonts.get(FontRole::Body11_5Reg),
                    text,
                    *label,
                    if *enabled { theme.text } else { theme.text_muted },
                    TextAlign::Left,
                    true,
                );
                if let (Some(rect), Some(value)) = (value, value_text) {
                    draw_text(
                        hdc,
                        fonts.get(FontRole::Body11_5Semi),
                        value,
                        *rect,
                        if *enabled { theme.text_sec } else { theme.text_muted },
                        TextAlign::Right,
                        true,
                    );
                }
            }
            RowLayout::Pill { options, .. } => {
                for (_, option, label, selected) in options {
                    draw_text(
                        hdc,
                        fonts.get(if *selected {
                            FontRole::Body11_5Semi
                        } else {
                            FontRole::Body11_5Med
                        }),
                        label,
                        *option,
                        if *selected { theme.inv_text } else { theme.text_sec },
                        TextAlign::Center,
                        true,
                    );
                }
            }
            RowLayout::Slider {
                rect,
                label,
                value_rect,
                text,
                value,
                unit,
                ..
            } => {
                if !row_intersects(data, *rect) {
                    continue;
                }
                draw_text(
                    hdc,
                    fonts.get(FontRole::Body11_5Reg),
                    text,
                    *label,
                    theme.text,
                    TextAlign::Left,
                    true,
                );
                draw_text(
                    hdc,
                    fonts.get(FontRole::Body11_5Semi),
                    &format!("{value}{unit}"),
                    *value_rect,
                    theme.text_sec,
                    TextAlign::Right,
                    false,
                );
            }
            RowLayout::IconCheck {
                rect,
                glyph_rect,
                label_rect,
                glyph,
                text,
                ..
            } => {
                if !row_intersects(data, *rect) {
                    continue;
                }
                draw_text(
                    hdc,
                    data.icon_font,
                    glyph,
                    *glyph_rect,
                    theme.text_sec,
                    TextAlign::Center,
                    false,
                );
                draw_text(
                    hdc,
                    fonts.get(FontRole::Body11_5Reg),
                    text,
                    *label_rect,
                    theme.text,
                    TextAlign::Left,
                    true,
                );
            }
            RowLayout::Action {
                label,
                text,
                danger,
                enabled,
                rect,
                id,
            } => {
                if !row_intersects(data, *rect) {
                    continue;
                }
                let hovered = data.hover.as_ref() == Some(id) && *enabled;
                let color = if !*enabled {
                    theme.text_muted
                } else if *danger && hovered {
                    theme.inv_text
                } else {
                    theme.text
                };
                draw_text(
                    hdc,
                    fonts.get(FontRole::Body11_5Med),
                    text,
                    *label,
                    color,
                    TextAlign::Left,
                    true,
                );
            }
            RowLayout::Note { rect, text } => {
                if !row_intersects(data, *rect) {
                    continue;
                }
                draw_wrapped_text(hdc, fonts.get(FontRole::Sub10_5Reg), text, *rect, theme.text_muted);
            }
        }
    }
    unsafe {
        let _ = RestoreDC(hdc, saved);
    }
}

fn row_intersects(data: &Data, rect: RectPx) -> bool {
    rect.bottom > data.content.top && rect.top < data.content.bottom
}

// ---------------------------------------------------------------- window plumbing

fn data_ptr(hwnd: HWND) -> *mut Data {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Data }
}

fn request_close(hwnd: HWND) {
    // Deferred: destroying here would free the WindowData while a handler still holds it
    // (opening the audio flyout takes the foreground, which sends us WM_ACTIVATE from inside
    // that same click).
    unsafe {
        let _ = winaudio::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        );
    }
}


unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = data_ptr(hwnd);
    match msg {
        WM_PAINT => {
            if !ptr.is_null() {
                let data = unsafe { &mut *ptr };
                paint(hwnd, data);
            }
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return LRESULT(0);
        }
        WM_ACTIVATE => {
            if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                request_close(hwnd);
            }
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            if ptr.is_null() {
                return LRESULT(0);
            }
            let data = unsafe { &mut *ptr };
            let (x, y) = lparam_point(lp);
            if !data.tracking {
                data.tracking = true;
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                unsafe {
                    let _ = TrackMouseEvent(&mut tme);
                }
            }
            let hit = data.hit(x, y);
            let mut repaint = false;
            if hit != data.hover {
                data.hover = hit.clone();
                repaint = true;
            }
            let tab = data
                .tabs
                .iter()
                .find(|(_, rect)| rect.contains(x, y))
                .map(|(page, _)| *page);
            if tab != data.hover_tab {
                data.hover_tab = tab;
                repaint = true;
            }
            let close = data.close_btn.contains(x, y);
            if close != data.hover_close {
                data.hover_close = close;
                repaint = true;
            }
            if data.drag.is_some()
                && let Some(id) = data.drag.clone()
            {
                data.apply_slider(&id, x);
                repaint = true;
            }
            if repaint {
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            return LRESULT(0);
        }
        WM_MOUSELEAVE => {
            if !ptr.is_null() {
                let data = unsafe { &mut *ptr };
                data.tracking = false;
                if data.hover.is_some() || data.hover_tab.is_some() || data.hover_close {
                    data.hover = None;
                    data.hover_tab = None;
                    data.hover_close = false;
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            if ptr.is_null() {
                return LRESULT(0);
            }
            let data = unsafe { &mut *ptr };
            let (x, y) = lparam_point(lp);
            if let Some(id) = data.hit(x, y) {
                let is_slider = data
                    .rows
                    .iter()
                    .any(|row| matches!(row, RowLayout::Slider { id: rid, .. } if *rid == id));
                if is_slider {
                    data.drag = Some(id.clone());
                    data.apply_slider(&id, x);
                    unsafe {
                        SetCapture(hwnd);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            if ptr.is_null() {
                return LRESULT(0);
            }
            let data = unsafe { &mut *ptr };
            let (x, y) = lparam_point(lp);
            if data.drag.take().is_some() {
                unsafe {
                    let _ = ReleaseCapture();
                }
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                return LRESULT(0);
            }
            if let Some(id) = data.hit(x, y)
                && activate(data, id)
            {
                request_close(hwnd);
                return LRESULT(0);
            }
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            return LRESULT(0);
        }
        WM_MOUSEWHEEL => {
            if ptr.is_null() {
                return LRESULT(0);
            }
            let data = unsafe { &mut *ptr };
            let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as i32;
            let step = px(72.0, data.scale) * if delta > 0 { -1 } else { 1 };
            data.scroll_by(step);
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            return LRESULT(0);
        }
        WM_KEYDOWN => {
            let vk = wp.0 as u16;
            if !ptr.is_null() {
                let data = unsafe { &mut *ptr };
                if data.capturing_shortcut {
                    if vk == VK_ESCAPE.0 {
                        // Esc removes the binding instead of closing the window.
                        settings::set_mute_shortcut(None);
                        crate::hotkey::uninstall();
                        data.capturing_shortcut = false;
                    } else if !crate::hotkey::is_modifier(vk) {
                        let mods = crate::hotkey::current_modifiers();
                        settings::set_mute_shortcut(Some((vk as u32, mods)));
                        crate::hotkey::install(data.controller);
                        data.capturing_shortcut = false;
                    }
                    data.relayout();
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    return LRESULT(0);
                }
            }
            if vk == VK_ESCAPE.0 {
                request_close(hwnd);
            }
            let _ = WHEEL_DELTA;
            return LRESULT(0);
        }
        WM_SETCURSOR => {
            if (lp.0 & 0xFFFF) as u32 == HTCLIENT {
                let cursor = unsafe { LoadCursorW(None, IDC_HAND) }.unwrap_or_default();
                unsafe {
                    SetCursor(Some(cursor));
                }
                return LRESULT(1);
            }
        }
        WM_NCDESTROY => {
            if !ptr.is_null() {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(ptr));
                }
            }
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn lparam_point(lp: LPARAM) -> (i32, i32) {
    let x = (lp.0 & 0xFFFF) as i16 as i32;
    let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
    (x, y)
}

fn post(controller: isize, message: u32) {
    unsafe {
        let _ = winaudio::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(HWND(controller as *mut c_void)),
            message,
            WPARAM(0),
            LPARAM(0),
        );
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// A measure DC and the font set, so the tests exercise the real value-column widths.
    fn fonts() -> (HDC, FontCache) {
        (unsafe { CreateCompatibleDC(None) }, FontCache::new(1.0))
    }

    fn toggle(id: u8) -> Row {
        Row::Toggle {
            id: Id::Sensor(id as usize),
            label: "x".to_string(),
            value: None,
            checked: false,
            enabled: true,
        }
    }

    #[test]
    fn slider_value_quantizes_and_clamps() {
        let track = RectPx::from_xywh(0, 0, 100, 6);
        assert_eq!(slider_value(-40, track, 60, 95, 5), 60);
        assert_eq!(slider_value(0, track, 60, 95, 5), 60);
        assert_eq!(slider_value(50, track, 60, 95, 5), 80);
        assert_eq!(slider_value(100, track, 60, 95, 5), 95);
        assert_eq!(slider_value(500, track, 60, 95, 5), 95);

        let offset = RectPx::from_xywh(0, 0, 100, 6);
        assert_eq!(slider_value(0, offset, -800, 800, 16), -800);
        assert_eq!(slider_value(50, offset, -800, 800, 16), 0);
        assert_eq!(slider_value(100, offset, -800, 800, 16), 800);
    }

    #[test]
    fn layout_rows_stacks_rows_and_reports_total_height() {
        let content = RectPx::from_xywh(0, 10, 200, 100);
        let (dc, fonts) = fonts();
        let (rows, total) = layout_rows(vec![Row::Section("A"), toggle(0)], content, 1.0, 0, dc, &fonts);
        assert_eq!(total, 30 + 38 + 2);
        assert_eq!(rows.len(), 2);
        match &rows[1] {
            RowLayout::Toggle { rect, .. } => {
                assert_eq!(rect.top, 10 + 30);
                assert_eq!(rect.height(), 38);
            }
            _ => panic!("expected a toggle"),
        }
    }

    #[test]
    fn scrolling_shifts_rows_up() {
        let content = RectPx::from_xywh(0, 10, 200, 100);
        let (dc, fonts) = fonts();
        let (rows, _) = layout_rows(vec![toggle(0)], content, 1.0, 12, dc, &fonts);
        match &rows[0] {
            RowLayout::Toggle { rect, .. } => assert_eq!(rect.top, -2),
            _ => panic!("expected a toggle"),
        }
    }

    #[test]
    fn split_columns_start_together_and_take_the_taller_height() {
        let content = RectPx::from_xywh(0, 10, 400, 300);
        let (dc, fonts) = fonts();
        let split = Row::Split {
            left: vec![Row::Section("L"), toggle(0), toggle(1)],
            right: vec![Row::Section("R"), toggle(2)],
        };
        let (rows, total) = layout_rows(vec![split], content, 1.0, 0, dc, &fonts);
        // Left: section 32 + two 42px toggles with 4px gaps = 124. The split is that tall.
        assert_eq!(total, 124);
        let mut left_rows = 0;
        let mut right_rows = 0;
        for row in &rows {
            if let RowLayout::Toggle { rect, .. } = row {
                if rect.left == 0 {
                    left_rows += 1;
                } else if rect.left > 0 {
                    right_rows += 1;
                }
            }
        }
        assert_eq!(left_rows, 2);
        assert_eq!(right_rows, 1);
        match &rows[0] {
            RowLayout::Section { rect, .. } => {
                assert_eq!(rect.left, 0);
                assert_eq!(rect.width(), 194);
            }
            _ => panic!("expected the left section"),
        }
    }
}
