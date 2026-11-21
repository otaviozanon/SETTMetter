//! PawnIO-backed MSR/SMN reader for CPU die temperature (Intel/AMD).

use crate::diag;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::DeviceIoControl;

const INTEL_BIN: &[u8] = include_bytes!("../../assets/pawnio/IntelMSR.bin");
const AMD_BIN: &[u8] = include_bytes!("../../assets/pawnio/AMDFamily17.bin");

const IOCTL_PIO_LOAD: u32 = 0xA1B2_2084;
const IOCTL_PIO_EXEC: u32 = 0xA1B2_2104;

/// CPU vendor detected via CPUID leaf 0 EBX.
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

/// The CPU's marketing name (CPUID leaves 0x80000002..4), e.g. "AMD Ryzen 7 9800X3D 8-Core
/// Processor", for naming the CPU in the driver prompt.
pub(crate) fn cpu_brand() -> String {
    let max = std::arch::x86_64::__cpuid(0x8000_0000).eax;
    if max < 0x8000_0004 {
        return String::new();
    }
    let mut bytes = [0u8; 48];
    for (i, leaf) in [0x8000_0002u32, 0x8000_0003, 0x8000_0004].into_iter().enumerate() {
        let r = std::arch::x86_64::__cpuid(leaf);
        for (j, word) in [r.eax, r.ebx, r.ecx, r.edx].into_iter().enumerate() {
            bytes[i * 16 + j * 4..i * 16 + j * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
    }
    String::from_utf8_lossy(&bytes).trim_matches(|c: char| c == '\0' || c == ' ').to_string()
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
            diag::event("cpu temp", "unknown CPU vendor, no MSR/SMN source");
            return None;
        }
        let path = crate::wide(r"\\?\GLOBALROOT\Device\PawnIO");
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // Counts as "no driver installed, or not running elevated".
            let e = unsafe { GetLastError() };
            diag::event("cpu temp", format!("PawnIO driver unavailable (error {e}), install PawnIO for CPU temperature"));
            return None;
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
            let e = unsafe { GetLastError() };
            diag::event("cpu temp", format!("PawnIO module load failed (error {e})"));
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
        let pio = PawnIo { handle, vendor, tjmax };
        // One reading here is the only chance to say which source answered before the values
        // are deduplicated away by the per-sample log.
        let source = if vendor == 1 { "MSR 0x1B1" } else { "SMN 0x59800" };
        match pio.temp() {
            Some(t) => diag::event("cpu temp", format!("{t:.1} C via PawnIO {source}")),
            None => diag::event("cpu temp", format!("PawnIO {source} gave no plausible value")),
        }
        Some(pio)
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

    /// Installs PawnIO from its winget package (`namazso.PawnIO`). The app doesn't bundle the
    /// installer: it is freeware and the driver GPL-2.0, so fetching it from the package manager
    /// keeps redistribution out of this project and lets winget check the installer hash.
    pub(crate) fn install() -> Result<(), String> {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let status = Command::new("winget")
            .args([
                "install",
                "--id",
                "namazso.PawnIO",
                "--exact",
                "--silent",
                "--accept-package-agreements",
                "--accept-source-agreements",
                "--disable-interactivity",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_err(|e| format!("winget unavailable: {e}"))?;
        match status.code() {
            Some(0) => Ok(()),
            // Success: Windows only wants a restart before the driver loads.
            Some(3010) => {
                diag::event("cpu temp", "PawnIO installed, restart required");
                Ok(())
            }
            Some(c) => Err(format!("winget exited with {c}")),
            None => Err("winget was terminated".into()),
        }
    }

    /// Returns a plausible die temperature in °C.
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
