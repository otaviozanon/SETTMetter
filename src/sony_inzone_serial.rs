//! Sony INZONE H9 / H7 first generation (054C:0E53) over the dongle's USB serial port.
//!
//! 460800 8N1, bare Sony HCI packets. Battery GET: `01 00 FC 08 96 C3 41 04 01 <tid> 00 <sum>`;
//! reply event `04 FF <len> 00 96 C3 <addr> 04 10 <tid> [charging][percent] <sum>`.
//! Read-only. Port is opened with share 0, so INZONE Hub wins while it is running.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/sony_inzone.cpp / protocol.h).

use crate::device::{Kind, Observation, State};
use crate::wide;
use hidapi::HidApi;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::Devices::Communication::{
    COMMTIMEOUTS, DCB, GetCommState, NOPARITY, ONESTOPBIT, PURGE_RXCLEAR, PURGE_TXCLEAR, PurgeComm, SetCommState, SetCommTimeouts,
};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DEV, GUID_DEVCLASS_PORTS, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiOpenDevRegKey,
};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING, ReadFile, WriteFile};
use windows_sys::Win32::System::Registry::{KEY_READ, REG_NONE, REG_SZ, RegCloseKey, RegQueryValueExW};

const KEY: &str = "inzone:0e53";
const BAUD: u32 = 460800;
const DTR_CONTROL_ENABLE: u32 = 1;
const RTS_CONTROL_ENABLE: u32 = 1;

static TID: AtomicU16 = AtomicU16::new(1);

#[derive(Default)]
pub struct SonyInzoneSerial {
    seen: bool,
}

impl SonyInzoneSerial {
    pub fn scan(&mut self, _api: &HidApi) -> Vec<Observation> {
        for port in ports() {
            let Some(payload) = read_battery(&port) else { continue };
            if payload.len() < 2 || payload[1] == 0xFF || payload[1] > 100 {
                continue;
            }
            self.seen = true;
            return vec![Observation {
                id: KEY.into(),
                name: Some("Sony INZONE H9 / H7".into()),
                kind: Some(Kind::Headset),
                state: State::Battery { percent: payload[1], charging: payload[0] != 0 },
            }];
        }
        if self.seen {
            vec![Observation {
                id: KEY.into(),
                name: Some("Sony INZONE H9 / H7".into()),
                kind: Some(Kind::Headset),
                state: State::Unreachable,
            }]
        } else {
            Vec::new()
        }
    }
}

fn ports() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let set = SetupDiGetClassDevsW(&GUID_DEVCLASS_PORTS, null(), null_mut(), DIGCF_PRESENT);
        if set as isize == -1 {
            return out;
        }
        let mut i = 0;
        loop {
            let mut data: SP_DEVINFO_DATA = std::mem::zeroed();
            data.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiEnumDeviceInfo(set, i, &mut data) == 0 {
                break;
            }
            i += 1;
            let mut id = [0u16; 512];
            if SetupDiGetDeviceInstanceIdW(set, &data, id.as_mut_ptr(), id.len() as u32, null_mut()) == 0 {
                continue;
            }
            let len = id.iter().position(|c| *c == 0).unwrap_or(id.len());
            let id = String::from_utf16_lossy(&id[..len]).to_ascii_uppercase();
            if !id.contains("VID_054C&PID_0E53") {
                continue;
            }
            let key = SetupDiOpenDevRegKey(set, &data, DICS_FLAG_GLOBAL, 0, DIREG_DEV, KEY_READ);
            if key == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut name = [0u16; 64];
            let mut kind = REG_NONE;
            let mut size = (name.len() * 2) as u32;
            let r = RegQueryValueExW(key, wide("PortName").as_ptr(), null(), &mut kind, name.as_mut_ptr() as *mut u8, &mut size);
            RegCloseKey(key);
            if r != 0 || kind != REG_SZ {
                continue;
            }
            let len = name.iter().position(|c| *c == 0).unwrap_or(name.len());
            let name = String::from_utf16_lossy(&name[..len]);
            if !name.is_empty() {
                out.push(name);
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

fn read_battery(port: &str) -> Option<Vec<u8>> {
    let path = wide(&format!("\\\\.\\{port}"));
    let handle: HANDLE = unsafe {
        CreateFileW(path.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING, 0, std::ptr::null_mut())
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    unsafe {
        let mut dcb: DCB = std::mem::zeroed();
        dcb.DCBlength = size_of::<DCB>() as u32;
        let mut timeouts: COMMTIMEOUTS = std::mem::zeroed();
        timeouts.ReadIntervalTimeout = u32::MAX;
        timeouts.ReadTotalTimeoutMultiplier = u32::MAX;
        timeouts.ReadTotalTimeoutConstant = 300;
        timeouts.WriteTotalTimeoutConstant = 1000;
        let ok = GetCommState(handle, &mut dcb) != 0
            && {
                dcb.BaudRate = BAUD;
                dcb.ByteSize = 8;
                dcb.Parity = NOPARITY;
                dcb.StopBits = ONESTOPBIT;
                dcb._bitfield = dcb_flags(dcb._bitfield);
                SetCommState(handle, &dcb) != 0
            }
            && SetCommTimeouts(handle, &timeouts) != 0;
        if !ok {
            CloseHandle(handle);
            return None;
        }
        PurgeComm(handle, PURGE_RXCLEAR | PURGE_TXCLEAR);

        let tid = TID.fetch_add(1, Ordering::Relaxed).max(1);
        let mut req = vec![0x01, 0x00, 0xFC, 8, 0x96, 0xC3, 0x41, 0x04, 0x01, tid as u8, (tid >> 8) as u8, 0];
        let sum = req[4..req.len() - 1].iter().fold(0u8, |s, b| s.wrapping_add(*b));
        *req.last_mut().unwrap() = sum;
        let mut written = 0u32;
        if WriteFile(handle, req.as_ptr(), req.len() as u32, &mut written, null_mut()) == 0 || written as usize != req.len() {
            CloseHandle(handle);
            return None;
        }

        let deadline = Instant::now() + Duration::from_millis(1500);
        let mut stream = Vec::new();
        while Instant::now() < deadline {
            let mut chunk = [0u8; 256];
            let mut read = 0u32;
            if ReadFile(handle, chunk.as_mut_ptr(), chunk.len() as u32, &mut read, null_mut()) == 0 {
                break;
            }
            stream.extend_from_slice(&chunk[..read as usize]);
            if let Some(payload) = next_battery(&mut stream, tid) {
                CloseHandle(handle);
                return Some(payload);
            }
            if read == 0 {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        CloseHandle(handle);
    }
    None
}

fn dcb_flags(mut bits: u32) -> u32 {
    bits |= 1; // fBinary
    bits &= !(1 << 2); // fOutxCtsFlow
    bits &= !(1 << 3); // fOutxDsrFlow
    bits &= !(1 << 8); // fOutX
    bits &= !(1 << 9); // fInX
    bits = (bits & !(0b11 << 4)) | (DTR_CONTROL_ENABLE << 4);
    bits = (bits & !(0b11 << 12)) | (RTS_CONTROL_ENABLE << 12);
    bits
}

fn next_battery(stream: &mut Vec<u8>, tid: u16) -> Option<Vec<u8>> {
    loop {
        if stream.is_empty() {
            return None;
        }
        let total = if stream[0] == 0x04 {
            if stream.len() < 3 {
                return None;
            }
            3 + usize::from(stream[2])
        } else if stream[0] == 0x01 {
            if stream.len() < 4 {
                return None;
            }
            4 + usize::from(stream[3])
        } else {
            stream.remove(0);
            continue;
        };
        if stream.len() < total {
            return None;
        }
        let frame = stream.drain(..total).collect::<Vec<_>>();
        if let Some(payload) = parse_event(&frame, tid) {
            return Some(payload);
        }
    }
}

fn parse_event(f: &[u8], tid: u16) -> Option<Vec<u8>> {
    if f.len() < 12 || f[0] != 0x04 || f[1] != 0xFF || f[3] != 0x00 || f[4] != 0x96 || f[5] != 0xC3 {
        return None;
    }
    if f[6] >> 4 != 1 {
        return None;
    }
    let sum = f[3..f.len() - 1].iter().fold(0u8, |s, b| s.wrapping_add(*b));
    if sum != f[f.len() - 1] {
        return None;
    }
    if f[7] != 0x04 || f[8] != 0x10 || u16::from(f[9]) | (u16::from(f[10]) << 8) != tid {
        return None;
    }
    Some(f[11..f.len() - 1].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_get_checksum() {
        let tid = 1u16;
        let mut req = vec![0x01, 0x00, 0xFC, 8, 0x96, 0xC3, 0x41, 0x04, 0x01, tid as u8, (tid >> 8) as u8, 0];
        let sum = req[4..11].iter().fold(0u8, |s, b| s.wrapping_add(*b));
        *req.last_mut().unwrap() = sum;
        assert_eq!(*req.last().unwrap(), sum);
    }

    #[test]
    fn parses_battery_event() {
        let mut f = vec![0x04, 0xFF, 8, 0x00, 0x96, 0xC3, 0x14, 0x04, 0x10, 0x01, 0x00, 0x01, 0x57, 0];
        let sum = f[3..f.len() - 1].iter().fold(0u8, |s, b| s.wrapping_add(*b));
        *f.last_mut().unwrap() = sum;
        assert_eq!(parse_event(&f, 1), Some(vec![0x01, 0x57]));
    }
}
