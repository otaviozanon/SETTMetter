//! Physical disk SSD/HDD temperature via `IOCTL_STORAGE_QUERY_PROPERTY`, with the sensor values
//! bounds-checked before they're reported.

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Ioctl::*;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;

/// Physical disk handle for SSD/HDD temperature, opened for the Windows system volume's disk
/// number.
pub(crate) struct Disk {
    handle: HANDLE,
}

impl Disk {
    pub(crate) fn open() -> Option<Disk> {
        let mut win_dir = [0u16; 260];
        if unsafe { GetWindowsDirectoryW(win_dir.as_mut_ptr(), win_dir.len() as u32) } < 2 {
            return None;
        }
        let drive_letter = char::from_u32(win_dir[0] as u32)?;
        let sys_drive = crate::wide(&format!(r"\\.\{drive_letter}:"));
        let mut disk_num = 0u32;
        let vol = unsafe {
            CreateFileW(sys_drive.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, 0, null_mut())
        };
        if vol != INVALID_HANDLE_VALUE {
            let mut buf = [0u64; 32]; // u64-aligned storage for VOLUME_DISK_EXTENTS
            let mut got = 0u32;
            if unsafe {
                DeviceIoControl(
                    vol,
                    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                    null(),
                    0,
                    buf.as_mut_ptr() as *mut c_void,
                    (buf.len() * 8) as u32,
                    &mut got,
                    null_mut(),
                )
            } != 0
            {
                let extents = unsafe { &*(buf.as_ptr() as *const VOLUME_DISK_EXTENTS) };
                disk_num = extents.Extents[0].DiskNumber;
            }
            unsafe { CloseHandle(vol) };
        }
        let path = crate::wide(&format!(r"\\.\PhysicalDrive{disk_num}"));
        let handle = unsafe {
            CreateFileW(path.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, 0, null_mut())
        };
        (handle != INVALID_HANDLE_VALUE).then_some(Disk { handle })
    }

    /// Max reported temperature across sensors, °C, bounds-checked.
    pub(crate) fn temp(&self) -> Option<f64> {
        let mut query: STORAGE_PROPERTY_QUERY = unsafe { std::mem::zeroed() };
        query.PropertyId = StorageDeviceTemperatureProperty;
        query.QueryType = 0; // PropertyStandardQuery
        let mut buf = [0u32; 128]; // u32-aligned storage for STORAGE_TEMPERATURE_DATA_DESCRIPTOR
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                &query as *const _ as *const c_void,
                std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 4) as u32,
                &mut got,
                null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        let desc = unsafe { &*(buf.as_ptr() as *const STORAGE_TEMPERATURE_DATA_DESCRIPTOR) };
        if (got as usize) < std::mem::size_of::<STORAGE_TEMPERATURE_DATA_DESCRIPTOR>() || desc.InfoCount == 0 {
            return None;
        }
        let infos = unsafe {
            std::slice::from_raw_parts(desc.TemperatureInfo.as_ptr(), (desc.InfoCount as usize).min(
                (got as usize - std::mem::offset_of!(STORAGE_TEMPERATURE_DATA_DESCRIPTOR, TemperatureInfo))
                    / std::mem::size_of::<STORAGE_TEMPERATURE_INFO>(),
            ))
        };
        let max = infos.iter().map(|i| i.Temperature as i32).max()?;
        (max > 0 && max < 150).then_some(max as f64)
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}
