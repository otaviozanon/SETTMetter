//! PawnIO-backed MSR/SMN reader for CPU die temperature (Intel/AMD), ported from TraySense's
//! `pio_init`/`pio_temp`.

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::DeviceIoControl;

const INTEL_BIN: &[u8] = include_bytes!("../../assets/pawnio/IntelMSR.bin");
const AMD_BIN: &[u8] = include_bytes!("../../assets/pawnio/AMDFamily17.bin");

const IOCTL_PIO_LOAD: u32 = 0xA1B2_2084;
const IOCTL_PIO_EXEC: u32 = 0xA1B2_2104;

/// CPU vendor detected via CPUID leaf 0 EBX, same check as TraySense (`pio_init`).
fn cpu_vendor() -> u8 {
    let ebx = std::arch::x86_64::__cpuid(0).ebx;
    if ebx == 0x756e_6547 {
        1 // GenuineIntel
    } else if ebx == 0x6874_7541 {
        2 // AuthenticAMD
    } else {
        0
    }
}

pub(crate) struct PawnIo {
    handle: HANDLE,
    vendor: u8,
    tjmax: i32,
}

impl PawnIo {
    pub(crate) fn open() -> Option<PawnIo> {
        let vendor = cpu_vendor();
        if vendor == 0 {
            return None;
        }
        let path = crate::wide(r"\\?\GLOBALROOT\Device\PawnIO");
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                (GENERIC_READ | GENERIC_WRITE) as u32,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None; // no driver installed, or not running elevated
        }
        let module = if vendor == 1 { INTEL_BIN } else { AMD_BIN };
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_PIO_LOAD,
                module.as_ptr() as *const c_void,
                module.len() as u32,
                null_mut(),
                0,
                &mut got,
                null_mut(),
            )
        };
        if ok == 0 {
            unsafe { CloseHandle(handle) };
            return None;
        }
        let mut tjmax = 100;
        if vendor == 1 {
            if let Some(v) = Self::exec_raw(handle, "ioctl_read_msr", 0x1A2) {
                let t = ((v >> 16) & 0xFF) as i32;
                if (60..=130).contains(&t) {
                    tjmax = t;
                }
            }
        }
        Some(PawnIo { handle, vendor, tjmax })
    }

    fn exec_raw(handle: HANDLE, func: &str, arg: u64) -> Option<u64> {
        let mut buf = [0u8; 40];
        let name = func.as_bytes();
        buf[..name.len().min(31)].copy_from_slice(&name[..name.len().min(31)]);
        buf[32..40].copy_from_slice(&arg.to_le_bytes());
        let mut out: u64 = 0;
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_PIO_EXEC,
                buf.as_ptr() as *const c_void,
                buf.len() as u32,
                &mut out as *mut u64 as *mut c_void,
                8,
                &mut got,
                null_mut(),
            )
        };
        (ok != 0 && got >= 8).then_some(out)
    }

    /// Returns a plausible die temperature in °C, same formulas as TraySense `pio_temp`.
    pub(crate) fn temp(&self) -> Option<f64> {
        let t = if self.vendor == 1 {
            let v = Self::exec_raw(self.handle, "ioctl_read_msr", 0x1B1)?;
            self.tjmax as f64 - ((v >> 16) & 0x7F) as f64
        } else {
            let v = Self::exec_raw(self.handle, "ioctl_read_smn", 0x0005_9800)?;
            let mut c = ((v >> 21) & 0x7FF) as f64 / 8.0;
            if v & 0x8_0000 != 0 {
                c -= 49.0;
            }
            c
        };
        (t > 0.0 && t < 125.0).then_some(t)
    }
}

impl Drop for PawnIo {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}
