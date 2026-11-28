//! JBL Flip 7 Bluetooth speaker (Harman 0057, PID 2107), BLE control service.
//!
//! Read-only speaker-info request `AA 11 00`; no EQ, mode or firmware writes. The level arrives as
//! notifications on the control service:
//!
//! - service `{65786365-6C70-6F69-6E74-2E636F6D0000}`
//! - RX `…0001` (notify), TX `…0002` (write)
//!
//! Frame: `AA [cmd] [len]` (`[len]` is two bytes big-endian for cmds 92 and 96..99), then the
//! payload. Command `12` carries `[0]` then a subtype: `44` is battery `[level, bit 7 charging]`,
//! `42` the product id, `48` the speaker's current BLE address.
//!
//! Protocol: JBLControl docs/PROTOCOL-FLIP7.md, ported from jbl_speaker.cpp. Only the
//! already-paired Classic address is queried (as the public LE address): the original driver also
//! scanned advertisements for the speaker's rotating private address, which would need the
//! `Devices_Bluetooth_Advertisement` feature.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::wide;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattCharacteristicProperties, GattClientCharacteristicConfigurationDescriptorValue,
    GattCommunicationStatus, GattDeviceService, GattValueChangedEventArgs, GattWriteOption,
};
use windows::Devices::Bluetooth::{BluetoothAddressType, BluetoothCacheMode, BluetoothLEDevice};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::{DataReader, DataWriter};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::Properties::{DEVPROP_TYPE_BOOLEAN, DEVPROP_TYPE_STRING, DEVPROPKEY};
use windows_sys::core::GUID;

const SERVICE: windows::core::GUID = windows::core::GUID::from_u128(0x65786365_6C70_6F69_6E74_2E636F6D_0000);
const RX: windows::core::GUID = windows::core::GUID::from_u128(0x65786365_6C70_6F69_6E74_2E636F6D_0001);
const TX: windows::core::GUID = windows::core::GUID::from_u128(0x65786365_6C70_6F69_6E74_2E636F6D_0002);
const REQUEST: [u8; 3] = [0xAA, 0x11, 0x00];
const PRODUCT: u16 = 0x2107;
const QUERY_EVERY: Duration = Duration::from_secs(60);
const CACHE_FOR: Duration = Duration::from_secs(5 * 60);
const TIMEOUT: Duration = Duration::from_secs(4);

/// DEVPKEY_Device_IsConnected.
const CONNECTED_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x83DA6326_97A6_4088_9453_A1923F573B29),
    pid: 15,
};
/// DEVPKEY_Device_FriendlyName.
const NAME_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0xA45C254E_DF1C_4EFD_8020_67D146A850E0),
    pid: 14,
};

/// Fresh instances are made on every poll, so the reading cache lives in a module static.
#[derive(Default)]
pub struct JblSpeaker;

struct Reading {
    at: Instant,
    percent: u8,
    charging: bool,
}

fn cache() -> &'static Mutex<HashMap<u64, Reading>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Reading>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

impl JblSpeaker {
    pub fn scan(&mut self) -> Vec<Observation> {
        let mut out = Vec::new();
        for (name, addr) in speakers() {
            let mut cache = cache().lock().unwrap();
            let fresh = cache.get(&addr).is_some_and(|r| r.at.elapsed() < QUERY_EVERY);
            if !fresh {
                match query(addr) {
                    Some((percent, charging)) => {
                        cache.insert(addr, Reading { at: Instant::now(), percent, charging });
                    }
                    None => diag::event(&format!("jbl-speaker:{}", diag::bt_tag(addr)), "no answer on the BLE control service"),
                }
            }
            // A reading older than this says nothing about a speaker that may be off.
            let Some(reading) = cache.get(&addr).filter(|r| r.at.elapsed() < CACHE_FOR) else { continue };
            out.push(Observation {
                id: format!("jbl-speaker:{addr:012x}"),
                name: Some(name),
                kind: Some(Kind::Other),
                state: State::Battery { percent: reading.percent, charging: reading.charging },
            });
        }
        out
    }
}

/// Connected Classic device nodes named "JBL Flip 7".
fn speakers() -> Vec<(String, u64)> {
    let mut out = Vec::new();
    unsafe {
        let enumerator = wide("BTHENUM");
        let set = SetupDiGetClassDevsW(null(), enumerator.as_ptr(), null_mut(), DIGCF_ALLCLASSES | DIGCF_PRESENT);
        if set as isize == -1 {
            return out;
        }
        let mut i = 0;
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
            let id = String::from_utf16_lossy(&id[..len]);
            let Some(hex) = id.to_ascii_uppercase().strip_prefix("BTHENUM\\DEV_").and_then(|r| r.get(..12).map(str::to_string)) else {
                continue;
            };
            let Some(addr) = u64::from_str_radix(&hex, 16).ok().filter(|a| *a != 0) else { continue };
            if !bool_prop(set, &data, &CONNECTED_KEY).unwrap_or(false) {
                continue;
            }
            let name = string_prop(set, &data, &NAME_KEY).unwrap_or_default();
            if name.to_ascii_lowercase().contains("jbl flip 7") {
                out.push((name, addr));
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

fn bool_prop(set: HDEVINFO, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<bool> {
    let mut buf = [0u8; 8];
    let (mut ty, mut size) = (0u32, 0u32);
    let ok = unsafe {
        SetupDiGetDevicePropertyW(set, data, key, &mut ty, buf.as_mut_ptr(), buf.len() as u32, &mut size, 0)
    };
    (ok != 0 && ty == DEVPROP_TYPE_BOOLEAN).then(|| buf[0] != 0)
}

fn string_prop(set: HDEVINFO, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<String> {
    let mut buf = [0u8; 512];
    let (mut ty, mut size) = (0u32, 0u32);
    let ok = unsafe {
        SetupDiGetDevicePropertyW(set, data, key, &mut ty, buf.as_mut_ptr(), buf.len() as u32, &mut size, 0)
    };
    if ok == 0 || ty != DEVPROP_TYPE_STRING {
        return None;
    }
    let bytes = &buf[..(size as usize).min(buf.len())];
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|u| *u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// Reads the speaker-info notifications and returns the battery when the speaker identifies itself
/// as the Flip 7.
fn query(addr: u64) -> Option<(u8, bool)> {
    let device = BluetoothLEDevice::FromBluetoothAddressWithBluetoothAddressTypeAsync(addr, BluetoothAddressType::Public)
        .ok()?
        .get()
        .ok()?;
    let services = device
        .GetGattServicesForUuidWithCacheModeAsync(SERVICE, BluetoothCacheMode::Uncached)
        .ok()?
        .get()
        .ok()?;
    if services.Status().ok()? != GattCommunicationStatus::Success {
        return None;
    }
    let list = services.Services().ok()?;
    if list.Size().ok()? != 1 {
        return None;
    }
    let service = list.GetAt(0).ok()?;
    let rx = characteristic(&service, RX)?;
    let tx = characteristic(&service, TX)?;
    let rx_props = rx.CharacteristicProperties().ok()?;
    let tx_props = tx.CharacteristicProperties().ok()?;
    if !rx_props.contains(GattCharacteristicProperties::Notify) {
        return None;
    }
    if !tx_props.contains(GattCharacteristicProperties::Write) && !tx_props.contains(GattCharacteristicProperties::WriteWithoutResponse) {
        return None;
    }

    let info = Arc::new(Mutex::new(Info::default()));
    let sink = Arc::clone(&info);
    let handler = TypedEventHandler::new(move |_sender: &Option<GattCharacteristic>, args: &Option<GattValueChangedEventArgs>| {
        if let Some(args) = args {
            if let Ok(buffer) = args.CharacteristicValue() {
                if let (Ok(reader), Ok(len)) = (DataReader::FromBuffer(&buffer), buffer.Length()) {
                    let mut data = vec![0u8; len as usize];
                    if reader.ReadBytes(&mut data).is_ok() {
                        if let Ok(mut info) = sink.lock() {
                            info.feed(&data);
                        }
                    }
                }
            }
        }
        Ok(())
    });
    let token = rx.ValueChanged(&handler).ok()?;

    let status = rx
        .WriteClientCharacteristicConfigurationDescriptorAsync(GattClientCharacteristicConfigurationDescriptorValue::Notify)
        .ok()?
        .get()
        .ok()?;
    if status != GattCommunicationStatus::Success {
        let _ = rx.RemoveValueChanged(token);
        return None;
    }

    let writer = DataWriter::new().ok()?;
    writer.WriteBytes(&REQUEST).ok()?;
    let buffer = writer.DetachBuffer().ok()?;
    let option = if tx_props.contains(GattCharacteristicProperties::Write) {
        GattWriteOption::WriteWithResponse
    } else {
        GattWriteOption::WriteWithoutResponse
    };
    let status = tx.WriteValueWithOptionAsync(&buffer, option).ok()?.get().ok()?;
    let result = if status == GattCommunicationStatus::Success { await_battery(&info) } else { None };
    let _ = rx.RemoveValueChanged(token);
    result
}

fn characteristic(service: &GattDeviceService, uuid: windows::core::GUID) -> Option<GattCharacteristic> {
    let result = service.GetCharacteristicsForUuidWithCacheModeAsync(uuid, BluetoothCacheMode::Uncached).ok()?.get().ok()?;
    if result.Status().ok()? != GattCommunicationStatus::Success {
        return None;
    }
    let list = result.Characteristics().ok()?;
    if list.Size().ok()? != 1 {
        return None;
    }
    list.GetAt(0).ok()
}

fn await_battery(info: &Mutex<Info>) -> Option<(u8, bool)> {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(info) = info.lock() {
            if info.product == Some(PRODUCT) {
                if let Some(address) = info.address {
                    diag::event("jbl-speaker", format!("info from {}", diag::bt_tag(address)));
                }
                if let Some(percent) = info.percent {
                    return Some((percent, info.charging));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// JBL Portable AA protocol parser.
#[derive(Default)]
struct Info {
    percent: Option<u8>,
    charging: bool,
    product: Option<u16>,
    address: Option<u64>,
    pending: Vec<u8>,
}

impl Info {
    fn feed(&mut self, data: &[u8]) {
        if data.is_empty() || data.len() > 4096 {
            return;
        }
        if self.pending.len() + data.len() > 8192 {
            self.pending.clear();
        }
        self.pending.extend_from_slice(data);
        loop {
            let Some(start) = self.pending.iter().position(|b| *b == 0xAA) else {
                self.pending.clear();
                return;
            };
            self.pending.drain(..start);
            if self.pending.len() < 3 {
                return;
            }
            let cmd = self.pending[1];
            let long_len = cmd == 0x92 || (0x96..=0x99).contains(&cmd);
            let header = if long_len { 4 } else { 3 };
            if self.pending.len() < header {
                return;
            }
            let len = if long_len {
                usize::from(self.pending[2]) * 256 + usize::from(self.pending[3])
            } else {
                usize::from(self.pending[2])
            };
            if len > 4096 {
                self.pending.remove(0);
                continue;
            }
            if self.pending.len() < header + len {
                return;
            }
            let payload = &self.pending[header..header + len];
            if cmd == 0x12 && len >= 3 && payload[0] == 0 {
                if payload[1] == 0x44 && len == 3 && (payload[2] & 0x7F) <= 100 {
                    self.percent = Some(payload[2] & 0x7F);
                    self.charging = payload[2] & 0x80 != 0;
                } else if payload[1] == 0x42 && len == 4 {
                    self.product = Some(u16::from(payload[2]) << 8 | u16::from(payload[3]));
                } else if payload[1] == 0x48 && len == 8 {
                    self.address = Some(payload[2..8].iter().fold(0u64, |a, b| a << 8 | u64::from(*b)));
                }
            }
            self.pending.drain(..header + len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_speaker_info() {
        let mut info = Info::default();
        // Battery 80%, then the product id and the current BLE address.
        info.feed(&[0xAA, 0x12, 0x03, 0x00, 0x44, 0x50]);
        assert_eq!(info.percent, Some(80));
        assert!(!info.charging);
        info.feed(&[0xAA, 0x12, 0x04, 0x00, 0x42, 0x21, 0x07]);
        assert_eq!(info.product, Some(PRODUCT));
        info.feed(&[0xAA, 0x12, 0x08, 0x00, 0x48, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(info.address, Some(0xAABBCCDDEEFF));
    }

    #[test]
    fn charging_bit_and_split_reads() {
        let mut info = Info::default();
        info.feed(&[0xAA, 0x12, 0x03]);
        assert_eq!(info.percent, None);
        info.feed(&[0x00, 0x44, 0xD0]);
        assert_eq!(info.percent, Some(80));
        assert!(info.charging);
    }

    #[test]
    fn garbage_before_a_frame_is_dropped() {
        let mut info = Info::default();
        info.feed(&[0x01, 0x02, 0xAA, 0x12, 0x03, 0x00, 0x44, 0x32]);
        assert_eq!(info.percent, Some(50));
        assert!(info.pending.is_empty());
    }

    #[test]
    fn long_length_header() {
        let mut info = Info::default();
        // cmd 0x99 uses a two-byte length; 3 payload bytes, still no battery for us.
        info.feed(&[0xAA, 0x99, 0x00, 0x03, 0x01, 0x02, 0x03]);
        assert!(info.pending.is_empty());
        assert_eq!(info.percent, None);
    }
}
