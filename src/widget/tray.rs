//! Fallback notification-area icon.
//!
//! Shown only when every device and sensor is hidden, so the right-click config menu stays
//! reachable even though the taskbar widget has nothing to draw (and thus no clickable area).

use crate::wide;
use std::mem::{size_of, zeroed};
use std::ptr::null;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Whether the fallback tray icon is currently shown.
static TRAY_SHOWN: AtomicBool = AtomicBool::new(false);

/// Right-clicking it posts the same `WM_APP_MENU` the widget itself posts.
pub(super) unsafe fn set_tray_icon(show: bool) {
    if TRAY_SHOWN.load(Ordering::Relaxed) == show {
        return;
    }
    let controller = super::with(|a| a.controller);
    if controller.is_null() {
        return;
    }
    let mut nid: NOTIFYICONDATAW = zeroed();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = controller;
    nid.uID = 1;
    if show {
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = super::WM_APP_MENU;
        nid.hIcon = LoadIconW(GetModuleHandleW(null()), std::ptr::dangling::<u16>()); // resource id 1, see build.rs
        let tip = wide("SETTMetter (all sensors hidden)");
        let len = tip.len().min(nid.szTip.len() - 1);
        nid.szTip[..len].copy_from_slice(&tip[..len]);
        Shell_NotifyIconW(NIM_ADD, &nid);
    } else {
        Shell_NotifyIconW(NIM_DELETE, &nid);
    }
    TRAY_SHOWN.store(show, Ordering::Relaxed);
}
