//! System-wide mute shortcut.
//!
//! A low-level keyboard hook watches the configured key (single key, optionally with modifiers)
//! and toggles the Windows microphone. The key is *observed*, never consumed, so an app that
//! binds the same key (Discord's `Break` by default) reacts at the same time. Clicking the
//! widget's mic icon sends the same key, so both stay together.

use std::mem::size_of;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED, PostMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

/// Posted to the controller window when the shortcut is pressed.
pub const WM_APP_MUTE_KEY: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 5;

pub const MOD_CTRL: u32 = 1;
pub const MOD_SHIFT: u32 = 2;
pub const MOD_ALT: u32 = 4;
pub const MOD_WIN: u32 = 8;

static HOOK: AtomicIsize = AtomicIsize::new(0);
static CONTROLLER: AtomicIsize = AtomicIsize::new(0);

/// Installs the low-level keyboard hook on the calling (UI) thread.
pub fn install(controller: isize) {
    CONTROLLER.store(controller, Ordering::Relaxed);
    if HOOK.load(Ordering::Relaxed) != 0 {
        return;
    }
    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(hook_proc),
            GetModuleHandleW(std::ptr::null()),
            0,
        )
    };
    HOOK.store(hook as isize, Ordering::Relaxed);
}

pub fn uninstall() {
    let hook = HOOK.swap(0, Ordering::Relaxed);
    if hook != 0 {
        unsafe {
            UnhookWindowsHookEx(hook as *mut _);
        }
    }
}

/// Modifier keys currently held, as a [`MOD_*`] bitmask.
pub fn current_modifiers() -> u32 {
    let held = |vk: u16| unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 };
    let mut mods = 0;
    if held(VK_CONTROL) {
        mods |= MOD_CTRL;
    }
    if held(VK_SHIFT) {
        mods |= MOD_SHIFT;
    }
    if held(VK_MENU) {
        mods |= MOD_ALT;
    }
    if held(VK_LWIN) || held(VK_RWIN) {
        mods |= MOD_WIN;
    }
    mods
}

/// True for the modifier keys themselves, which cannot be a shortcut on their own.
pub fn is_modifier(vk: u16) -> bool {
    matches!(vk, 0x10..=0x12 | 0x5B | 0x5C | 0xA0..=0xA5)
}

/// Human-readable name for a configured shortcut, e.g. `Ctrl + Shift + M`.
pub fn shortcut_label(shortcut: Option<(u32, u32)>) -> String {
    let Some((vk, mods)) = shortcut else {
        return "None".to_string();
    };
    let mut parts = Vec::new();
    if mods & MOD_CTRL != 0 {
        parts.push("Ctrl".to_string());
    }
    if mods & MOD_SHIFT != 0 {
        parts.push("Shift".to_string());
    }
    if mods & MOD_ALT != 0 {
        parts.push("Alt".to_string());
    }
    if mods & MOD_WIN != 0 {
        parts.push("Win".to_string());
    }
    parts.push(key_name(vk as u16));
    parts.join(" + ")
}

fn key_name(vk: u16) -> String {
    match vk {
        0x13 => "Break".to_string(),
        0x14 => "Caps Lock".to_string(),
        0x91 => "Scroll Lock".to_string(),
        0x2C => "Print Screen".to_string(),
        0x20 => "Space".to_string(),
        0x09 => "Tab".to_string(),
        0x0D => "Enter".to_string(),
        0x1B => "Escape".to_string(),
        0x08 => "Backspace".to_string(),
        0x25..=0x28 => ["Left", "Up", "Right", "Down"][(vk - 0x25) as usize].to_string(),
        0x2D => "Insert".to_string(),
        0x2E => "Delete".to_string(),
        0x24 => "Home".to_string(),
        0x23 => "End".to_string(),
        0x21 => "Page Up".to_string(),
        0x22 => "Page Down".to_string(),
        0x60..=0x69 => format!("Num {}", vk - 0x60),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x41..=0x5A => ((vk - 0x41 + b'A' as u16) as u8 as char).to_string(),
        0x30..=0x39 => ((vk - 0x30 + b'0' as u16) as u8 as char).to_string(),
        _ => format!("0x{vk:02X}"),
    }
}

/// True when the Discord client is running.
pub fn discord_running() -> bool {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        let mut ok = Process32FirstW(snapshot, &mut entry) != 0;
        while ok {
            let len = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case("Discord.exe")
            {
                found = true;
                break;
            }
            ok = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        found
    }
}

/// Sends the configured shortcut, so apps bound to the same key (Discord) follow the click.
pub fn send_shortcut(vk: u32, mods: u32) {
    let mut inputs = Vec::with_capacity(8);
    let mut modifiers = Vec::new();
    if mods & MOD_CTRL != 0 {
        modifiers.push(VK_CONTROL);
    }
    if mods & MOD_SHIFT != 0 {
        modifiers.push(VK_SHIFT);
    }
    if mods & MOD_ALT != 0 {
        modifiers.push(VK_MENU);
    }
    if mods & MOD_WIN != 0 {
        modifiers.push(VK_LWIN);
    }
    for modifier in &modifiers {
        inputs.push(key_input(*modifier, false));
    }
    inputs.push(key_input(vk as u16, false));
    inputs.push(key_input(vk as u16, true));
    for modifier in modifiers.iter().rev() {
        inputs.push(key_input(*modifier, true));
    }
    unsafe {
        let _ = SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        );
    }
}

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && (wparam == WM_KEYDOWN as usize || wparam == WM_SYSKEYDOWN as usize)
    {
        let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
        let injected = kb.flags & LLKHF_INJECTED != 0;
        if !injected
            && let Some((vk, mods)) = crate::settings::mute_shortcut()
            && kb.vkCode == vk
            && current_modifiers() == mods
        {
            let controller = CONTROLLER.load(Ordering::Relaxed);
            if controller != 0 {
                PostMessageW(controller as *mut _, WM_APP_MUTE_KEY, 0, 0);
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}
