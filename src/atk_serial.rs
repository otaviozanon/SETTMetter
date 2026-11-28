//! ATK N9 Pro (373B:11CB dongle / 11CA cable) and first-generation N9 Ultra (1204 / 1203)
//! headsets, which are not queried over HID but over a USB virtual COM port (9600 8N1, 64-byte
//! frames):
//!   request [len = 2][cmd][sub][0...]        reply [len][cmd][sub][data...]
//!   cmd 00 sub 04 = status  -> data[1] == 1 when the headset is connected to its dongle
//!   cmd 00 sub 03 = battery -> data[1] bits 0-6 percent, bit 7 charging
//! The newer N9 V2 uses a different (HID) protocol. The port name comes from SetupAPI's PORTS
//! class and is only accepted when its instance id carries VID 373B and one of the four PIDs.
//! Opening with no share mode means the ATK HUB app, which holds the port while running, wins:
//! the headset is then skipped instead of having its port stolen.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/atk_serial.cpp), taken from ATK HUB
//! ("n9-pro" model, 9600 baud, 64-byte command frames).

use crate::device::{Kind, Observation, State};
use crate::{diag, wide};
use hidapi::HidApi;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
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

const VID: u16 = 0x373B;
/// ATK HUB uses CBR_9600 (see atk_serial.cpp); `CBR_9600` lives behind WindowsProgramming, so the
/// documented value is repeated here.
const BAUD: u32 = 9600;
/// DTR_CONTROL_ENABLE / RTS_CONTROL_ENABLE from winbase.h (also WindowsProgramming-only).
const DTR_CONTROL_ENABLE: u32 = 1;
const RTS_CONTROL_ENABLE: u32 = 1;

struct Model {
    pid: u16,
    name: &'static str,
    wired: bool,
}

const MODELS: &[Model] = &[
    Model { pid: 0x11CB, name: "ATK N9 Pro", wired: false },
    Model { pid: 0x11CA, name: "ATK N9 Pro", wired: true },
    Model { pid: 0x1204, name: "ATK N9 Ultra", wired: false },
    Model { pid: 0x1203, name: "ATK N9 Ultra", wired: true },
];

#[derive(Default)]
pub struct AtkSerial;

struct Port {
    name: String,
    model: &'static Model,
}

impl AtkSerial {
    pub fn scan(&mut self, _api: &HidApi) -> Vec<Observation> {
        let mut out: Vec<Observation> = Vec::new();
        for port in ports() {
            let model = port.model;
            let key = format!("atk-serial:{:04x}", model.pid);
            if out.iter().any(|o| o.id == key) {
                continue;
            }
            let via = if model.wired { "USB cable" } else { "2.4 GHz receiver" };
            let Some(serial) = Serial::open(&port.name) else {
                diag::event(&key, "port busy (ATK app open?) or not a working port");
                continue;
            };
            let Some(status) = serial.command(0x00, 0x04) else { continue };
            let connected = status.get(1) == Some(&1);
            let mut level = None;
            if connected && let Some(reply) = serial.command(0x00, 0x03) {
                level = battery(&reply);
            }
            let state = match (connected, level) {
                (false, _) => State::Off,
                (true, Some((percent, charging))) => State::Battery { percent, charging },
                // Connected but the battery query went unanswered: no "charging, level unknown" state.
                (true, None) => State::Unreachable,
            };
            let text = match state {
                State::Battery { percent, charging } => format!("{percent}%{}", if charging { " charging" } else { "" }),
                State::Off => "not connected to the dongle".to_string(),
                State::Unreachable => "connected, no level".to_string(),
            };
            diag::event(&key, format!("{via}: {text}"));
            out.push(Observation { id: key, name: Some(model.name.into()), kind: Some(Kind::Headset), state });
        }
        out
    }
}

/// One virtual COM port of the four known USB ids, with the port name from the registry.
fn ports() -> Vec<Port> {
    let mut out = Vec::new();
    unsafe {
        let set = SetupDiGetClassDevsW(&GUID_DEVCLASS_PORTS, null(), null_mut(), DIGCF_PRESENT);
        if set == -1 {
            return out;
        }
        let mut i = 0u32;
        loop {
            let mut data: SP_DEVINFO_DATA = zeroed();
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
            let Some(model) = MODELS.iter().find(|m| id.contains(&format!("VID_{VID:04X}&PID_{:04X}", m.pid))) else { continue };
            let key = SetupDiOpenDevRegKey(set, &data, DICS_FLAG_GLOBAL, 0, DIREG_DEV, KEY_READ);
            if key == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut name = [0u16; 32];
            let mut size = (size_of::<u16>() * (name.len() - 1)) as u32;
            let mut kind = REG_NONE;
            let value = wide("PortName");
            let ok = RegQueryValueExW(key, value.as_ptr(), null(), &mut kind, name.as_mut_ptr().cast::<u8>(), &mut size) == 0;
            RegCloseKey(key);
            if !ok || kind != REG_SZ {
                continue;
            }
            let end = name.iter().position(|c| *c == 0).unwrap_or(name.len());
            out.push(Port { name: String::from_utf16_lossy(&name[..end]), model });
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

struct Serial(HANDLE);

impl Serial {
    /// `\\.\COMx` at 9600 8N1, DTR/RTS enabled, no flow control, upstream's 50/300 ms timeouts.
    fn open(port: &str) -> Option<Self> {
        unsafe {
            let path = wide(&format!("\\\\.\\{port}"));
            // Share mode 0: an open ATK HUB makes this fail, which is what keeps the port safe.
            let handle = CreateFileW(path.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING, 0, null_mut());
            if handle == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut dcb: DCB = zeroed();
            dcb.DCBlength = size_of::<DCB>() as u32;
            if GetCommState(handle, &mut dcb) == 0 {
                CloseHandle(handle);
                return None;
            }
            dcb.BaudRate = BAUD;
            dcb.ByteSize = 8;
            dcb.Parity = NOPARITY;
            dcb.StopBits = ONESTOPBIT;
            dcb._bitfield = dcb_flags(dcb._bitfield);
            let timeouts = COMMTIMEOUTS {
                ReadIntervalTimeout: 50,
                ReadTotalTimeoutMultiplier: 0,
                ReadTotalTimeoutConstant: 300,
                WriteTotalTimeoutMultiplier: 0,
                WriteTotalTimeoutConstant: 300,
            };
            if SetCommState(handle, &dcb) == 0 || SetCommTimeouts(handle, &timeouts) == 0 {
                CloseHandle(handle);
                return None;
            }
            PurgeComm(handle, PURGE_RXCLEAR | PURGE_TXCLEAR);
            Some(Self(handle))
        }
    }

    /// Sends (cmd, sub) and returns the data bytes of the matching 64-byte reply frame.
    fn command(&self, cmd: u8, sub: u8) -> Option<Vec<u8>> {
        let frame = request(cmd, sub);
        let mut written = 0u32;
        if unsafe { WriteFile(self.0, frame.as_ptr(), frame.len() as u32, &mut written, null_mut()) } == 0 || written != frame.len() as u32 {
            return None;
        }
        let mut buf: Vec<u8> = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(800);
        while Instant::now() < deadline {
            let mut chunk = [0u8; 128];
            let mut read = 0u32;
            if unsafe { ReadFile(self.0, chunk.as_mut_ptr(), chunk.len() as u32, &mut read, null_mut()) } == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..read as usize]);
            // The headset also pushes unsolicited frames; skip those.
            while buf.len() >= 64 {
                if let Some(data) = reply(&buf, cmd, sub) {
                    return Some(data.to_vec());
                }
                buf.drain(..64);
            }
        }
        None
    }
}

impl Drop for Serial {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Request frame: the length byte counts itself, then the command and sub-command; the rest is
/// zero padding to the 64-byte frame the headset expects.
fn request(cmd: u8, sub: u8) -> [u8; 64] {
    let mut frame = [0u8; 64];
    frame[..3].copy_from_slice(&[2, cmd, sub]);
    frame
}

/// The 61 data bytes when `buf` starts with a reply to (cmd, sub).
fn reply(buf: &[u8], cmd: u8, sub: u8) -> Option<&[u8]> {
    (buf.len() >= 64 && buf[1] == cmd && buf[2] == sub).then(|| &buf[3..64])
}

/// Battery reply data: percent in bits 0-6 (clamped like the vendor app), charging in bit 7.
fn battery(data: &[u8]) -> Option<(u8, bool)> {
    let value = *data.get(1)?;
    Some(((value & 0x7F).min(100), value & 0x80 != 0))
}

/// DCB flags (winbase.h): fBinary:1, fParity:1, fOutxCtsFlow:1, fOutxDsrFlow:1, fDtrControl:2,
/// fDsrSensitivity:1, fTXContinueOnXoff:1, fOutX:1, fInX:1, fErrorChar:1, fNull:1,
/// fRtsControl:2, fAbortOnError:1.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(cmd: u8, sub: u8, data: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 64];
        f[0] = 2;
        f[1] = cmd;
        f[2] = sub;
        f[3..3 + data.len()].copy_from_slice(data);
        f
    }

    #[test]
    fn request_header_is_len_cmd_sub() {
        let f = request(0x00, 0x04);
        assert_eq!(f.len(), 64);
        assert_eq!(&f[..3], &[2, 0x00, 0x04]);
        assert!(f[3..].iter().all(|b| *b == 0));
    }

    #[test]
    fn status_and_battery_replies_are_matched() {
        let status = frame(0x00, 0x04, &[0x00, 0x01]);
        let data = reply(&status, 0x00, 0x04).unwrap();
        assert_eq!(data.len(), 61);
        assert_eq!(data[1], 1); // linked to the dongle

        // Unsolicited frame with another command must not be taken as the reply.
        assert!(reply(&frame(0x00, 0x05, &[0, 1]), 0x00, 0x04).is_none());
        assert!(reply(&status[..63], 0x00, 0x04).is_none());
    }

    #[test]
    fn battery_byte_percent_and_charging() {
        let data = frame(0x00, 0x03, &[0x00, 0x60]);
        assert_eq!(battery(reply(&data, 0x00, 0x03).unwrap()), Some((96, false)));

        let data = frame(0x00, 0x03, &[0x00, 0xE0]);
        assert_eq!(battery(reply(&data, 0x00, 0x03).unwrap()), Some((96, true)));

        // The vendor app clamps above 100 (0x7F unusable bits already masked).
        let data = frame(0x00, 0x03, &[0x00, 0x7F]);
        assert_eq!(battery(reply(&data, 0x00, 0x03).unwrap()), Some((100, false)));
    }

    #[test]
    fn dcb_sets_binary_dtr_rts_without_flow_control() {
        let bits = dcb_flags(0);
        assert_eq!(bits & 1, 1);
        assert_eq!((bits >> 4) & 0b11, DTR_CONTROL_ENABLE);
        assert_eq!((bits >> 12) & 0b11, RTS_CONTROL_ENABLE);
        assert_eq!(bits & (1 << 2), 0);
        assert_eq!(bits & (1 << 3), 0);
        assert_eq!(bits & (1 << 8), 0);
        assert_eq!(bits & (1 << 9), 0);
        // Existing parity/abort bits survive GetCommState.
        assert_eq!(dcb_flags(1 << 1) & (1 << 1), 1 << 1);
    }
}
