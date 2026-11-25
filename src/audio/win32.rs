use winaudio::Win32::Foundation::{HWND, LPARAM, WPARAM};
use winaudio::Win32::System::Memory::{SETPROCESSWORKINGSETSIZEEX_FLAGS, SetProcessWorkingSetSizeEx};
use winaudio::Win32::System::Threading::GetCurrentProcess;
use winaudio::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_USER};

pub const WM_APP_AUDIO_REFRESH: u32 = WM_USER + 1;
/// The widget's controller window handles these (keep in sync with src/widget/mod.rs).
pub const WM_APP_MENU: u32 = 0x8002;
pub const WM_APP_EXIT: u32 = 0x8003;

pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn trim_working_set() {
    unsafe {
        let _ = SetProcessWorkingSetSizeEx(
            GetCurrentProcess(),
            usize::MAX,
            usize::MAX,
            SETPROCESSWORKINGSETSIZEEX_FLAGS(0),
        );
    }
}

#[derive(Clone, Copy)]
pub struct WakeHandle(isize);

impl WakeHandle {
    pub fn new(hwnd: isize) -> Self {
        Self(hwnd)
    }

    pub fn post(self, message: u32) {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.0 as *mut core::ffi::c_void)), message, WPARAM(0), LPARAM(0));
        }
    }
}
