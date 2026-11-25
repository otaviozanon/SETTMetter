mod layout;

use std::cell::Cell;
use std::mem::size_of;
use std::rc::Rc;
use std::sync::Once;
use std::time::{Duration, Instant};

use crate::audio::model::AppAction;
use crate::audio::model::{AppState, DeviceFlow};
use winaudio::Win32::Foundation::{
    GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use winaudio::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, DeleteDC, EndPaint, GdiFlush, GetMonitorInfoW, HDC, HMONITOR, IntersectClipRect,
    InvalidateRect, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, PAINTSTRUCT, RestoreDC,
    SRCCOPY, SaveDC,
};
use winaudio::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use winaudio::Win32::System::LibraryLoader::GetModuleHandleW;
use winaudio::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use winaudio::Win32::UI::Controls::WM_MOUSELEAVE;
use winaudio::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use winaudio::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_BACK,
    VK_CONTROL, VK_DELETE, VK_ESCAPE, VK_LEFT, VK_MENU, VK_RETURN, VK_RIGHT,
};
use winaudio::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA,
    GetWindowLongPtrW, HTCLIENT, HWND_TOPMOST, IDC_ARROW, IDC_HAND, IDC_IBEAM, IDC_SIZEWE,
    KillTimer, LoadCursorW, RegisterClassW, SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER, SetCursor,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, WA_INACTIVE,
    WHEEL_DELTA, WM_ACTIVATE, WM_CAPTURECHANGED, WM_CHAR, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCDESTROY, WM_PAINT,
    WM_SETCURSOR, WM_TIMER, WNDCLASSW, WS_CLIPCHILDREN, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use winaudio::core::PCWSTR;

use crate::audio::AudioEngine;
use crate::flyout::layout::{
    ActiveInput, ActiveOverlay, ConfigOverlayLayout, ConfigRowKind, CursorKind, DeviceCardLayout,
    EDGE_MARGIN_LOGICAL, FLYOUT_HEIGHT_LOGICAL, FLYOUT_WIDTH_LOGICAL, FlyoutLayout, HitAction,
    LayoutInput, ListenOverlayLayout, OverlayLayout, ProfileOverlayLayout, TACTILE_PAD_LEFT_LOGICAL,
    TACTILE_PAD_RIGHT_LOGICAL,
};
use crate::ui::paint::{
    Canvas, FontCache, FontRole, IconKind, RectPx, TextAlign, draw_text, draw_wrapped_text,
    measure_text_width, px,
};
use crate::ui::theme::{self, Theme, ThemeMode};
use crate::audio::win32;
use crate::ui::{DibBackbuffer, apply_dwm_chrome};

const FLYOUT_CLASS_NAME: &str = "SETTMetterAudioFlyout";
const STARTUP_GRACE: Duration = Duration::from_millis(250);
const REOPEN_DEBOUNCE: Duration = Duration::from_millis(350);
const STATUS_TIMER_ID: usize = 1;
const STATUS_FLASH_MS: u32 = 2_000;
const MAX_PROFILE_NAME_LEN: usize = 36;
const MAX_SEARCH_QUERY_LEN: usize = 64;
const CF_UNICODETEXT: u32 = 13;

static REGISTER_CLASS: Once = Once::new();

pub struct FlyoutController {
    shared: Rc<FlyoutShared>,
}

struct FlyoutShared {
    audio: &'static AudioEngine,
    /// The widget's controller window, as an isize so this module keeps its own HWND type.
    controller: isize,
    active_hwnd: Cell<Option<HWND>>,
    last_closed_at: Cell<Option<Instant>>,
}

impl FlyoutShared {
    fn on_destroyed(&self, hwnd: HWND) {
        if self.active_hwnd.get() == Some(hwnd) {
            self.active_hwnd.set(None);
        }
        self.last_closed_at.set(Some(Instant::now()));
    }
}

impl FlyoutController {
    pub fn new(audio: &'static AudioEngine, controller: isize) -> Self {
        Self {
            shared: Rc::new(FlyoutShared {
                audio,
                controller,
                active_hwnd: Cell::new(None),
                last_closed_at: Cell::new(None),
            }),
        }
    }

    pub fn is_open(&self) -> bool {
        self.shared.active_hwnd.get().is_some()
    }

    pub fn closed_within(&self, duration: Duration) -> bool {
        self.shared
            .last_closed_at
            .get()
            .is_some_and(|at| at.elapsed() < duration)
    }

    pub fn open_or_toggle(&mut self, anchor: (i32, i32)) {
        if self.is_open() {
            self.close();
        } else if !self.closed_within(REOPEN_DEBOUNCE) {
            self.open(anchor);
        }
    }

    pub fn open(&mut self, anchor: (i32, i32)) {
        if let Some(hwnd) = self.shared.active_hwnd.get() {
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
            }
            self.invalidate();
            return;
        }

        register_window_class();

        let monitor = unsafe {
            MonitorFromPoint(
                POINT {
                    x: anchor.0,
                    y: anchor.1,
                },
                MONITOR_DEFAULTTOPRIMARY,
            )
        };
        let dpi = monitor_dpi(monitor);
        let scale = dpi as f32 / 96.0;
        let (x, y, w, h) = compute_window_rect(monitor, anchor, scale);

        let class_name = win32::to_wide(FLYOUT_CLASS_NAME);
        let hinstance = unsafe { GetModuleHandleW(None) }.ok();

        let Ok(hwnd) = (unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                PCWSTR(class_name.as_ptr()),
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
        }) else {
            return;
        };

        let actual_dpi = unsafe { GetDpiForWindow(hwnd) };
        let (final_scale, final_x, final_y, final_w, final_h) =
            if actual_dpi > 0 && actual_dpi != dpi {
                let s = actual_dpi as f32 / 96.0;
                let (nx, ny, nw, nh) = compute_window_rect(monitor, anchor, s);
                (s, nx, ny, nw, nh)
            } else {
                (scale, x, y, w, h)
            };

        let snapshot = self.shared.audio.snapshot();
        let theme = theme::current();
        let dwm_border_ok = apply_dwm_chrome(hwnd, theme.border_strong);

        let measure_dc = unsafe { CreateCompatibleDC(None) };
        let fonts = FontCache::new(final_scale);

        let mut data = Box::new(WindowData {
            shared: Rc::clone(&self.shared),
            scale: final_scale,
            width: final_w,
            height: final_h,
            fonts,
            measure_dc,
            backbuffer: DibBackbuffer::empty(),
            active_tab: DeviceFlow::Output,
            pinned: false,
            search_query: String::new(),
            listen_search_query: String::new(),
            active_input: ActiveInput::None,
            input_selected_all: false,
            pending_high_surrogate: None,
            active_overlay: ActiveOverlay::None,
            saving_profile: false,
            confirm_quit: false,
            profile_name_input: String::new(),
            status_text: "Auto-saved".to_string(),
            status_flash: false,
            device_scroll: 0,
            config_scroll: 0,
            profile_scroll: 0,
            listen_scroll: 0,
            dragging_volume: None,
            dragging_card: None,
            card_drop: None,
            hovered_action: None,
            hovered_cursor: CursorKind::Arrow,
            hovered_card_id: None,
            last_mouse: None,
            tracking_mouse: false,
            dwm_border_ok,
            theme,
            opened_at: Instant::now(),
            is_closing: false,
            layout: None,
        });
        data.recompute_layout(&snapshot);

        let ptr = Box::into_raw(data);
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
        }
        self.shared.active_hwnd.set(Some(hwnd));

        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                final_x,
                final_y,
                final_w,
                final_h,
                SWP_NOACTIVATE,
            );
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    pub fn close(&mut self) {
        if let Some(hwnd) = self.shared.active_hwnd.get() {
            request_close(hwnd);
        }
    }

    pub fn invalidate(&self) {
        let Some(hwnd) = self.shared.active_hwnd.get() else {
            return;
        };
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowData;
        if ptr.is_null() {
            return;
        }
        let (should_release_capture, should_repaint) = {
            let data = unsafe { &mut *ptr };
            let snapshot = data.shared.audio.snapshot();
            let drag_invalidated = data.dragging_volume.as_ref().is_some_and(|(drag_id, _)| {
                snapshot.device(drag_id).is_none_or(|d| d.disabled)
            });
            if drag_invalidated {
                data.dragging_volume = None;
            }
            let drag_fast_path = if let Some((drag_id, _)) = data.dragging_volume.as_ref()
                && let Some(layout) = data.layout.as_mut()
            {
                let mut other_changed = false;
                for card in &mut layout.cards {
                    if card.device_id == *drag_id {
                        continue;
                    }
                    if let Some(dev) = snapshot.device(&card.device_id) {
                        let vol = dev.volume.min(100);
                        if card.volume != vol || card.muted != dev.muted {
                            card.volume = vol;
                            card.muted = dev.muted;
                            other_changed = true;
                        }
                    }
                }
                Some(other_changed)
            } else {
                None
            };

            if let Some(other_changed) = drag_fast_path {
                (false, other_changed)
            } else {
                data.recompute_layout(&snapshot);
                (drag_invalidated, true)
            }
        };
        if should_release_capture {
            unsafe {
                let _ = ReleaseCapture();
            }
        }
        if should_repaint {
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
    }

}

impl Drop for FlyoutController {
    fn drop(&mut self) {
        self.close();
    }
}

enum PostMsgAction {
    None,
    Close,
    Capture,
    ReleaseCapture,
    CopyClipboard(String),
    PasteClipboard,
}


struct WindowData {
    shared: Rc<FlyoutShared>,
    scale: f32,
    width: i32,
    height: i32,
    fonts: FontCache,
    measure_dc: HDC,
    backbuffer: DibBackbuffer,
    active_tab: DeviceFlow,
    pinned: bool,
    search_query: String,
    listen_search_query: String,
    active_input: ActiveInput,
    input_selected_all: bool,
    pending_high_surrogate: Option<u16>,
    active_overlay: ActiveOverlay,
    saving_profile: bool,
    pub confirm_quit: bool,
    profile_name_input: String,
    status_text: String,
    status_flash: bool,
    device_scroll: i32,
    config_scroll: i32,
    profile_scroll: i32,
    listen_scroll: i32,
    dragging_volume: Option<(String, RectPx)>,
    /// Card being dragged for reordering: device id plus the mouse position where it started.
    dragging_card: Option<(String, i32, i32)>,
    /// Insertion index shown while dragging_card is live.
    card_drop: Option<usize>,
    hovered_action: Option<HitAction>,
    hovered_cursor: CursorKind,
    hovered_card_id: Option<String>,
    last_mouse: Option<(i32, i32)>,
    tracking_mouse: bool,
    dwm_border_ok: bool,
    theme: Theme,
    opened_at: Instant,
    is_closing: bool,
    layout: Option<FlyoutLayout>,
}

impl Drop for WindowData {
    fn drop(&mut self) {
        if !self.measure_dc.is_invalid() {
            unsafe {
                let _ = DeleteDC(self.measure_dc);
            }
        }
    }
}

impl WindowData {
    fn snapshot(&self) -> AppState {
        self.shared.audio.snapshot()
    }

    fn recompute_layout(&mut self, state: &AppState) {
        let input = LayoutInput {
            width: self.width,
            height: self.height,
            scale: self.scale,
            state,
            active_tab: self.active_tab,
            search_query: &self.search_query,
            listen_search_query: &self.listen_search_query,
            active_overlay: &self.active_overlay,
            saving_profile: self.saving_profile,
            confirm_quit: self.confirm_quit,
            device_scroll: self.device_scroll,
            config_scroll: self.config_scroll,
            profile_scroll: self.profile_scroll,
            listen_scroll: self.listen_scroll,
        };
        let layout = FlyoutLayout::compute(self.measure_dc, &self.fonts, &input);
        self.device_scroll = layout.list_scroll;
        match &layout.overlay {
            OverlayLayout::None => {}
            OverlayLayout::Config(c) => self.config_scroll = c.scroll,
            OverlayLayout::Profile(p) => self.profile_scroll = p.scroll,
            OverlayLayout::Listen(l) => self.listen_scroll = l.scroll,
        }
        if let Some((mx, my)) = self.last_mouse {
            if let Some(hit) = layout.hit_test(mx, my) {
                self.hovered_action = Some(hit.action.clone());
                self.hovered_cursor = hit.cursor;
            } else {
                self.hovered_action = None;
                self.hovered_cursor = CursorKind::Arrow;
            }
            self.hovered_card_id = layout.hovered_card_id(mx, my).map(str::to_string);
        }
        self.layout = Some(layout);
    }

    fn flash_status(&mut self, hwnd: HWND, msg: impl Into<String>) {
        self.status_text = msg.into();
        self.status_flash = true;
        unsafe {
            let _ = SetTimer(Some(hwnd), STATUS_TIMER_ID, STATUS_FLASH_MS, None);
        }
    }

    fn dispatch(&mut self, hwnd: HWND, action: AppAction, status: Option<String>) -> PostMsgAction {
        self.shared.audio.apply_action_async(action);
        if let Some(msg) = status {
            self.flash_status(hwnd, msg);
        }
        let snapshot = self.snapshot();
        self.recompute_layout(&snapshot);
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        PostMsgAction::None
    }

    fn refresh(&mut self, hwnd: HWND) {
        let snapshot = self.snapshot();
        self.recompute_layout(&snapshot);
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    fn device_name(&self, state: &AppState, device_id: &str) -> String {
        state
            .device(device_id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| device_id.to_string())
    }

    fn active_input_str(&self) -> &str {
        match self.active_input {
            ActiveInput::SaveProfile => &self.profile_name_input,
            ActiveInput::ListenSearch => &self.listen_search_query,
            ActiveInput::Search | ActiveInput::None => &self.search_query,
        }
    }

    fn active_input_mut(&mut self) -> &mut String {
        match self.active_input {
            ActiveInput::SaveProfile => &mut self.profile_name_input,
            ActiveInput::ListenSearch => &mut self.listen_search_query,
            ActiveInput::Search | ActiveInput::None => &mut self.search_query,
        }
    }

    fn active_input_max_len(&self) -> usize {
        match self.active_input {
            ActiveInput::SaveProfile => MAX_PROFILE_NAME_LEN,
            ActiveInput::Search | ActiveInput::ListenSearch | ActiveInput::None => {
                MAX_SEARCH_QUERY_LEN
            }
        }
    }

    fn reset_scroll_for_active_input(&mut self) {
        match self.active_input {
            ActiveInput::Search => self.device_scroll = 0,
            ActiveInput::ListenSearch => self.listen_scroll = 0,
            ActiveInput::SaveProfile | ActiveInput::None => {}
        }
    }

    fn update_volume_from_x(&mut self, hwnd: HWND, device_id: &str, bar_rect: RectPx, x: i32) {
        let next_volume = volume_from_bar_x(bar_rect, x, self.scale);

        let (prev_volume, was_muted) = if let Some(card) =
            self.layout.as_ref().and_then(|l| l.card(device_id))
        {
            if card.disabled {
                return;
            }
            (card.volume, card.muted)
        } else {
            let snapshot = self.snapshot();
            let Some(dev) = snapshot.device(device_id) else {
                return;
            };
            if dev.disabled {
                return;
            }
            (dev.volume.min(100), dev.muted)
        };

        let should_unmute = was_muted && next_volume > 0;
        let volume_changed = prev_volume != next_volume;
        if !should_unmute && !volume_changed {
            return;
        }
        if should_unmute {
            self.shared.audio.apply_action_async(AppAction::SetMute {
                device_id: device_id.to_string(),
                muted: false,
            });
        }
        if volume_changed {
            self.shared.audio.apply_action_async(AppAction::SetVolume {
                device_id: device_id.to_string(),
                volume: next_volume,
            });
        }

        let next_muted = was_muted && !should_unmute;
        if let Some(dirty) = self
            .layout
            .as_mut()
            .and_then(|l| l.update_card_volume(device_id, next_volume, next_muted))
        {
            if !dirty.is_empty() {
                let rect = dirty.to_win32();
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), Some(&rect), false);
                }
            }
        } else {
            self.refresh(hwnd);
        }
    }

    fn commit_save_profile(&mut self, hwnd: HWND) {
        let raw_name = std::mem::take(&mut self.profile_name_input);
        self.saving_profile = false;
        self.active_input = ActiveInput::None;
        self.input_selected_all = false;
        let snapshot = self.shared.audio.apply_action(AppAction::SaveProfile {
            name: raw_name.clone(),
        });
        let saved_name = snapshot
            .active_profile()
            .map(|p| p.name.clone())
            .unwrap_or(raw_name);
        self.flash_status(hwnd, format!("Saved profile '{saved_name}'"));
        self.recompute_layout(&snapshot);
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    fn cancel_save_profile(&mut self, hwnd: HWND) {
        self.saving_profile = false;
        self.profile_name_input.clear();
        self.input_selected_all = false;
        if self.active_input == ActiveInput::SaveProfile {
            self.active_input = ActiveInput::None;
        }
        self.refresh(hwnd);
    }

    fn close_listen_overlay(&mut self, hwnd: HWND) {
        self.listen_search_query.clear();
        self.listen_scroll = 0;
        self.input_selected_all = false;
        if self.active_input == ActiveInput::ListenSearch {
            self.active_input = ActiveInput::None;
        }
        self.active_overlay = ActiveOverlay::None;
        self.refresh(hwnd);
    }

    /// Device card under the pointer, when the click landed on the scrollable list itself.
    fn card_at(&self, x: i32, y: i32) -> Option<String> {
        if self.active_overlay != ActiveOverlay::None {
            return None;
        }
        let layout = self.layout.as_ref()?;
        if !layout.list_viewport.contains(x, y) {
            return None;
        }
        layout
            .cards
            .iter()
            .find(|card| card.card_rect.contains(x, y))
            .map(|card| card.device_id.clone())
    }

    /// Tracks the insertion marker while a card is being dragged. Returns whether to repaint.
    fn update_card_drag(&mut self, x: i32, y: i32) -> bool {
        let (dragged, x0, y0) = match self.dragging_card.as_ref() {
            Some((dragged, x0, y0)) => (dragged.clone(), *x0, *y0),
            None => return false,
        };
        // A few pixels of slop keeps a plain click from registering as a reorder.
        let slop = px(4.0, self.scale);
        if (x - x0).abs() < slop && (y - y0).abs() < slop {
            return false;
        }
        let next = self.drop_index(y, &dragged);
        if next == self.card_drop {
            return false;
        }
        self.card_drop = next;
        true
    }

    /// Insertion index for a pointer position, clamped to the dragged card's pin block so pinned
    /// cards always stay on top.
    fn drop_index(&self, y: i32, dragged_id: &str) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        if layout.cards.is_empty() {
            return None;
        }
        let pinned_count = layout.cards.iter().filter(|card| card.pinned).count();
        let dragged = layout
            .cards
            .iter()
            .position(|card| card.device_id == dragged_id)?;
        let (low, high) = if layout.cards[dragged].pinned {
            (0, pinned_count)
        } else {
            (pinned_count, layout.cards.len())
        };
        let mut index = layout.cards.len();
        for (i, card) in layout.cards.iter().enumerate() {
            if y < card.card_rect.top + card.card_rect.height() / 2 {
                index = i;
                break;
            }
        }
        Some(index.clamp(low, high))
    }

    /// Ends a card drag, sending the new order when the card actually moved.
    fn commit_card_drag(&mut self, hwnd: HWND) {
        let Some((dragged, _, _)) = self.dragging_card.take() else {
            return;
        };
        let Some(mut drop) = self.card_drop.take() else {
            return;
        };
        let Some(layout) = self.layout.as_ref() else {
            return;
        };
        let mut ids: Vec<String> = layout.cards.iter().map(|card| card.device_id.clone()).collect();
        let Some(from) = ids.iter().position(|id| *id == dragged) else {
            return;
        };
        ids.remove(from);
        if from < drop {
            drop -= 1;
        }
        let drop = drop.min(ids.len());
        if drop == from {
            return;
        }
        ids.insert(drop, dragged);
        let flow = self.active_tab;
        self.dispatch(
            hwnd,
            AppAction::ReorderDevices {
                flow,
                device_ids: ids,
            },
            Some("Order saved".to_string()),
        );
    }

    fn handle_click(&mut self, hwnd: HWND, x: i32, y: i32) -> PostMsgAction {
        let hit_action = self
            .layout
            .as_ref()
            .and_then(|l| l.hit_test(x, y))
            .map(|h| h.action.clone());

        let Some(action) = hit_action else {
            let mut dirty = false;
            if self.confirm_quit {
                self.confirm_quit = false;
                dirty = true;
            }
            if self.active_input == ActiveInput::Search {
                self.active_input = ActiveInput::None;
                self.input_selected_all = false;
                dirty = true;
            } else if self.input_selected_all {
                self.input_selected_all = false;
                dirty = true;
            }
            // Pressing the empty part of a card starts a reorder drag (search must be off, or
            // the visible list is only a subset).
            if self.search_query.is_empty()
                && self.active_input == ActiveInput::None
                && let Some(device_id) = self.card_at(x, y)
            {
                self.dragging_card = Some((device_id, x, y));
                self.card_drop = None;
                if dirty {
                    self.refresh(hwnd);
                }
                return PostMsgAction::Capture;
            }
            if dirty {
                self.refresh(hwnd);
            }
            return PostMsgAction::None;
        };

        let cleared_confirm_quit = !matches!(
            action,
            HitAction::ConfigQuit | HitAction::HeaderQuit
        ) && std::mem::replace(&mut self.confirm_quit, false);

        let state = self.snapshot();
        match action {
            HitAction::ToggleLock => {
                let next = !state.lock_enabled;
                let msg = if next { "Lock enabled" } else { "Lock paused" };
                self.dispatch(hwnd, AppAction::SetLockEnabled(next), Some(msg.to_string()))
            }
            HitAction::ToggleConfig => {
                self.saving_profile = false;
                self.listen_search_query.clear();
                self.active_input = ActiveInput::None;
                self.input_selected_all = false;
                self.config_scroll = 0;
                self.active_overlay = match self.active_overlay {
                    ActiveOverlay::Config => ActiveOverlay::None,
                    _ => ActiveOverlay::Config,
                };
                self.refresh(hwnd);
                PostMsgAction::None
            }
            HitAction::TogglePin => {
                self.pinned = !self.pinned;
                let msg = if self.pinned {
                    "Window pinned on top"
                } else {
                    "Window unpinned"
                };
                self.flash_status(hwnd, msg);
                self.refresh(hwnd);
                PostMsgAction::None
            }
            HitAction::ToggleProfileMenu => {
                self.saving_profile = false;
                self.listen_search_query.clear();
                self.active_input = ActiveInput::None;
                self.input_selected_all = false;
                self.profile_scroll = 0;
                self.active_overlay = match self.active_overlay {
                    ActiveOverlay::ProfileMenu => ActiveOverlay::None,
                    _ => ActiveOverlay::ProfileMenu,
                };
                self.refresh(hwnd);
                PostMsgAction::None
            }
            HitAction::ToggleSaveProfileBar => {
                self.active_overlay = ActiveOverlay::None;
                self.listen_search_query.clear();
                self.input_selected_all = false;
                self.saving_profile = !self.saving_profile;
                if self.saving_profile {
                    self.profile_name_input.clear();
                    self.active_input = ActiveInput::SaveProfile;
                } else if self.active_input == ActiveInput::SaveProfile {
                    self.active_input = ActiveInput::None;
                }
                self.refresh(hwnd);
                PostMsgAction::None
            }
            HitAction::FocusSaveProfileInput => {
                self.active_overlay = ActiveOverlay::None;
                self.listen_search_query.clear();
                self.active_input = ActiveInput::SaveProfile;
                self.input_selected_all = false;
                self.refresh(hwnd);
                PostMsgAction::None
            }
            HitAction::CommitSaveProfile => {
                self.commit_save_profile(hwnd);
                PostMsgAction::None
            }
            HitAction::CancelSaveProfile => {
                self.cancel_save_profile(hwnd);
                PostMsgAction::None
            }
            HitAction::SetTab(flow) => {
                if self.active_tab != flow {
                    self.active_tab = flow;
                    self.device_scroll = 0;
                    self.active_overlay = ActiveOverlay::None;
                    self.listen_search_query.clear();
                    self.input_selected_all = false;
                    if self.active_input == ActiveInput::ListenSearch {
                        self.active_input = ActiveInput::None;
                    }
                    self.refresh(hwnd);
                } else if cleared_confirm_quit {
                    self.refresh(hwnd);
                }
                PostMsgAction::None
            }
            HitAction::FocusSearch => {
                self.active_overlay = ActiveOverlay::None;
                self.listen_search_query.clear();
                self.active_input = ActiveInput::Search;
                self.input_selected_all = false;
                self.refresh(hwnd);
                PostMsgAction::None
            }
            HitAction::ClearSearch | HitAction::EmptyClearSearch => {
                self.search_query.clear();
                self.device_scroll = 0;
                self.active_input = ActiveInput::Search;
                self.input_selected_all = false;
                self.refresh(hwnd);
                PostMsgAction::None
            }
            HitAction::SetDefault { flow, device_id } => {
                let name = self.device_name(&state, &device_id);
                self.dispatch(
                    hwnd,
                    AppAction::SetDefault { flow, device_id },
                    Some(format!("Default: {name}")),
                )
            }
            HitAction::SetComms { flow, device_id } => {
                let name = self.device_name(&state, &device_id);
                self.dispatch(
                    hwnd,
                    AppAction::SetComms { flow, device_id },
                    Some(format!("Communications: {name}")),
                )
            }
            HitAction::VolumeBar(device_id) => {
                if state.device(&device_id).is_some_and(|d| d.disabled) {
                    if cleared_confirm_quit {
                        self.refresh(hwnd);
                    }
                    return PostMsgAction::None;
                }
                let Some(bar_rect) = self.layout.as_ref().and_then(|l| l.slider_rect(&device_id))
                else {
                    if cleared_confirm_quit {
                        self.refresh(hwnd);
                    }
                    return PostMsgAction::None;
                };
                let cleared_ui_focus = cleared_confirm_quit
                    || self.active_overlay != ActiveOverlay::None
                    || !self.listen_search_query.is_empty()
                    || self.input_selected_all
                    || matches!(
                        self.active_input,
                        ActiveInput::Search | ActiveInput::ListenSearch
                    );
                self.active_overlay = ActiveOverlay::None;
                self.listen_search_query.clear();
                self.input_selected_all = false;
                if matches!(
                    self.active_input,
                    ActiveInput::Search | ActiveInput::ListenSearch
                ) {
                    self.active_input = ActiveInput::None;
                }
                self.dragging_volume = Some((device_id.clone(), bar_rect));
                self.update_volume_from_x(hwnd, &device_id, bar_rect, x);
                if cleared_ui_focus {
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                PostMsgAction::Capture
            }
            HitAction::ToggleMute(device_id) => {
                let next_muted = !state.device(&device_id).is_some_and(|d| d.muted);
                let name = self.device_name(&state, &device_id);
                let verb = if next_muted { "Muted" } else { "Unmuted" };
                self.dispatch(
                    hwnd,
                    AppAction::SetMute {
                        device_id,
                        muted: next_muted,
                    },
                    Some(format!("{verb} {name}")),
                )
            }
            HitAction::ToggleEnhancements(device_id) => {
                let currently_disabled = state
                    .device(&device_id)
                    .is_some_and(|d| state.are_enhancements_disabled(d));
                let next_disabled = !currently_disabled;
                let msg = if next_disabled {
                    "Enhancements disabled"
                } else {
                    "Enhancements restored"
                };
                self.dispatch(
                    hwnd,
                    AppAction::SetEnhancementsDisabled {
                        device_id,
                        disabled: next_disabled,
                    },
                    Some(msg.to_string()),
                )
            }
            HitAction::ToggleDeviceDisabled(device_id) => {
                if state.auto_disable_unused_devices {
                    if cleared_confirm_quit {
                        self.refresh(hwnd);
                    }
                    return PostMsgAction::None;
                }
                let Some(dev) = state.device(&device_id) else {
                    if cleared_confirm_quit {
                        self.refresh(hwnd);
                    }
                    return PostMsgAction::None;
                };
                let next_disabled = !dev.disabled;
                if next_disabled
                    && state
                        .devices
                        .iter()
                        .filter(|d| d.flow == dev.flow && !d.disabled)
                        .count()
                        <= 1
                {
                    self.flash_status(hwnd, "Can't disable last active device");
                    self.refresh(hwnd);
                    return PostMsgAction::None;
                }
                let name = dev.name.clone();
                let msg = if next_disabled {
                    format!("Disabled {name}")
                } else {
                    format!("Enabled {name}")
                };
                self.dispatch(
                    hwnd,
                    AppAction::SetDeviceDisabled {
                        device_id,
                        disabled: next_disabled,
                    },
                    Some(msg),
                )
            }
            HitAction::ToggleDevicePin(device_id) => {
                let next_pinned = !state.device(&device_id).is_some_and(|d| d.pinned);
                let name = self.device_name(&state, &device_id);
                let verb = if next_pinned { "Pinned" } else { "Unpinned" };
                self.dispatch(
                    hwnd,
                    AppAction::SetDevicePinned {
                        device_id,
                        pinned: next_pinned,
                    },
                    Some(format!("{verb} {name}")),
                )
            }
            HitAction::ToggleListenEnabled(device_id) => {
                let current = state
                    .device(&device_id)
                    .and_then(|d| d.listen.as_ref())
                    .is_some_and(|l| l.enabled);
                let has_target = state
                    .device(&device_id)
                    .and_then(|d| d.listen.as_ref())
                    .is_some_and(|l| l.target_device_id.is_some());
                let next = !current;
                let msg = if next && !has_target {
                    None
                } else if next {
                    Some("Listen routing active".to_string())
                } else {
                    Some("Listen routing off".to_string())
                };
                self.dispatch(
                    hwnd,
                    AppAction::SetListenEnabled {
                        device_id,
                        enabled: next,
                    },
                    msg,
                )
            }
            HitAction::ToggleListenMenu(device_id) => {
                self.saving_profile = false;
                self.listen_search_query.clear();
                self.listen_scroll = 0;
                self.input_selected_all = false;
                let is_same = matches!(
                    &self.active_overlay,
                    ActiveOverlay::ListenTarget(open_id) if open_id == &device_id
                );
                if is_same {
                    self.active_overlay = ActiveOverlay::None;
                    self.active_input = ActiveInput::None;
                } else {
                    self.active_overlay = ActiveOverlay::ListenTarget(device_id);
                    self.active_input = ActiveInput::ListenSearch;
                }
                self.refresh(hwnd);
                PostMsgAction::None
            }
            HitAction::DismissOverlay => {
                if matches!(self.active_overlay, ActiveOverlay::ListenTarget(_)) {
                    self.close_listen_overlay(hwnd);
                } else {
                    self.active_overlay = ActiveOverlay::None;
                    self.input_selected_all = false;
                    self.refresh(hwnd);
                }
                PostMsgAction::None
            }
            HitAction::OverlaySurface => {
                if self.input_selected_all || cleared_confirm_quit {
                    self.input_selected_all = false;
                    self.refresh(hwnd);
                }
                PostMsgAction::None
            }
            HitAction::ConfigSeparateComms => {
                let next = !state.separate_comms_role;
                let msg = if next {
                    "Separate Default & Comms enabled"
                } else {
                    "Unified Default & Comms enabled"
                };
                self.dispatch(
                    hwnd,
                    AppAction::SetSeparateCommsRole(next),
                    Some(msg.to_string()),
                )
            }
            HitAction::ConfigAutoDisableEnhancements => {
                let next = !state.auto_disable_enhancements;
                let msg = if next {
                    "Auto-disable enhancements enabled"
                } else {
                    "Auto-disable enhancements disabled"
                };
                self.dispatch(
                    hwnd,
                    AppAction::SetAutoDisableEnhancements(next),
                    Some(msg.to_string()),
                )
            }
            HitAction::ConfigReenableEnhancements => self.dispatch(
                hwnd,
                AppAction::ReenableAllEnhancements,
                Some("Re-enabled enhancements".to_string()),
            ),
            HitAction::ConfigAutoDisableUnused => {
                let next = !state.auto_disable_unused_devices;
                let msg = if next {
                    "Auto-disable unused devices enabled"
                } else {
                    "Auto-disable unused devices disabled"
                };
                self.dispatch(
                    hwnd,
                    AppAction::SetAutoDisableUnusedDevices(next),
                    Some(msg.to_string()),
                )
            }
            HitAction::ConfigReenableDevices => self.dispatch(
                hwnd,
                AppAction::ReenableAllDevices,
                Some("Re-enabled all devices".to_string()),
            ),
            HitAction::ConfigRestartAudio => {
                crate::audio::restart_audio_service();
                self.flash_status(hwnd, "Restarting the audio service...");
                self.refresh(hwnd);
                PostMsgAction::None
            }
            HitAction::HeaderQuit | HitAction::ConfigQuit => {
                if self.confirm_quit {
                    self.confirm_quit = false;
                    unsafe { crate::widget::shutdown(self.shared.controller as *mut std::ffi::c_void) };
                    PostMsgAction::Close
                } else {
                    self.confirm_quit = true;
                    self.flash_status(hwnd, "Click again to exit");
                    self.refresh(hwnd);
                    PostMsgAction::None
                }
            }
            HitAction::ApplyProfile(profile_id) => {
                let Some(name) = state
                    .profiles
                    .iter()
                    .find(|p| p.id == profile_id)
                    .map(|p| p.name.clone())
                else {
                    if cleared_confirm_quit {
                        self.refresh(hwnd);
                    }
                    return PostMsgAction::None;
                };
                self.active_overlay = ActiveOverlay::None;
                self.input_selected_all = false;
                self.dispatch(
                    hwnd,
                    AppAction::ApplyProfile { profile_id },
                    Some(format!("Loaded {name}")),
                )
            }
            HitAction::DeleteProfile(profile_id) => {
                let Some(name) = state
                    .profiles
                    .iter()
                    .find(|p| p.id == profile_id)
                    .map(|p| p.name.clone())
                else {
                    if cleared_confirm_quit {
                        self.refresh(hwnd);
                    }
                    return PostMsgAction::None;
                };
                self.dispatch(
                    hwnd,
                    AppAction::DeleteProfile { profile_id },
                    Some(format!("Deleted profile '{name}'")),
                )
            }
            HitAction::FocusListenSearch => {
                self.active_input = ActiveInput::ListenSearch;
                self.input_selected_all = false;
                self.refresh(hwnd);
                PostMsgAction::None
            }
            HitAction::ClearListenSearch => {
                self.listen_search_query.clear();
                self.listen_scroll = 0;
                self.active_input = ActiveInput::ListenSearch;
                self.input_selected_all = false;
                self.refresh(hwnd);
                PostMsgAction::None
            }
            HitAction::SetListenTarget {
                device_id,
                target_id,
            } => {
                let msg = match &target_id {
                    Some(tid) => format!("Routing to {}", state.listen_target_label(Some(tid))),
                    None => "Listen target cleared".to_string(),
                };
                self.listen_search_query.clear();
                self.listen_scroll = 0;
                self.input_selected_all = false;
                if self.active_input == ActiveInput::ListenSearch {
                    self.active_input = ActiveInput::None;
                }
                self.active_overlay = ActiveOverlay::None;
                self.dispatch(
                    hwnd,
                    AppAction::SetListenTarget {
                        device_id,
                        target_device_id: target_id,
                    },
                    Some(msg),
                )
            }
        }
    }

    fn apply_pasted_text(&mut self, hwnd: HWND, pasted: &str) {
        if self.active_input == ActiveInput::None {
            return;
        }
        let cleaned: String = pasted
            .chars()
            .filter(|ch| *ch != '\r' && *ch != '\n' && !ch.is_control())
            .collect();
        if cleaned.is_empty() {
            return;
        }
        let replace = self.input_selected_all;
        self.input_selected_all = false;
        let max_len = self.active_input_max_len();
        let target = self.active_input_mut();
        let mut changed = false;
        if replace && !target.is_empty() {
            target.clear();
            changed = true;
        }
        let mut current_len = target.chars().count();
        for ch in cleaned.chars() {
            if current_len < max_len {
                target.push(ch);
                current_len += 1;
                changed = true;
            } else {
                break;
            }
        }
        if changed || replace {
            self.reset_scroll_for_active_input();
            self.refresh(hwnd);
        }
    }

    fn handle_key_down(&mut self, hwnd: HWND, vk: u16) -> (bool, PostMsgAction) {
        if vk == VK_ESCAPE.0 {
            self.input_selected_all = false;
            if matches!(self.active_overlay, ActiveOverlay::ListenTarget(_)) {
                if !self.listen_search_query.is_empty() {
                    self.listen_search_query.clear();
                    self.listen_scroll = 0;
                    self.refresh(hwnd);
                } else {
                    self.close_listen_overlay(hwnd);
                }
            } else if self.active_overlay == ActiveOverlay::Config && self.confirm_quit {
                self.confirm_quit = false;
                self.refresh(hwnd);
            } else if self.active_overlay != ActiveOverlay::None {
                self.confirm_quit = false;
                self.active_overlay = ActiveOverlay::None;
                self.refresh(hwnd);
            } else if self.saving_profile {
                self.cancel_save_profile(hwnd);
            } else if !self.search_query.is_empty() {
                self.search_query.clear();
                self.device_scroll = 0;
                self.refresh(hwnd);
            } else if self.active_input == ActiveInput::Search {
                self.active_input = ActiveInput::None;
                self.refresh(hwnd);
            } else {
                return (true, PostMsgAction::Close);
            }
            return (true, PostMsgAction::None);
        }

        if self.active_input == ActiveInput::None {
            return (false, PostMsgAction::None);
        }

        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let alt = unsafe { GetKeyState(VK_MENU.0 as i32) } < 0;

        if ctrl && !alt {
            match vk {
                0x41 => {
                    if !self.active_input_str().is_empty() {
                        self.input_selected_all = true;
                        self.refresh(hwnd);
                    }
                    return (true, PostMsgAction::None);
                }
                0x43 => {
                    let text = self.active_input_str().to_string();
                    let post = if text.is_empty() {
                        PostMsgAction::None
                    } else {
                        PostMsgAction::CopyClipboard(text)
                    };
                    return (true, post);
                }
                0x56 => {
                    return (true, PostMsgAction::PasteClipboard);
                }
                v if v == VK_BACK.0 => {
                    let was_selected = self.input_selected_all;
                    self.input_selected_all = false;
                    let target = self.active_input_mut();
                    let had = !target.is_empty();
                    target.clear();
                    if had || was_selected {
                        self.reset_scroll_for_active_input();
                        self.refresh(hwnd);
                    }
                    return (true, PostMsgAction::None);
                }
                _ => return (false, PostMsgAction::None),
            }
        }

        let handled = match vk {
            v if v == VK_RETURN.0 => {
                if self.active_input == ActiveInput::SaveProfile {
                    self.commit_save_profile(hwnd);
                }
                true
            }
            v if v == VK_BACK.0 => {
                let was_selected = self.input_selected_all;
                self.input_selected_all = false;
                let target = self.active_input_mut();
                let changed = if was_selected {
                    let had = !target.is_empty();
                    target.clear();
                    had
                } else {
                    target.pop().is_some()
                };
                if changed || was_selected {
                    self.reset_scroll_for_active_input();
                    self.refresh(hwnd);
                }
                true
            }
            v if v == VK_DELETE.0 => {
                if self.input_selected_all {
                    self.input_selected_all = false;
                    self.active_input_mut().clear();
                    self.reset_scroll_for_active_input();
                    self.refresh(hwnd);
                }
                true
            }
            v if v == VK_LEFT.0 || v == VK_RIGHT.0 => {
                if self.input_selected_all {
                    self.input_selected_all = false;
                    self.refresh(hwnd);
                }
                true
            }
            _ => false,
        };
        (handled, PostMsgAction::None)
    }

    fn handle_char(&mut self, hwnd: HWND, code_unit: u16) {
        if self.active_input == ActiveInput::None {
            self.pending_high_surrogate = None;
            return;
        }
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let alt = unsafe { GetKeyState(VK_MENU.0 as i32) } < 0;
        if ctrl && !alt {
            self.pending_high_surrogate = None;
            return;
        }

        let ch = if (0xD800..=0xDBFF).contains(&code_unit) {
            self.pending_high_surrogate = Some(code_unit);
            return;
        } else if (0xDC00..=0xDFFF).contains(&code_unit) {
            let Some(high) = self.pending_high_surrogate.take() else {
                return;
            };
            let code_point =
                0x10000 + (((u32::from(high) - 0xD800) << 10) | (u32::from(code_unit) - 0xDC00));
            let Some(c) = char::from_u32(code_point) else {
                return;
            };
            c
        } else {
            self.pending_high_surrogate = None;
            let Some(c) = char::from_u32(u32::from(code_unit)) else {
                return;
            };
            c
        };

        if ch.is_control() {
            return;
        }

        let replace = self.input_selected_all;
        self.input_selected_all = false;
        let max_len = self.active_input_max_len();
        let target = self.active_input_mut();
        if replace {
            target.clear();
        }
        let mut changed = replace;
        if target.chars().count() < max_len {
            target.push(ch);
            changed = true;
        }
        if changed {
            self.reset_scroll_for_active_input();
            self.refresh(hwnd);
        }
    }

    fn handle_wheel(&mut self, hwnd: HWND, delta: i32) {
        let Some(ref layout) = self.layout else {
            return;
        };
        let step = 36.0 * self.scale;
        let scroll_delta = (-(delta as f32 / WHEEL_DELTA as f32) * step).round() as i32;
        if scroll_delta == 0 {
            return;
        }

        let changed = match self.active_overlay {
            ActiveOverlay::None => {
                let next = (self.device_scroll + scroll_delta).clamp(0, layout.list_max_scroll);
                let diff = next != self.device_scroll;
                self.device_scroll = next;
                diff
            }
            ActiveOverlay::Config => {
                let next =
                    (self.config_scroll + scroll_delta).clamp(0, layout.overlay_max_scroll());
                let diff = next != self.config_scroll;
                self.config_scroll = next;
                diff
            }
            ActiveOverlay::ProfileMenu => {
                let next =
                    (self.profile_scroll + scroll_delta).clamp(0, layout.overlay_max_scroll());
                let diff = next != self.profile_scroll;
                self.profile_scroll = next;
                diff
            }
            ActiveOverlay::ListenTarget(_) => {
                let next =
                    (self.listen_scroll + scroll_delta).clamp(0, layout.overlay_max_scroll());
                let diff = next != self.listen_scroll;
                self.listen_scroll = next;
                diff
            }
        };

        if changed {
            self.refresh(hwnd);
            // Keep the insertion marker under the pointer while the wheel moves the list.
            if let Some((dragged, _, _)) = self.dragging_card.as_ref()
                && let Some((_, y)) = self.last_mouse
            {
                let dragged = dragged.clone();
                self.card_drop = self.drop_index(y, &dragged);
            }
        }
    }
}

fn volume_from_bar_x(bar_rect: RectPx, x: i32, scale: f32) -> u8 {
    let pad_l = TACTILE_PAD_LEFT_LOGICAL * scale;
    let pad_r = TACTILE_PAD_RIGHT_LOGICAL * scale;
    let usable = (bar_rect.width() as f32 - pad_l - pad_r).max(1.0);
    let ratio = ((x as f32 - (bar_rect.left as f32 + pad_l)) / usable).clamp(0.0, 1.0);
    (ratio * 100.0).round() as u8
}

fn register_window_class() {
    REGISTER_CLASS.call_once(|| {
        let class_name = win32::to_wide(FLYOUT_CLASS_NAME);
        unsafe {
            let Ok(hinstance) = GetModuleHandleW(None) else {
                return;
            };
            let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(flyout_wnd_proc),
                hInstance: hinstance.into(),
                hCursor: cursor,
                lpszClassName: PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            RegisterClassW(&wc);
        }
    });
}

fn monitor_dpi(monitor: HMONITOR) -> u32 {
    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    let ok = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    if ok.is_ok() && dpi_x > 0 { dpi_x } else { 96 }
}

fn compute_window_rect(monitor: HMONITOR, anchor: (i32, i32), scale: f32) -> (i32, i32, i32, i32) {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let desired_w = px(FLYOUT_WIDTH_LOGICAL, scale);
    let desired_h = px(FLYOUT_HEIGHT_LOGICAL, scale);

    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return (anchor.0 - desired_w / 2, anchor.1 - desired_h, desired_w, desired_h);
    }

    let work = info.rcWork;
    let margin = px(EDGE_MARGIN_LOGICAL, scale);
    let max_w = (work.right - work.left - margin * 2).max(px(280.0, scale));
    let max_h = (work.bottom - work.top - margin * 2).max(px(240.0, scale));
    let win_w = desired_w.min(max_w);
    let win_h = desired_h.min(max_h);

    let min_x = work.left + margin;
    let max_x = (work.right - margin - win_w).max(min_x);
    let x = (anchor.0 - win_w / 2).clamp(min_x, max_x);

    let min_y = work.top + margin;
    let max_y = (work.bottom - margin - win_h).max(min_y);
    let raw_y = if anchor.1 <= work.top {
        work.top + margin
    } else if anchor.1 >= work.bottom {
        work.bottom - margin - win_h
    } else if anchor.1 - win_h >= work.top + margin {
        anchor.1 - win_h
    } else {
        anchor.1
    };
    let y = raw_y.clamp(min_y, max_y);

    (x, y, win_w, win_h)
}

fn request_close(hwnd: HWND) {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowData;
    if ptr.is_null() {
        return;
    }
    let (should_destroy, should_release_capture) = {
        let data = unsafe { &mut *ptr };
        if data.is_closing {
            (false, false)
        } else {
            data.is_closing = true;
            data.confirm_quit = false;
            let had_drag = data.dragging_volume.take().is_some();
            (true, had_drag)
        }
    };
    if should_release_capture {
        unsafe {
            let _ = ReleaseCapture();
        }
    }
    if should_destroy {
        unsafe {
            let _ = KillTimer(Some(hwnd), STATUS_TIMER_ID);
            let _ = DestroyWindow(hwnd);
        }
    }
}

fn run_post_action(hwnd: HWND, action: PostMsgAction) {
    match action {
        PostMsgAction::None => {}
        PostMsgAction::Close => request_close(hwnd),
        PostMsgAction::Capture => unsafe {
            SetCapture(hwnd);
        },
        PostMsgAction::ReleaseCapture => unsafe {
            let _ = ReleaseCapture();
        },
        PostMsgAction::CopyClipboard(text) => {
            write_clipboard_text(hwnd, &text);
        }
        PostMsgAction::PasteClipboard => {
            if let Some(pasted) = read_clipboard_text(hwnd) {
                let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowData;
                if !ptr.is_null() {
                    let data = unsafe { &mut *ptr };
                    data.apply_pasted_text(hwnd, &pasted);
                }
            }
        }
    }
}

fn lparam_point(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam.0 & 0xFFFF) as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
    (x, y)
}

fn write_clipboard_text(hwnd: HWND, text: &str) {
    let wide = win32::to_wide(text);
    let byte_len = wide.len() * size_of::<u16>();
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, byte_len) {
            let dst = GlobalLock(hmem).cast::<u16>();
            if !dst.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
                let _ = GlobalUnlock(hmem);
                if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hmem.0))).is_err() {
                    let _ = GlobalFree(Some(hmem));
                }
            } else {
                let _ = GlobalFree(Some(hmem));
            }
        }
        let _ = CloseClipboard();
    }
}

fn read_clipboard_text(hwnd: HWND) -> Option<String> {
    unsafe {
        OpenClipboard(Some(hwnd)).ok()?;
        let result = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
            let hmem = HGLOBAL(handle.0);
            let byte_len = GlobalSize(hmem);
            let max_units = (byte_len / size_of::<u16>()).min(1024);
            if max_units == 0 {
                return None;
            }
            let ptr = GlobalLock(hmem).cast::<u16>();
            if ptr.is_null() {
                return None;
            }
            let mut len = 0usize;
            while len < max_units && *ptr.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            let text = String::from_utf16_lossy(slice);
            let _ = GlobalUnlock(hmem);
            Some(text)
        })();
        let _ = CloseClipboard();
        result
    }
}

unsafe extern "system" fn flyout_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowData;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint_flyout(hwnd, unsafe { &mut *ptr });
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_point(lparam);
            let (need_track, dragging, repaint) = {
                let data = unsafe { &mut *ptr };
                data.last_mouse = Some((x, y));
                let need_track = !data.tracking_mouse;
                if need_track {
                    data.tracking_mouse = true;
                }
                if let Some((ref dev_id, bar_rect)) = data.dragging_volume {
                    (need_track, Some((dev_id.clone(), bar_rect)), false)
                } else if data.dragging_card.is_some() {
                    let repaint = data.update_card_drag(x, y);
                    (need_track, None, repaint)
                } else {
                    let (next_action, next_cursor, next_card) = match data.layout.as_ref() {
                        Some(layout) => {
                            let (act, cur) = match layout.hit_test(x, y) {
                                Some(hit) => (Some(hit.action.clone()), hit.cursor),
                                None => (None, CursorKind::Arrow),
                            };
                            (act, cur, layout.hovered_card_id(x, y).map(str::to_string))
                        }
                        None => (None, CursorKind::Arrow, None),
                    };
                    let changed =
                        next_action != data.hovered_action || next_card != data.hovered_card_id;
                    data.hovered_action = next_action;
                    data.hovered_cursor = next_cursor;
                    data.hovered_card_id = next_card;
                    (need_track, None, changed)
                }
            };
            if need_track {
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
            if let Some((dev_id, bar_rect)) = dragging {
                let data = unsafe { &mut *ptr };
                data.update_volume_from_x(hwnd, &dev_id, bar_rect, x);
            } else if repaint {
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let repaint = {
                let data = unsafe { &mut *ptr };
                data.tracking_mouse = false;
                data.last_mouse = None;
                let had = data.hovered_action.is_some() || data.hovered_card_id.is_some();
                data.hovered_action = None;
                data.hovered_cursor = CursorKind::Arrow;
                data.hovered_card_id = None;
                had
            };
            if repaint {
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if (lparam.0 & 0xFFFF) as u32 == HTCLIENT {
                let cursor_id = {
                    let data = unsafe { &*ptr };
                    if data.dragging_volume.is_some() {
                        IDC_SIZEWE
                    } else {
                        match data.hovered_cursor {
                            CursorKind::Arrow => IDC_ARROW,
                            CursorKind::Hand => IDC_HAND,
                            CursorKind::IBeam => IDC_IBEAM,
                            CursorKind::ResizeEW => IDC_SIZEWE,
                        }
                    }
                };
                unsafe {
                    if let Ok(cursor) = LoadCursorW(None, cursor_id) {
                        SetCursor(Some(cursor));
                    }
                }
                return LRESULT(1);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_LBUTTONDOWN => {
            let (x, y) = lparam_point(lparam);
            let post_action = {
                let data = unsafe { &mut *ptr };
                data.last_mouse = Some((x, y));
                data.handle_click(hwnd, x, y)
            };
            run_post_action(hwnd, post_action);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = lparam_point(lparam);
            let post_action = {
                let data = unsafe { &mut *ptr };
                data.last_mouse = Some((x, y));
                if data.dragging_card.is_some() {
                    data.commit_card_drag(hwnd);
                    PostMsgAction::ReleaseCapture
                } else if data.dragging_volume.take().is_some() {
                    PostMsgAction::ReleaseCapture
                } else {
                    PostMsgAction::None
                }
            };
            if matches!(post_action, PostMsgAction::ReleaseCapture) {
                run_post_action(hwnd, post_action);
                let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowData;
                if !ptr.is_null() {
                    let data = unsafe { &mut *ptr };
                    data.refresh(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            let data = unsafe { &mut *ptr };
            let card = data.dragging_card.take().is_some();
            let volume = data.dragging_volume.take().is_some();
            if (card || volume) && !data.is_closing {
                data.card_drop = None;
                data.refresh(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
            {
                let data = unsafe { &mut *ptr };
                data.handle_wheel(hwnd, delta);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let vk = (wparam.0 & 0xFFFF) as u16;
            let (handled, post_action) = {
                let data = unsafe { &mut *ptr };
                data.handle_key_down(hwnd, vk)
            };
            run_post_action(hwnd, post_action);
            if handled {
                LRESULT(0)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_CHAR => {
            let code_unit = (wparam.0 & 0xFFFF) as u16;
            {
                let data = unsafe { &mut *ptr };
                data.handle_char(hwnd, code_unit);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == STATUS_TIMER_ID {
                unsafe {
                    let _ = KillTimer(Some(hwnd), STATUS_TIMER_ID);
                }
                {
                    let data = unsafe { &mut *ptr };
                    data.status_text = "Auto-saved".to_string();
                    data.status_flash = false;
                }
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_DPICHANGED => {
            let new_dpi = (wparam.0 & 0xFFFF) as u32;
            let new_scale = if new_dpi > 0 {
                new_dpi as f32 / 96.0
            } else {
                1.0
            };
            let suggested_ptr = lparam.0 as *const RECT;
            let suggested = (!suggested_ptr.is_null()).then(|| unsafe { *suggested_ptr });
            let target_rect = {
                let data = unsafe { &mut *ptr };
                let (w, h, pos) = if let Some(rc) = suggested {
                    let rw = (rc.right - rc.left).max(1);
                    let rh = (rc.bottom - rc.top).max(1);
                    (rw, rh, Some((rc.left, rc.top, rw, rh)))
                } else {
                    let rw = px(FLYOUT_WIDTH_LOGICAL, new_scale).max(1);
                    let rh = px(FLYOUT_HEIGHT_LOGICAL, new_scale).max(1);
                    (rw, rh, None)
                };
                data.scale = new_scale;
                data.width = w;
                data.height = h;
                data.fonts = FontCache::new(new_scale);
                let snapshot = data.snapshot();
                data.recompute_layout(&snapshot);
                pos
            };
            if let Some((x, y, w, h)) = target_rect {
                unsafe {
                    let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
                }
            }
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                let should_close = {
                    let data = unsafe { &*ptr };
                    !data.pinned && data.opened_at.elapsed() >= STARTUP_GRACE
                };
                if should_close {
                    request_close(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let data = Box::from_raw(ptr);
                data.shared.on_destroyed(hwnd);
                drop(data);
            }
            win32::trim_working_set();
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn paint_flyout(hwnd: HWND, data: &mut WindowData) {
    let snapshot = data.snapshot();
    if data.layout.is_none() {
        data.recompute_layout(&snapshot);
    }
    let Some((width, height)) = data.layout.as_ref().map(|l| (l.width, l.height)) else {
        return;
    };
    let scale = data.scale;
    let theme = data.theme;

    let mut ps = PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    if hdc.is_invalid() || width <= 0 || height <= 0 {
        unsafe {
            let _ = EndPaint(hwnd, &ps);
        }
        return;
    }

    if !data.backbuffer.ensure(hdc, width, height) {
        unsafe {
            let _ = EndPaint(hwnd, &ps);
        }
        return;
    }

    let mem_dc = data.backbuffer.mem_dc;
    let pixels_ptr = data.backbuffer.pixels.cast::<u8>();
    let Some(ref layout) = data.layout else {
        unsafe {
            let _ = EndPaint(hwnd, &ps);
        }
        return;
    };

    let byte_len = (width as usize) * (height as usize) * 4;
    let search_text_w = measure_text_width(
        mem_dc,
        data.fonts.get(FontRole::Body11_5Reg),
        &data.search_query,
    );
    let save_text_w = measure_text_width(
        mem_dc,
        data.fonts.get(FontRole::Body11_5Reg),
        &data.profile_name_input,
    );
    let listen_text_w = measure_text_width(
        mem_dc,
        data.fonts.get(FontRole::Body11_5Reg),
        &data.listen_search_query,
    );

    // Pass 1: Base layer pixels (shapes + icons)
    unsafe {
        let _ = GdiFlush();
    }
    {
        let pixels = unsafe { std::slice::from_raw_parts_mut(pixels_ptr, byte_len) };
        let mut canvas = Canvas::new(pixels, width, height);
        paint_base_shapes(
            &mut canvas,
            data,
            &snapshot,
            layout,
            theme,
            scale,
            search_text_w,
            save_text_w,
        );
    }

    // Pass 2: Base layer ClearType text
    paint_base_text(mem_dc, data, &snapshot, layout, theme, scale);

    // Pass 3 & 4: Active floating overlay (shadow, shapes, icons, ClearType text)
    if !matches!(layout.overlay, OverlayLayout::None) {
        unsafe {
            let _ = GdiFlush();
        }
        {
            let pixels = unsafe { std::slice::from_raw_parts_mut(pixels_ptr, byte_len) };
            let mut canvas = Canvas::new(pixels, width, height);
            paint_overlay_shapes(
                &mut canvas,
                data,
                &snapshot,
                &layout.overlay,
                theme,
                scale,
                listen_text_w,
            );
        }
        paint_overlay_text(mem_dc, data, &snapshot, &layout.overlay, theme, scale);
    }

    unsafe {
        let _ = BitBlt(hdc, 0, 0, width, height, Some(mem_dc), 0, 0, SRCCOPY);
        let _ = EndPaint(hwnd, &ps);
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_base_shapes(
    canvas: &mut Canvas<'_>,
    data: &WindowData,
    state: &AppState,
    layout: &FlyoutLayout,
    theme: Theme,
    s: f32,
    search_text_w: i32,
    save_text_w: i32,
) {
    let bw = s.max(1.0);
    let bw_px = px(1.0, s).max(1);
    canvas.fill_all(theme.bg);
    if !data.dwm_border_ok {
        canvas.stroke_rect_1px(RectPx::new(0, 0, layout.width, layout.height), theme.border_strong);
    }

    let is_hovered = |act: &HitAction| data.hovered_action.as_ref() == Some(act);

    // Header: lock pill
    {
        let pill = layout.header.lock_pill;
        let hovered = is_hovered(&HitAction::ToggleLock);
        let locked = state.lock_enabled;
        let r = pill.height() as f32 * 0.5;
        let (fill, border, icon_col) = if locked {
            let bg = if hovered { theme.inv_hover } else { theme.inv_bg };
            (Some(bg), Some((bg, bw)), theme.inv_text)
        } else {
            let bd = if hovered { theme.text_sec } else { theme.border_strong };
            let ic = if hovered { theme.text } else { theme.text_sec };
            (None, Some((bd, bw)), ic)
        };
        canvas.fill_round_rect(pill, r, fill, border);
        let icon_s = 12.0 * s;
        let ix = pill.left as f32 + 6.0 * s;
        let iy = pill.top as f32 + (pill.height() as f32 - icon_s) * 0.5;
        canvas.draw_icon(
            if locked { IconKind::Lock } else { IconKind::Unlock },
            ix,
            iy,
            icon_s,
            icon_col,
        );
    }

    // Header: Option B 3-button cluster [Settings][Pin][Theme]
    {
        let cluster = layout.header.cluster_rect;
        canvas.fill_round_rect(cluster, 7.0 * s, Some(theme.bg_elev), None);

        let r_inner = 6.0 * s;
        let config_open = data.active_overlay == ActiveOverlay::Config;
        let cfg_hov = is_hovered(&HitAction::ToggleConfig);
        let cfg_fill = if config_open {
            Some(if cfg_hov { theme.inv_hover } else { theme.inv_bg })
        } else if cfg_hov {
            Some(theme.bg_hover)
        } else {
            None
        };
        if cfg_fill.is_some() {
            canvas.fill_round_rect_corners(
                layout.header.config_btn,
                [r_inner, 0.0, 0.0, r_inner],
                cfg_fill,
                None,
            );
        }
        canvas.draw_icon_centered(
            IconKind::Settings,
            layout.header.config_btn,
            13.0 * s,
            if config_open { theme.inv_text } else { theme.text_sec },
        );

        let pin_hov = is_hovered(&HitAction::TogglePin);
        let pin_fill = if data.pinned {
            Some(if pin_hov { theme.inv_hover } else { theme.inv_bg })
        } else if pin_hov {
            Some(theme.bg_hover)
        } else {
            None
        };
        if let Some(fill) = pin_fill {
            canvas.fill_rect(layout.header.pin_btn, fill);
        }
        canvas.draw_icon_centered(
            IconKind::Pin,
            layout.header.pin_btn,
            13.0 * s,
            if data.pinned { theme.inv_text } else { theme.text_sec },
        );

        // Power: closes SETTMetter. A second click confirms, same as the Quit row.
        let quit_hov = is_hovered(&HitAction::HeaderQuit);
        let quit_fill = if data.confirm_quit {
            Some(if quit_hov { theme.inv_hover } else { theme.inv_bg })
        } else if quit_hov {
            Some(theme.bg_hover)
        } else {
            None
        };
        if let Some(fill) = quit_fill {
            canvas.fill_round_rect_corners(
                layout.header.quit_btn,
                [0.0, r_inner, r_inner, 0.0],
                Some(fill),
                None,
            );
        }
        canvas.draw_icon_centered(
            IconKind::Power,
            layout.header.quit_btn,
            13.0 * s,
            if data.confirm_quit {
                theme.inv_text
            } else if quit_hov {
                theme.text
            } else {
                theme.text_sec
            },
        );

        canvas.vline(
            layout.header.config_btn.right,
            cluster.top + bw_px,
            cluster.bottom - bw_px,
            bw_px,
            theme.border_strong,
        );
        canvas.vline(
            layout.header.pin_btn.right,
            cluster.top + bw_px,
            cluster.bottom - bw_px,
            bw_px,
            theme.border_strong,
        );
        canvas.stroke_round_rect(cluster, 7.0 * s, theme.border_strong, bw);
    }

    // Profile bar
    {
        let pb = &layout.profile_bar;
        let menu_open = data.active_overlay == ActiveOverlay::ProfileMenu;
        let sel_hov = is_hovered(&HitAction::ToggleProfileMenu);
        canvas.fill_round_rect(
            pb.selector_btn,
            7.0 * s,
            Some(if sel_hov { theme.bg_hover } else { theme.bg_elev }),
            Some((
                if menu_open || sel_hov {
                    theme.text_muted
                } else {
                    theme.border_strong
                },
                bw,
            )),
        );
        let chev_s = 11.0 * s;
        canvas.draw_icon(
            IconKind::ChevronDown,
            pb.selector_btn.right as f32 - 7.0 * s - chev_s,
            pb.selector_btn.top as f32 + (pb.selector_btn.height() as f32 - chev_s) * 0.5,
            chev_s,
            theme.text_sec,
        );

        let save_hov = is_hovered(&HitAction::ToggleSaveProfileBar);
        let (save_fill, save_bd, plus_col) = if data.saving_profile {
            let bg = if save_hov { theme.inv_hover } else { theme.inv_bg };
            (Some(bg), bg, theme.inv_text)
        } else {
            (
                save_hov.then_some(theme.bg_hover),
                if save_hov {
                    theme.text_muted
                } else {
                    theme.border_strong
                },
                theme.text,
            )
        };
        canvas.fill_round_rect(pb.save_toggle_btn, 7.0 * s, save_fill, Some((save_bd, bw)));
        let plus_s = 11.0 * s;
        canvas.draw_icon(
            IconKind::Plus,
            pb.save_toggle_btn.left as f32 + 9.0 * s,
            pb.save_toggle_btn.top as f32 + (pb.save_toggle_btn.height() as f32 - plus_s) * 0.5,
            plus_s,
            plus_col,
        );

        if let Some(ref sb) = pb.save_bar {
            let focused = data.active_input == ActiveInput::SaveProfile;
            canvas.fill_round_rect(
                sb.input_rect,
                7.0 * s,
                Some(if focused { theme.bg_elev } else { theme.bg_recess }),
                Some((
                    if focused {
                        theme.text_muted
                    } else {
                        theme.border_strong
                    },
                    bw,
                )),
            );
            let text_area = RectPx::new(
                sb.input_rect.left + px(8.0, s),
                sb.input_rect.top,
                sb.input_rect.right - px(8.0, s),
                sb.input_rect.bottom,
            );
            draw_input_caret_or_selection(
                canvas,
                text_area,
                focused,
                data.input_selected_all,
                data.profile_name_input.is_empty(),
                save_text_w,
                theme,
                s,
            );

            let confirm_hov = is_hovered(&HitAction::CommitSaveProfile);
            canvas.fill_round_rect(
                sb.confirm_btn,
                7.0 * s,
                Some(if confirm_hov {
                    theme.inv_hover
                } else {
                    theme.inv_bg
                }),
                None,
            );

            let cancel_hov = is_hovered(&HitAction::CancelSaveProfile);
            canvas.fill_round_rect(
                sb.cancel_btn,
                7.0 * s,
                Some(if cancel_hov {
                    theme.bg_hover
                } else {
                    theme.bg_elev
                }),
                Some((
                    if cancel_hov {
                        theme.text_muted
                    } else {
                        theme.border_strong
                    },
                    bw,
                )),
            );
            canvas.draw_icon_centered(IconKind::CloseX, sb.cancel_btn, 11.0 * s, theme.text_sec);
        }
    }

    // Tabs + inline Search
    {
        let ts = &layout.tabs_search;
        canvas.fill_round_rect(
            ts.tabs_container,
            8.0 * s,
            Some(theme.bg_recess),
            Some((theme.border, bw)),
        );
        for (rect, flow) in [
            (ts.output_tab, DeviceFlow::Output),
            (ts.input_tab, DeviceFlow::Input),
        ] {
            if data.active_tab == flow {
                canvas.fill_round_rect(
                    rect,
                    5.0 * s,
                    Some(theme.bg_pill),
                    Some((theme.border_strong, bw)),
                );
            }
        }

        let search_focused = data.active_input == ActiveInput::Search;
        let search_hov = is_hovered(&HitAction::FocusSearch);
        canvas.fill_round_rect(
            ts.search_box,
            8.0 * s,
            Some(if search_focused {
                theme.bg_elev
            } else {
                theme.bg_recess
            }),
            Some((
                if search_focused {
                    theme.text_muted
                } else if search_hov {
                    theme.border_strong
                } else {
                    theme.border
                },
                bw,
            )),
        );
        let search_icon_s = 12.0 * s;
        canvas.draw_icon(
            IconKind::Search,
            ts.search_box.left as f32 + 8.0 * s,
            ts.search_box.top as f32 + (ts.search_box.height() as f32 - search_icon_s) * 0.5,
            search_icon_s,
            if search_focused {
                theme.text_sec
            } else {
                theme.text_muted
            },
        );
        let text_left = ts.search_box.left + px(8.0 + 12.0 + 6.0, s);
        let text_right = ts
            .search_clear_btn
            .map_or(ts.search_box.right - px(6.0, s), |r| r.left - px(4.0, s));
        draw_input_caret_or_selection(
            canvas,
            RectPx::new(text_left, ts.search_box.top, text_right, ts.search_box.bottom),
            search_focused,
            data.input_selected_all,
            data.search_query.is_empty(),
            search_text_w,
            theme,
            s,
        );
        if let Some(clear_r) = ts.search_clear_btn {
            if is_hovered(&HitAction::ClearSearch) {
                canvas.fill_round_rect(clear_r, 4.0 * s, Some(theme.bg_hover), None);
            }
            canvas.draw_icon_centered(IconKind::CloseX, clear_r, 10.5 * s, theme.text_muted);
        }
    }

    // Device List (clipped)
    canvas.push_clip(layout.list_viewport);
    if let Some(ref empty) = layout.empty_state {
        canvas.fill_round_rect(
            empty.card_rect,
            10.0 * s,
            Some(theme.bg_recess),
            Some((theme.border_strong, bw)),
        );
        if let Some(btn) = empty.clear_btn {
            let hov = is_hovered(&HitAction::EmptyClearSearch);
            canvas.fill_round_rect(
                btn,
                6.0 * s,
                Some(if hov { theme.bg_hover } else { theme.bg_elev }),
                Some((
                    if hov {
                        theme.text_muted
                    } else {
                        theme.border_strong
                    },
                    bw,
                )),
            );
        }
    } else {
        for card in &layout.cards {
            if card.card_rect.bottom <= layout.list_viewport.top
                || card.card_rect.top >= layout.list_viewport.bottom
            {
                continue;
            }
            paint_device_card_shapes(canvas, data, card, theme, s);
        }
    }

    // Reorder feedback: outline the card being dragged and mark where it will land.
    if let Some((dragged, _, _)) = data.dragging_card.as_ref() {
        canvas.push_clip(layout.list_viewport);
        if let Some(card) = layout.cards.iter().find(|card| &card.device_id == dragged) {
            canvas.stroke_round_rect(card.card_rect, 10.0 * s, theme.text_muted, 1.5 * s);
        }
        if let Some(index) = data.card_drop {
            let y = match layout.cards.get(index) {
                Some(card) => card.card_rect.top - px(3.0, s),
                None => layout
                    .cards
                    .last()
                    .map(|card| card.card_rect.bottom + px(1.0, s))
                    .unwrap_or(0),
            };
            canvas.fill_round_rect(
                RectPx::from_xywh(
                    layout.list_viewport.left + px(8.0, s),
                    y,
                    layout.list_viewport.width() - px(16.0, s),
                    px(2.0, s).max(2),
                ),
                s,
                Some(theme.text_sec),
                None,
            );
        }
        canvas.pop_clip();
    }

    if layout.list_max_scroll > 0 {
        draw_scrollbar(
            canvas,
            layout.list_viewport,
            layout.list_scroll,
            layout.list_max_scroll,
            layout.list_content_height,
            theme,
            s,
        );
    }
    canvas.pop_clip();

    // Footer
    {
        let ft = &layout.footer;
        canvas.hline(0, layout.width, ft.top_y, bw_px, theme.divider);
        let ind_col = if data.status_flash {
            theme.text
        } else {
            theme.text_muted
        };
        canvas.fill_round_rect(ft.dot_rect, ft.dot_rect.height() as f32 * 0.5, Some(ind_col), None);
    }
}

fn paint_device_card_shapes(
    canvas: &mut Canvas<'_>,
    data: &WindowData,
    card: &DeviceCardLayout,
    theme: Theme,
    s: f32,
) {
    let bw = s.max(1.0);
    let bw_px = px(1.0, s).max(1);
    let card_hov = data.hovered_card_id.as_deref() == Some(card.device_id.as_str());
    canvas.fill_round_rect(
        card.card_rect,
        10.0 * s,
        Some(theme.bg_elev),
        Some((
            if card_hov {
                theme.border_strong
            } else {
                theme.border
            },
            bw,
        )),
    );

    // Role pills
    let draw_role_pill = |canvas: &mut Canvas<'_>, rect: RectPx, active: bool, action: HitAction| {
        let hov = data.hovered_action.as_ref() == Some(&action);
        let r = rect.height() as f32 * 0.5;
        let (fill, border) = if active {
            let c = if hov { theme.inv_hover } else { theme.inv_bg };
            (Some(c), c)
        } else if hov {
            (Some(theme.bg_hover), theme.text_muted)
        } else {
            (None, theme.border_strong)
        };
        canvas.fill_round_rect(rect, r, fill, Some((border, bw)));
    };

    draw_role_pill(
        canvas,
        card.default_pill,
        card.is_default,
        HitAction::SetDefault {
            flow: card.flow,
            device_id: card.device_id.clone(),
        },
    );
    if let Some(com_pill) = card.comms_pill {
        draw_role_pill(
            canvas,
            com_pill,
            card.is_comms,
            HitAction::SetComms {
                flow: card.flow,
                device_id: card.device_id.clone(),
            },
        );
    }

    // C2 Tactile 26px volume bar
    let mute_icon = match (card.flow, card.muted) {
        (DeviceFlow::Output, false) => IconKind::Volume,
        (DeviceFlow::Output, true) => IconKind::VolumeMuted,
        (DeviceFlow::Input, false) => IconKind::Mic,
        (DeviceFlow::Input, true) => IconKind::MicMuted,
    };

    if card.disabled {
        canvas.fill_round_rect(
            card.volume_bar,
            7.0 * s,
            Some(theme.bg_recess),
            Some((theme.border, bw)),
        );
        canvas.draw_icon_centered(mute_icon, card.mute_btn, 13.0 * s, theme.text_muted);
    } else {
        canvas.fill_round_rect(card.volume_bar, 7.0 * s, Some(theme.tactile_bg), None);
        let fill_w = ((card.volume_bar.width() as f32) * (f32::from(card.volume) / 100.0)).round()
            as i32;
        if fill_w > 0 {
            let fill_col = if card.muted {
                theme.tactile_fill_muted
            } else {
                theme.tactile_fill
            };
            canvas.fill_bar_inside_round_rect(card.volume_bar, 7.0 * s, bw, fill_w, fill_col);
            let edge_w = px(2.0, s).max(1);
            let edge_x = (card.volume_bar.left + fill_w - edge_w)
                .clamp(card.volume_bar.left + bw_px, card.volume_bar.right - bw_px - edge_w);
            let edge_rect = RectPx::new(
                edge_x,
                card.volume_bar.top + px(4.0, s),
                edge_x + edge_w,
                card.volume_bar.bottom - px(4.0, s),
            );
            let edge_col = if card.muted {
                theme.tactile_edge_muted
            } else {
                theme.tactile_edge
            };
            canvas.fill_round_rect(edge_rect, 1.0 * s, Some(edge_col), None);
        }
        canvas.stroke_round_rect(card.volume_bar, 7.0 * s, theme.border_strong, bw);

        let mute_hov =
            data.hovered_action.as_ref() == Some(&HitAction::ToggleMute(card.device_id.clone()));
        if mute_hov {
            canvas.fill_round_rect(card.mute_btn, 5.0 * s, Some(theme.mute_hover_bg), None);
        }
        canvas.draw_icon_centered(
            mute_icon,
            card.mute_btn,
            13.0 * s,
            if card.muted {
                theme.text_muted
            } else {
                theme.text
            },
        );
    }

    // Footer row checkboxes + Pin button
    draw_checkbox(canvas, card.enh_box, card.enh_disabled, theme, s);
    if let Some(dbox) = card.disable_box {
        draw_checkbox(canvas, dbox, card.disabled, theme, s);
    }

    let pin_hov =
        data.hovered_action.as_ref() == Some(&HitAction::ToggleDevicePin(card.device_id.clone()));
    let (pin_fill, pin_bd) = if card.pinned {
        if pin_hov {
            (Some(theme.bg_hover), Some((theme.text_muted, bw)))
        } else {
            (Some(theme.pin_soft_bg), Some((theme.border_strong, bw)))
        }
    } else if pin_hov {
        (Some(theme.bg_hover), Some((theme.border_strong, bw)))
    } else {
        (None, None)
    };
    if pin_fill.is_some() || pin_bd.is_some() {
        canvas.fill_round_rect(card.pin_btn, 5.0 * s, pin_fill, pin_bd);
    }
    let pin_icon_s = 11.5 * s;
    let pin_icon = if card.pinned {
        IconKind::PinSolid
    } else {
        IconKind::Pin
    };
    let pin_icon_col = if card.pinned {
        theme.text
    } else {
        theme.text_muted
    };
    if card.pinned {
        canvas.draw_icon(
            pin_icon,
            card.pin_btn.left as f32 + 5.5 * s,
            card.pin_btn.top as f32 + (card.pin_btn.height() as f32 - pin_icon_s) * 0.5,
            pin_icon_s,
            pin_icon_col,
        );
    } else {
        canvas.draw_icon_centered(pin_icon, card.pin_btn, pin_icon_s, pin_icon_col);
    }

    // Input Listen row
    if let Some(ref listen) = card.listen {
        canvas.hline(
            card.volume_bar.left,
            card.volume_bar.right,
            listen.divider_y,
            bw_px,
            theme.divider,
        );
        draw_checkbox(canvas, listen.checkbox_box, listen.enabled, theme, s);

        let is_open = matches!(
            &data.active_overlay,
            ActiveOverlay::ListenTarget(id) if id == &card.device_id
        );
        let btn_hov = data.hovered_action.as_ref()
            == Some(&HitAction::ToggleListenMenu(card.device_id.clone()));
        canvas.fill_round_rect(
            listen.target_btn,
            6.0 * s,
            Some(if btn_hov {
                theme.bg_hover
            } else {
                theme.bg_elev
            }),
            Some((
                if is_open || btn_hov {
                    theme.text_muted
                } else {
                    theme.border_strong
                },
                bw,
            )),
        );
        let chev_s = 10.5 * s;
        canvas.draw_icon(
            IconKind::ChevronDown,
            listen.target_btn.right as f32 - 6.0 * s - chev_s,
            listen.target_btn.top as f32 + (listen.target_btn.height() as f32 - chev_s) * 0.5,
            chev_s,
            theme.text_sec,
        );
    }
}

fn paint_base_text(
    hdc: HDC,
    data: &WindowData,
    state: &AppState,
    layout: &FlyoutLayout,
    theme: Theme,
    s: f32,
) {
    let fonts = &data.fonts;
    let is_hovered = |act: &HitAction| data.hovered_action.as_ref() == Some(act);

    // Header
    draw_text(
        hdc,
        fonts.get(FontRole::Title14Semi),
        "SETTMetter Audio",
        layout.header.title_rect,
        theme.text,
        TextAlign::Left,
        false,
    );
    {
        let pill = layout.header.lock_pill;
        let locked = state.lock_enabled;
        let hov = is_hovered(&HitAction::ToggleLock);
        let col = if locked {
            theme.inv_text
        } else if hov {
            theme.text
        } else {
            theme.text_sec
        };
        draw_text(
            hdc,
            fonts.get(FontRole::Body11Semi),
            if locked { "Locked" } else { "Unlocked" },
            RectPx::new(pill.left + px(6.0 + 12.0 + 4.0, s), pill.top, pill.right - px(6.0, s), pill.bottom),
            col,
            TextAlign::Left,
            false,
        );
    }

    // Profile bar
    {
        let pb = &layout.profile_bar;
        draw_text(
            hdc,
            fonts.get(FontRole::Body11_5Med),
            &pb.selector_label,
            RectPx::new(
                pb.selector_btn.left + px(9.0, s),
                pb.selector_btn.top,
                pb.selector_btn.right - px(7.0 + 11.0 + 6.0, s),
                pb.selector_btn.bottom,
            ),
            if pb.has_profiles {
                theme.text
            } else {
                theme.text_muted
            },
            TextAlign::Left,
            true,
        );
        draw_text(
            hdc,
            fonts.get(FontRole::Body11_5Med),
            "Save",
            RectPx::new(
                pb.save_toggle_btn.left + px(9.0 + 11.0 + 4.0, s),
                pb.save_toggle_btn.top,
                pb.save_toggle_btn.right - px(6.0, s),
                pb.save_toggle_btn.bottom,
            ),
            if data.saving_profile {
                theme.inv_text
            } else {
                theme.text
            },
            TextAlign::Left,
            false,
        );
        if let Some(ref sb) = pb.save_bar {
            let focused = data.active_input == ActiveInput::SaveProfile;
            let selected = focused && data.input_selected_all && !data.profile_name_input.is_empty();
            let text_rect = RectPx::new(
                sb.input_rect.left + px(8.0, s) + if focused && data.profile_name_input.is_empty() { px(3.0, s) } else { 0 },
                sb.input_rect.top,
                sb.input_rect.right - px(8.0, s),
                sb.input_rect.bottom,
            );
            let (txt, col) = if data.profile_name_input.is_empty() {
                ("Profile name...", theme.text_muted)
            } else if selected {
                (data.profile_name_input.as_str(), theme.inv_text)
            } else {
                (data.profile_name_input.as_str(), theme.text)
            };
            draw_text(
                hdc,
                fonts.get(FontRole::Body11_5Reg),
                txt,
                text_rect,
                col,
                TextAlign::Left,
                true,
            );
            draw_text(
                hdc,
                fonts.get(FontRole::Body11Semi),
                "Save",
                sb.confirm_btn,
                theme.inv_text,
                TextAlign::Center,
                false,
            );
        }
    }

    // Tabs + Search
    {
        let ts = &layout.tabs_search;
        for (rect, label, count, flow) in [
            (ts.output_tab, "Output", ts.output_count, DeviceFlow::Output),
            (ts.input_tab, "Input", ts.input_count, DeviceFlow::Input),
        ] {
            let active = data.active_tab == flow;
            let hov = is_hovered(&HitAction::SetTab(flow));
            let lw = measure_text_width(hdc, fonts.get(FontRole::Body11_5Med), label);
            let label_r = RectPx::new(rect.left + px(10.0, s), rect.top, rect.left + px(10.0, s) + lw + px(2.0, s), rect.bottom);
            let cnt_r = RectPx::new(label_r.right + px(3.0, s), rect.top, rect.right - px(6.0, s), rect.bottom);
            draw_text(
                hdc,
                fonts.get(FontRole::Body11_5Med),
                label,
                label_r,
                if active || hov {
                    theme.text
                } else {
                    theme.text_muted
                },
                TextAlign::Left,
                false,
            );
            draw_text(
                hdc,
                fonts.get(FontRole::Tiny10Semi),
                &count.to_string(),
                cnt_r,
                theme.text_muted,
                TextAlign::Left,
                false,
            );
        }

        let search_focused = data.active_input == ActiveInput::Search;
        let has_query = !data.search_query.is_empty();
        let selected = search_focused && data.input_selected_all && has_query;
        let text_left = ts.search_box.left
            + px(8.0 + 12.0 + 6.0, s)
            + if search_focused && !has_query {
                px(3.0, s)
            } else {
                0
            };
        let text_right = ts
            .search_clear_btn
            .map_or(ts.search_box.right - px(6.0, s), |r| r.left - px(4.0, s));
        let (txt, col) = if !has_query {
            ("Search devices...", theme.text_muted)
        } else if selected {
            (data.search_query.as_str(), theme.inv_text)
        } else {
            (data.search_query.as_str(), theme.text)
        };
        draw_text(
            hdc,
            fonts.get(FontRole::Body11_5Reg),
            txt,
            RectPx::new(text_left, ts.search_box.top, text_right, ts.search_box.bottom),
            col,
            TextAlign::Left,
            true,
        );
    }

    // Device list text (clipped)
    let saved = unsafe { SaveDC(hdc) };
    unsafe {
        let _ = IntersectClipRect(
            hdc,
            layout.list_viewport.left,
            layout.list_viewport.top,
            layout.list_viewport.right,
            layout.list_viewport.bottom,
        );
    }

    if let Some(ref empty) = layout.empty_state {
        draw_text(
            hdc,
            fonts.get(FontRole::Body12Semi),
            "No matching devices",
            empty.title_rect,
            theme.text_sec,
            TextAlign::Center,
            false,
        );
        if let Some((ref sub_r, ref sub_txt)) = empty.subtitle {
            draw_text(
                hdc,
                fonts.get(FontRole::Sub10_5Reg),
                sub_txt,
                *sub_r,
                theme.text_muted,
                TextAlign::Center,
                true,
            );
        }
        if let Some(btn_r) = empty.clear_btn {
            draw_text(
                hdc,
                fonts.get(FontRole::Sub10_5Med),
                "Clear search",
                btn_r,
                theme.text,
                TextAlign::Center,
                false,
            );
        }
    } else {
        for card in &layout.cards {
            if card.card_rect.bottom <= layout.list_viewport.top
                || card.card_rect.top >= layout.list_viewport.bottom
            {
                continue;
            }
            draw_text(
                hdc,
                fonts.get(FontRole::Name13Semi),
                &card.name,
                card.name_rect,
                if card.disabled {
                    theme.text_sec
                } else {
                    theme.text
                },
                TextAlign::Left,
                true,
            );
            draw_text(
                hdc,
                fonts.get(FontRole::Sub10_5Reg),
                &card.subtitle,
                card.subtitle_rect,
                theme.text_muted,
                TextAlign::Left,
                true,
            );

            let def_hov = is_hovered(&HitAction::SetDefault {
                flow: card.flow,
                device_id: card.device_id.clone(),
            });
            draw_text(
                hdc,
                fonts.get(if card.is_default {
                    FontRole::Sub10_5Semi
                } else {
                    FontRole::Sub10_5Med
                }),
                "Default",
                card.default_pill,
                if card.is_default {
                    theme.inv_text
                } else if def_hov {
                    theme.text
                } else {
                    theme.text_muted
                },
                TextAlign::Center,
                false,
            );

            if let Some(com_pill) = card.comms_pill {
                let com_hov = is_hovered(&HitAction::SetComms {
                    flow: card.flow,
                    device_id: card.device_id.clone(),
                });
                draw_text(
                    hdc,
                    fonts.get(if card.is_comms {
                        FontRole::Sub10_5Semi
                    } else {
                        FontRole::Sub10_5Med
                    }),
                    "Communications",
                    com_pill,
                    if card.is_comms {
                        theme.inv_text
                    } else if com_hov {
                        theme.text
                    } else {
                        theme.text_muted
                    },
                    TextAlign::Center,
                    false,
                );
            }

            let pct_rect = RectPx::new(
                card.mute_btn.right + px(4.0, s),
                card.volume_bar.top,
                card.volume_bar.right - px(8.0, s),
                card.volume_bar.bottom,
            );
            if card.disabled {
                draw_text(
                    hdc,
                    fonts.get(FontRole::Body11Med),
                    "Disabled",
                    pct_rect,
                    theme.text_muted,
                    TextAlign::Right,
                    false,
                );
            } else {
                let pct = format!("{}%", card.volume);
                draw_text(
                    hdc,
                    fonts.get(FontRole::Body11Semi),
                    &pct,
                    pct_rect,
                    if card.muted {
                        theme.text_muted
                    } else {
                        theme.text
                    },
                    TextAlign::Right,
                    false,
                );
            }

            draw_text(
                hdc,
                fonts.get(FontRole::Body11Med),
                "Disable enhancements",
                card.enh_label,
                if card.enh_disabled {
                    theme.text
                } else {
                    theme.text_sec
                },
                TextAlign::Left,
                false,
            );

            if let Some(dlab) = card.disable_label {
                draw_text(
                    hdc,
                    fonts.get(FontRole::Body11Med),
                    if card.disabled { "Disabled" } else { "Disable" },
                    dlab,
                    if card.disabled {
                        theme.text
                    } else {
                        theme.text_sec
                    },
                    TextAlign::Left,
                    false,
                );
            }

            if card.pinned {
                draw_text(
                    hdc,
                    fonts.get(FontRole::Sub10_5Med),
                    "Pinned",
                    RectPx::new(
                        card.pin_btn.left + px(5.5 + 11.5 + 4.0, s),
                        card.pin_btn.top,
                        card.pin_btn.right - px(5.0, s),
                        card.pin_btn.bottom,
                    ),
                    theme.text,
                    TextAlign::Left,
                    false,
                );
            }

            if let Some(ref listen) = card.listen {
                draw_text(
                    hdc,
                    fonts.get(FontRole::Body11Med),
                    "Listen to this device",
                    listen.label_rect,
                    if listen.enabled {
                        theme.text
                    } else {
                        theme.text_sec
                    },
                    TextAlign::Left,
                    false,
                );
                draw_text(
                    hdc,
                    fonts.get(FontRole::Sub10_5Med),
                    &listen.target_label,
                    RectPx::new(
                        listen.target_btn.left + px(7.0, s),
                        listen.target_btn.top,
                        listen.target_btn.right - px(6.0 + 10.5 + 5.0, s),
                        listen.target_btn.bottom,
                    ),
                    if listen.has_target {
                        theme.text
                    } else {
                        theme.text_sec
                    },
                    TextAlign::Left,
                    true,
                );
                if let Some(hint_r) = listen.hint_rect {
                    if listen.has_target {
                        let routing = format!("Routing to {}", listen.target_label);
                        draw_text(
                            hdc,
                            fonts.get(FontRole::Tiny10Med),
                            &routing,
                            hint_r,
                            theme.text_sec,
                            TextAlign::Left,
                            true,
                        );
                    } else {
                        draw_text(
                            hdc,
                            fonts.get(FontRole::Tiny10Italic),
                            "Pick a device to activate",
                            hint_r,
                            theme.text_muted,
                            TextAlign::Left,
                            true,
                        );
                    }
                }
            }
        }
    }

    if saved != 0 {
        unsafe {
            let _ = RestoreDC(hdc, saved);
        }
    }

    // Footer status text
    draw_text(
        hdc,
        fonts.get(FontRole::Sub10_5Reg),
        &data.status_text,
        layout.footer.status_rect,
        if data.status_flash {
            theme.text
        } else {
            theme.text_muted
        },
        TextAlign::Left,
        true,
    );
}

fn paint_overlay_shapes(
    canvas: &mut Canvas<'_>,
    data: &WindowData,
    state: &AppState,
    overlay: &OverlayLayout,
    theme: Theme,
    s: f32,
    listen_text_w: i32,
) {
    match overlay {
        OverlayLayout::None => {}
        OverlayLayout::Config(cfg) => paint_config_overlay_shapes(canvas, data, state, cfg, theme, s),
        OverlayLayout::Profile(prof) => paint_profile_overlay_shapes(canvas, data, prof, theme, s),
        OverlayLayout::Listen(lst) => {
            paint_listen_overlay_shapes(canvas, data, state, lst, theme, s, listen_text_w);
        }
    }
}

fn paint_config_overlay_shapes(
    canvas: &mut Canvas<'_>,
    data: &WindowData,
    _state: &AppState,
    cfg: &ConfigOverlayLayout,
    theme: Theme,
    s: f32,
) {
    let bw = s.max(1.0);
    let bw_px = px(1.0, s).max(1);
    canvas.draw_shadow_round_rect(cfg.panel_rect, 10.0 * s, 14.0 * s, 4.0 * s, theme.shadow_alpha);
    canvas.fill_round_rect(
        cfg.panel_rect,
        10.0 * s,
        Some(theme.bg),
        Some((theme.border_strong, bw)),
    );

    canvas.push_clip(cfg.viewport_rect);
    for row in &cfg.rows {
        if row.rect.bottom <= cfg.viewport_rect.top || row.rect.top >= cfg.viewport_rect.bottom {
            continue;
        }
        match &row.kind {
            ConfigRowKind::Header { .. } => {}
            ConfigRowKind::CheckboxItem {
                action,
                checked,
                checkbox_box,
                ..
            } => {
                if data.hovered_action.as_ref() == Some(action) {
                    canvas.fill_round_rect(row.rect, 7.0 * s, Some(theme.bg_hover), None);
                }
                draw_checkbox(canvas, *checkbox_box, *checked, theme, s);
            }
            ConfigRowKind::ActionButton {
                action,
                inverted,
                btn_rect,
                ..
            } => {
                let hov = data.hovered_action.as_ref() == Some(action);
                if *inverted {
                    canvas.fill_round_rect(
                        *btn_rect,
                        6.0 * s,
                        Some(if hov { theme.inv_hover } else { theme.inv_bg }),
                        None,
                    );
                } else {
                    canvas.fill_round_rect(
                        *btn_rect,
                        6.0 * s,
                        Some(if hov { theme.bg_hover } else { theme.bg_elev }),
                        Some((
                            if hov {
                                theme.border_strong
                            } else {
                                theme.border
                            },
                            bw,
                        )),
                    );
                }
            }
            ConfigRowKind::Divider { y, left, right } => {
                canvas.hline(*left, *right, *y, bw_px, theme.divider);
            }
            ConfigRowKind::QuitRow {
                icon_rect,
                confirm_quit,
                ..
            } => {
                let hov = data.hovered_action.as_ref() == Some(&HitAction::ConfigQuit);
                if *confirm_quit {
                    canvas.fill_round_rect(
                        row.rect,
                        6.0 * s,
                        Some(if hov { theme.inv_hover } else { theme.inv_bg }),
                        None,
                    );
                    canvas.draw_icon_centered(
                        IconKind::Power,
                        *icon_rect,
                        13.0 * s,
                        theme.inv_text,
                    );
                } else {
                    if hov {
                        canvas.fill_round_rect(row.rect, 7.0 * s, Some(theme.bg_hover), None);
                    }
                    canvas.draw_icon_centered(
                        IconKind::Power,
                        *icon_rect,
                        13.0 * s,
                        if hov { theme.text } else { theme.text_sec },
                    );
                }
            }
        }
    }
    if cfg.max_scroll > 0 {
        draw_scrollbar(
            canvas,
            cfg.viewport_rect,
            cfg.scroll,
            cfg.max_scroll,
            cfg.content_height,
            theme,
            s,
        );
    }
    canvas.pop_clip();
}

fn paint_profile_overlay_shapes(
    canvas: &mut Canvas<'_>,
    data: &WindowData,
    prof: &ProfileOverlayLayout,
    theme: Theme,
    s: f32,
) {
    let bw = s.max(1.0);
    canvas.draw_shadow_round_rect(prof.panel_rect, 9.0 * s, 14.0 * s, 4.0 * s, theme.shadow_alpha);
    canvas.fill_round_rect(
        prof.panel_rect,
        9.0 * s,
        Some(theme.bg),
        Some((theme.border_strong, bw)),
    );

    canvas.push_clip(prof.viewport_rect);
    for row in &prof.rows {
        if row.row_rect.bottom <= prof.viewport_rect.top
            || row.row_rect.top >= prof.viewport_rect.bottom
        {
            continue;
        }
        let row_hov =
            data.hovered_action.as_ref() == Some(&HitAction::ApplyProfile(row.id.clone()));
        let del_hov =
            data.hovered_action.as_ref() == Some(&HitAction::DeleteProfile(row.id.clone()));
        let fill = if row_hov || del_hov {
            Some(theme.bg_hover)
        } else if row.active {
            Some(theme.bg_elev)
        } else {
            None
        };
        let border = row.active.then_some((theme.border, bw));
        if fill.is_some() || border.is_some() {
            canvas.fill_round_rect(row.row_rect, 6.0 * s, fill, border);
        }
        if row.active {
            canvas.draw_icon_centered(IconKind::Check, row.check_rect, 11.0 * s, theme.text);
        }
        if del_hov {
            canvas.fill_round_rect(row.delete_btn, 4.0 * s, Some(theme.bg_pill), None);
        }
        canvas.draw_icon_centered(IconKind::CloseX, row.delete_btn, 10.0 * s, theme.text_muted);
    }
    if prof.max_scroll > 0 {
        draw_scrollbar(
            canvas,
            prof.viewport_rect,
            prof.scroll,
            prof.max_scroll,
            prof.content_height,
            theme,
            s,
        );
    }
    canvas.pop_clip();
}

fn paint_listen_overlay_shapes(
    canvas: &mut Canvas<'_>,
    data: &WindowData,
    _state: &AppState,
    lst: &ListenOverlayLayout,
    theme: Theme,
    s: f32,
    listen_text_w: i32,
) {
    let bw = s.max(1.0);
    let bw_px = px(1.0, s).max(1);
    canvas.draw_shadow_round_rect(lst.panel_rect, 10.0 * s, 14.0 * s, 4.0 * s, theme.shadow_alpha);
    canvas.fill_round_rect(
        lst.panel_rect,
        10.0 * s,
        Some(theme.listen_popover_bg),
        Some((theme.border_strong, bw)),
    );

    // Search bar
    let search_focused = data.active_input == ActiveInput::ListenSearch;
    let search_hov = data.hovered_action.as_ref() == Some(&HitAction::FocusListenSearch);
    let focused_bg = match theme.mode {
        ThemeMode::Dark => theme.bg_elev,
        ThemeMode::Light => theme.bg,
    };
    canvas.fill_round_rect(
        lst.search_box,
        7.0 * s,
        Some(if search_focused {
            focused_bg
        } else {
            theme.bg_recess
        }),
        Some((
            if search_focused {
                theme.text_muted
            } else if search_hov {
                theme.border_strong
            } else {
                theme.border
            },
            bw,
        )),
    );
    let s_icon = 12.0 * s;
    canvas.draw_icon(
        IconKind::Search,
        lst.search_box.left as f32 + 8.0 * s,
        lst.search_box.top as f32 + (lst.search_box.height() as f32 - s_icon) * 0.5,
        s_icon,
        if search_focused {
            theme.text_sec
        } else {
            theme.text_muted
        },
    );
    let tl = lst.search_box.left + px(8.0 + 12.0 + 6.0, s);
    let tr = lst
        .search_clear_btn
        .map_or(lst.search_box.right - px(6.0, s), |r| r.left - px(4.0, s));
    draw_input_caret_or_selection(
        canvas,
        RectPx::new(tl, lst.search_box.top, tr, lst.search_box.bottom),
        search_focused,
        data.input_selected_all,
        data.listen_search_query.is_empty(),
        listen_text_w,
        theme,
        s,
    );
    if let Some(clear_r) = lst.search_clear_btn {
        if data.hovered_action.as_ref() == Some(&HitAction::ClearListenSearch) {
            canvas.fill_round_rect(clear_r, 4.0 * s, Some(theme.bg_hover), None);
        }
        canvas.draw_icon_centered(IconKind::CloseX, clear_r, 10.5 * s, theme.text_muted);
    }

    let active_item_bg = match theme.mode {
        ThemeMode::Dark => theme.bg_elev,
        ThemeMode::Light => theme.bg,
    };

    canvas.push_clip(lst.viewport_rect);

    if let Some(sec_rect) = lst.default_section_rect {
        canvas.fill_round_rect(sec_rect, 7.0 * s, Some(theme.listen_default_bg), None);
        for entry in &lst.default_entries {
            let hov = data.hovered_action.as_ref()
                == Some(&HitAction::SetListenTarget {
                    device_id: lst.input_device_id.clone(),
                    target_id: Some(entry.target_id.to_string()),
                });
            let fill = if hov {
                Some(theme.listen_default_hover)
            } else if entry.active {
                Some(theme.listen_default_active)
            } else {
                None
            };
            if fill.is_some() {
                canvas.fill_round_rect_corners(entry.rect, entry.radii, fill, None);
            }
            if entry.active {
                canvas.draw_icon_centered(IconKind::Check, entry.check_rect, 10.5 * s, theme.text);
            }
        }
        if let Some(div_y) = lst.default_divider_y {
            canvas.hline(
                sec_rect.left + bw_px,
                sec_rect.right - bw_px,
                div_y,
                bw_px,
                theme.listen_default_border,
            );
        }
        canvas.stroke_round_rect(sec_rect, 7.0 * s, theme.listen_default_border, bw);
    }

    if let Some(div_y) = lst.section_divider_y {
        canvas.hline(
            lst.viewport_rect.left + px(4.0, s),
            lst.viewport_rect.right - px(4.0, s),
            div_y,
            bw_px,
            theme.divider,
        );
    }

    for entry in &lst.device_entries {
        if entry.rect.bottom <= lst.viewport_rect.top || entry.rect.top >= lst.viewport_rect.bottom
        {
            continue;
        }
        let hov = data.hovered_action.as_ref()
            == Some(&HitAction::SetListenTarget {
                device_id: lst.input_device_id.clone(),
                target_id: entry.target_id.clone(),
            });
        let fill = if hov {
            Some(theme.bg_hover)
        } else if entry.active {
            Some(active_item_bg)
        } else {
            None
        };
        let border = entry.active.then_some((theme.border, bw));
        if fill.is_some() || border.is_some() {
            canvas.fill_round_rect(entry.rect, 6.0 * s, fill, border);
        }
        if entry.active {
            canvas.draw_icon_centered(IconKind::Check, entry.check_rect, 10.5 * s, theme.text);
        }
        if let Some(pill) = entry.disabled_pill {
            canvas.fill_round_rect(
                pill,
                4.0 * s,
                Some(theme.bg_recess),
                Some((theme.border, bw)),
            );
        }
    }

    if lst.max_scroll > 0 {
        draw_scrollbar(
            canvas,
            lst.viewport_rect,
            lst.scroll,
            lst.max_scroll,
            lst.content_height,
            theme,
            s,
        );
    }
    canvas.pop_clip();
}

fn paint_overlay_text(
    hdc: HDC,
    data: &WindowData,
    _state: &AppState,
    overlay: &OverlayLayout,
    theme: Theme,
    s: f32,
) {
    let fonts = &data.fonts;
    match overlay {
        OverlayLayout::None => {}
        OverlayLayout::Config(cfg) => {
            let saved = unsafe { SaveDC(hdc) };
            unsafe {
                let _ = IntersectClipRect(
                    hdc,
                    cfg.viewport_rect.left,
                    cfg.viewport_rect.top,
                    cfg.viewport_rect.right,
                    cfg.viewport_rect.bottom,
                );
            }
            for row in &cfg.rows {
                if row.rect.bottom <= cfg.viewport_rect.top
                    || row.rect.top >= cfg.viewport_rect.bottom
                {
                    continue;
                }
                match &row.kind {
                    ConfigRowKind::Header { version } => {
                        let inner = RectPx::new(
                            row.rect.left + px(6.0, s),
                            row.rect.top,
                            row.rect.right - px(6.0, s),
                            row.rect.bottom,
                        );
                        draw_text(
                            hdc,
                            fonts.get(FontRole::Tiny10Semi),
                            "SETTINGS",
                            inner,
                            theme.text_muted,
                            TextAlign::Left,
                            false,
                        );
                        draw_text(
                            hdc,
                            fonts.get(FontRole::Tiny10Reg),
                            version,
                            inner,
                            theme.text_muted,
                            TextAlign::Right,
                            false,
                        );
                    }
                    ConfigRowKind::CheckboxItem {
                        title,
                        subtitle,
                        title_rect,
                        subtitle_rect,
                        ..
                    } => {
                        draw_text(
                            hdc,
                            fonts.get(FontRole::Body11_5Med),
                            title,
                            *title_rect,
                            theme.text,
                            TextAlign::Left,
                            true,
                        );
                        if let (Some(sub), Some(sr)) = (subtitle, subtitle_rect) {
                            draw_wrapped_text(
                                hdc,
                                fonts.get(FontRole::Sub10_5Reg),
                                sub,
                                *sr,
                                theme.text_muted,
                            );
                        }
                    }
                    ConfigRowKind::ActionButton {
                        label,
                        inverted,
                        btn_rect,
                        ..
                    } => {
                        draw_text(
                            hdc,
                            fonts.get(if *inverted {
                                FontRole::Body11Semi
                            } else {
                                FontRole::Body11Med
                            }),
                            label,
                            *btn_rect,
                            if *inverted {
                                theme.inv_text
                            } else {
                                theme.text
                            },
                            TextAlign::Center,
                            true,
                        );
                    }
                    ConfigRowKind::Divider { .. } => {}
                    ConfigRowKind::QuitRow {
                        label_rect,
                        confirm_quit,
                        ..
                    } => {
                        let hov = data.hovered_action.as_ref() == Some(&HitAction::ConfigQuit);
                        let (font, label, color) = if *confirm_quit {
                            (FontRole::Body11_5Semi, "Confirm Quit?", theme.inv_text)
                        } else {
                            (
                                FontRole::Body11_5Med,
                                "Quit SETTMetter",
                                if hov { theme.text } else { theme.text_sec },
                            )
                        };
                        draw_text(
                            hdc,
                            fonts.get(font),
                            label,
                            *label_rect,
                            color,
                            TextAlign::Left,
                            false,
                        );
                    }
                }
            }
            if saved != 0 {
                unsafe {
                    let _ = RestoreDC(hdc, saved);
                }
            }
        }
        OverlayLayout::Profile(prof) => {
            let saved = unsafe { SaveDC(hdc) };
            unsafe {
                let _ = IntersectClipRect(
                    hdc,
                    prof.viewport_rect.left,
                    prof.viewport_rect.top,
                    prof.viewport_rect.right,
                    prof.viewport_rect.bottom,
                );
            }
            if let Some(empty_r) = prof.empty_rect {
                let mid = empty_r.top + empty_r.height() / 2;
                draw_text(
                    hdc,
                    fonts.get(FontRole::Body11_5Med),
                    "No profiles",
                    RectPx::new(empty_r.left, empty_r.top + px(8.0, s), empty_r.right, mid),
                    theme.text_sec,
                    TextAlign::Center,
                    false,
                );
                draw_text(
                    hdc,
                    fonts.get(FontRole::Tiny10Reg),
                    "Click + Save to create one",
                    RectPx::new(empty_r.left, mid, empty_r.right, empty_r.bottom - px(8.0, s)),
                    theme.text_muted,
                    TextAlign::Center,
                    false,
                );
            } else {
                for row in &prof.rows {
                    if row.row_rect.bottom <= prof.viewport_rect.top
                        || row.row_rect.top >= prof.viewport_rect.bottom
                    {
                        continue;
                    }
                    let hov = data.hovered_action.as_ref()
                        == Some(&HitAction::ApplyProfile(row.id.clone()));
                    draw_text(
                        hdc,
                        fonts.get(if row.active {
                            FontRole::Body11_5Semi
                        } else {
                            FontRole::Body11_5Med
                        }),
                        &row.name,
                        row.label_rect,
                        if row.active || hov {
                            theme.text
                        } else {
                            theme.text_sec
                        },
                        TextAlign::Left,
                        true,
                    );
                }
            }
            if saved != 0 {
                unsafe {
                    let _ = RestoreDC(hdc, saved);
                }
            }
        }
        OverlayLayout::Listen(lst) => {
            let search_focused = data.active_input == ActiveInput::ListenSearch;
            let has_query = !data.listen_search_query.is_empty();
            let selected = search_focused && data.input_selected_all && has_query;
            let tl = lst.search_box.left
                + px(8.0 + 12.0 + 6.0, s)
                + if search_focused && !has_query {
                    px(3.0, s)
                } else {
                    0
                };
            let tr = lst
                .search_clear_btn
                .map_or(lst.search_box.right - px(6.0, s), |r| r.left - px(4.0, s));
            let (txt, col) = if !has_query {
                ("Search playback devices...", theme.text_muted)
            } else if selected {
                (data.listen_search_query.as_str(), theme.inv_text)
            } else {
                (data.listen_search_query.as_str(), theme.text)
            };
            draw_text(
                hdc,
                fonts.get(FontRole::Body11_5Reg),
                txt,
                RectPx::new(tl, lst.search_box.top, tr, lst.search_box.bottom),
                col,
                TextAlign::Left,
                true,
            );

            let saved = unsafe { SaveDC(hdc) };
            unsafe {
                let _ = IntersectClipRect(
                    hdc,
                    lst.viewport_rect.left,
                    lst.viewport_rect.top,
                    lst.viewport_rect.right,
                    lst.viewport_rect.bottom,
                );
            }

            for entry in &lst.default_entries {
                draw_text(
                    hdc,
                    fonts.get(if entry.active {
                        FontRole::Body11_5Semi
                    } else {
                        FontRole::Body11_5Med
                    }),
                    entry.title,
                    entry.title_rect,
                    theme.text,
                    TextAlign::Left,
                    true,
                );
                if let (Some(sub), Some(sr)) = (&entry.subtext, entry.subtext_rect) {
                    draw_text(
                        hdc,
                        fonts.get(FontRole::Tiny10Reg),
                        sub,
                        sr,
                        theme.text_muted,
                        TextAlign::Left,
                        true,
                    );
                }
            }

            for entry in &lst.device_entries {
                if entry.rect.bottom <= lst.viewport_rect.top
                    || entry.rect.top >= lst.viewport_rect.bottom
                {
                    continue;
                }
                let title_col = if entry.active {
                    theme.text
                } else if entry.disabled {
                    theme.text_muted
                } else {
                    theme.text_sec
                };
                draw_text(
                    hdc,
                    fonts.get(if entry.active {
                        FontRole::Body11_5Semi
                    } else {
                        FontRole::Body11_5Med
                    }),
                    &entry.title,
                    entry.title_rect,
                    title_col,
                    TextAlign::Left,
                    true,
                );
                if let Some(pill) = entry.disabled_pill {
                    draw_text(
                        hdc,
                        fonts.get(FontRole::Micro9_5Med),
                        "Disabled",
                        pill,
                        theme.text_muted,
                        TextAlign::Center,
                        false,
                    );
                }
                if let (Some(sub), Some(sr)) = (&entry.subtitle, entry.subtitle_rect) {
                    draw_text(
                        hdc,
                        fonts.get(FontRole::Tiny10Reg),
                        sub,
                        sr,
                        theme.text_muted,
                        TextAlign::Left,
                        true,
                    );
                }
            }

            if let Some(empty_r) = lst.empty_rect {
                draw_text(
                    hdc,
                    fonts.get(FontRole::Body11Med),
                    "No matching devices",
                    empty_r,
                    theme.text_muted,
                    TextAlign::Center,
                    false,
                );
            }

            if saved != 0 {
                unsafe {
                    let _ = RestoreDC(hdc, saved);
                }
            }
        }
    }
}

fn draw_checkbox(canvas: &mut Canvas<'_>, rect: RectPx, checked: bool, theme: Theme, s: f32) {
    let bw = s.max(1.0);
    if checked {
        canvas.fill_round_rect(rect, 3.5 * s, Some(theme.inv_bg), Some((theme.inv_bg, bw)));
        canvas.draw_icon_centered(IconKind::Check, rect, 10.0 * s, theme.inv_text);
    } else {
        canvas.fill_round_rect(rect, 3.5 * s, None, Some((theme.check_border, bw)));
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_input_caret_or_selection(
    canvas: &mut Canvas<'_>,
    text_area: RectPx,
    focused: bool,
    selected_all: bool,
    is_empty: bool,
    text_width: i32,
    theme: Theme,
    s: f32,
) {
    if !focused || text_area.is_empty() {
        return;
    }
    let caret_h = px(12.0, s).min(text_area.height());
    let caret_top = text_area.top + (text_area.height() - caret_h) / 2;
    let caret_w = px(1.0, s).max(1);

    if selected_all && !is_empty {
        let sel_w = (text_width + px(4.0, s)).min(text_area.width());
        let sel_h = px(15.0, s).min(text_area.height() - px(4.0, s)).max(caret_h);
        let sel_top = text_area.top + (text_area.height() - sel_h) / 2;
        let sel_rect = RectPx::from_xywh(text_area.left - px(2.0, s), sel_top, sel_w, sel_h);
        canvas.fill_round_rect(sel_rect, 2.0 * s, Some(theme.inv_bg), None);
    } else {
        let cx = if is_empty {
            text_area.left
        } else {
            (text_area.left + text_width + px(1.0, s)).min(text_area.right - caret_w)
        };
        canvas.fill_rect(RectPx::from_xywh(cx, caret_top, caret_w, caret_h), theme.text);
    }
}

fn draw_scrollbar(
    canvas: &mut Canvas<'_>,
    viewport: RectPx,
    scroll: i32,
    max_scroll: i32,
    content_height: i32,
    theme: Theme,
    s: f32,
) {
    if max_scroll <= 0 || content_height <= viewport.height() || viewport.height() <= 0 {
        return;
    }
    let bar_w = px(3.0, s).max(2);
    let pad_y = px(3.0, s);
    let track_h = (viewport.height() - pad_y * 2).max(12);
    let ratio = (viewport.height() as f32 / content_height as f32).clamp(0.1, 1.0);
    let thumb_h = ((track_h as f32) * ratio).round().max(px(24.0, s) as f32) as i32;
    let thumb_h = thumb_h.min(track_h);
    let travel = (track_h - thumb_h).max(0);
    let offset = ((scroll as f32 / max_scroll as f32) * travel as f32).round() as i32;
    let x = viewport.right - bar_w - px(2.5, s);
    let y = viewport.top + pad_y + offset;
    let rect = RectPx::from_xywh(x, y, bar_w, thumb_h);
    canvas.fill_round_rect(rect, bar_w as f32 * 0.5, Some(theme.border_strong), None);
}

