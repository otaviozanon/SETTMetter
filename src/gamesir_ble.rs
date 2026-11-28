//! GameSir Cyclone Pro (T4cp) on its 2.4 GHz dongle (3537:1015).
//!
//! The dongle is a wired Xbox 360 pad to Windows (no real level), but the pad keeps advertising
//! over Bluetooth LE as "GameSir-Cyclone Pro" and answers GameSir's app "base info" request:
//!   service 0x8650, characteristic 0x865F: enable notifications, write 01 01
//!   the pad notifies a 19-byte packet whose byte 18 is the battery
//! 01 01 is a read-only "get base info" request; notifications are switched back off afterwards.
//! Service 0xFF10 on the same pad is the firmware-update channel: never touched.
//! Packet: [0..5] MAC [6..7] hw [8..9] BT ver [10..11] fw [12..17] build date [18] battery.
//! Battery is 0..100 percent, or 255 while charging; some firmwares prefix the packet with C9.
//!
//! The pad has to be paired with Windows once. Connecting can take seconds (or time out while the
//! pad is off), so the query runs on its own thread at most every two minutes and the scan only
//! reports the cached result. Protocol from Wireless-Device-Battery-Monitor (drivers/gamesir_ble.cpp
//! and gamesir_ble_protocol.h), worked out from GameSir's Android app.
//!
//! Discovery uses SetupAPI (BTHLE device nodes named "GameSir-Cyclone ..."), then WinRT for the
//! GATT transaction, so no AQS device query is needed. If another paired pad exposes the same
//! service, only the first one is queried: a second pad would share the dongle's row anyway.

use crate::device::{Kind, Observation, State};
use crate::{diag, wide};
use hidapi::HidApi;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows::core::GUID;
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattCharacteristicProperties, GattClientCharacteristicConfigurationDescriptorValue, GattCommunicationStatus,
    GattValueChangedEventArgs, GattWriteOption,
};
use windows::Devices::Bluetooth::BluetoothLEDevice;
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::{DataReader, DataWriter, IBuffer};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::Properties::DEVPROPKEY;

const VID: u16 = 0x3537;
const DONGLE_PID: u16 = 0x1015;
const NAME: &str = "GameSir Cyclone Pro";
const QUERY_EVERY: Duration = Duration::from_secs(120);
const RETRY: Duration = Duration::from_secs(60);
/// Stop reporting a reading older than this; afterwards the pad shows as asleep.
const STALE: Duration = Duration::from_secs(6 * 60);
/// The first read of a run is waited for; later ones complete in the background.
const FIRST_WAIT: Duration = Duration::from_secs(4);

const FRIENDLY_NAME_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: windows_sys::core::GUID::from_u128(0xA45C254E_DF1C_4EFD_8020_67D146A850E0),
    pid: 14,
};
const CONNECTED_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: windows_sys::core::GUID::from_u128(0x83DA6326_97A6_4088_9453_A1923F573B29),
    pid: 15,
};

fn service_uuid() -> GUID {
    GUID::from_u128(0x00008650_0000_1000_8000_00805F9B34FB)
}

fn characteristic_uuid() -> GUID {
    GUID::from_u128(0x0000865F_0000_1000_8000_00805F9B34FB)
}

#[derive(Clone, Copy)]
struct Reading {
    percent: Option<u8>,
    charging: bool,
}

#[derive(Default)]
struct Cached {
    address: Option<u64>,
    running: bool,
    last_attempt: Option<Instant>,
    last_ok: Option<Instant>,
    percent: Option<u8>,
    charging: bool,
    seen: bool,
}

pub struct GameSirBle {
    state: Arc<Mutex<Cached>>,
}

impl Default for GameSirBle {
    fn default() -> Self {
        Self { state: Arc::new(Mutex::new(Cached::default())) }
    }
}

impl GameSirBle {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let address = match pads().into_iter().find(|p| p.connected) {
            Some(pad) => pad.address,
            None => {
                // The pad is off or out of range: keep a row it had while it was answering.
                let c = self.state.lock().unwrap();
                let (Some(address), true) = (c.address, c.seen) else { return Vec::new() };
                return vec![Observation {
                    id: format!("gamesir-ble:{address:012x}"),
                    name: Some(NAME.into()),
                    kind: Some(Kind::Gamepad),
                    state: State::Unreachable,
                }];
            }
        };
        let id = format!("gamesir-ble:{address:012x}");

        let (first, spawned) = {
            let mut c = self.state.lock().unwrap();
            if c.address != Some(address) {
                *c = Cached { address: Some(address), ..Cached::default() };
            }
            let due = match (c.last_ok, c.last_attempt) {
                (Some(ok), Some(at)) if ok >= at => at.elapsed() >= QUERY_EVERY,
                _ => c.last_attempt.is_none_or(|at| at.elapsed() >= RETRY),
            };
            let first = c.last_attempt.is_none();
            let spawned = due && !c.running;
            if spawned {
                c.running = true;
                c.last_attempt = Some(Instant::now());
            }
            (first && spawned, spawned)
        };

        if spawned {
            let state = self.state.clone();
            let spawned = std::thread::Builder::new().spawn(move || {
                let reading = query(address);
                let mut c = state.lock().unwrap();
                // Another pad may have been paired while this query ran.
                if c.address == Some(address)
                    && let Some(reading) = reading
                {
                    // Charging reports no level: keep the last known percentage next to the bolt.
                    if reading.percent.is_some() || c.percent.is_none() {
                        c.percent = reading.percent;
                    }
                    c.charging = reading.charging;
                    c.last_ok = Some(Instant::now());
                    c.seen = true;
                }
                c.running = false;
            });
            if spawned.is_err() {
                self.state.lock().unwrap().running = false;
            }
        }

        if first {
            let deadline = Instant::now() + FIRST_WAIT;
            while Instant::now() < deadline && self.state.lock().unwrap().running {
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        let c = self.state.lock().unwrap();
        if !c.last_ok.is_some_and(|ok| ok.elapsed() < STALE) {
            return if c.seen {
                vec![Observation { id, name: Some(NAME.into()), kind: Some(Kind::Gamepad), state: State::Unreachable }]
            } else {
                Vec::new()
            };
        }
        let dongle = api.device_list().any(|d| d.vendor_id() == VID && d.product_id() == DONGLE_PID);
        let via = if dongle { "2.4 GHz receiver (battery over Bluetooth LE)" } else { "Bluetooth LE" };
        let state = match c.percent {
            Some(percent) => State::Battery { percent, charging: c.charging },
            None => State::Unreachable,
        };
        diag::event(&format!("gamesir-ble#{}", diag::bt_tag(address)), format!("{via}: {state:?}"));
        vec![Observation { id, name: Some(NAME.into()), kind: Some(Kind::Gamepad), state }]
    }
}

/// One base-info round trip. Blocking: run off the scan thread.
fn query(address: u64) -> Option<Reading> {
    crate::init_com();
    let dev = BluetoothLEDevice::FromBluetoothAddressAsync(address).ok()?.get().ok()?;

    let services = dev.GetGattServicesForUuidAsync(service_uuid()).ok()?.get().ok()?;
    if services.Status().ok()? != GattCommunicationStatus::Success {
        return None;
    }
    let list = services.Services().ok()?;
    if list.Size().ok()? == 0 {
        return None;
    }
    let service = list.GetAt(0).ok()?;

    let chars = service.GetCharacteristicsForUuidAsync(characteristic_uuid()).ok()?.get().ok()?;
    if chars.Status().ok()? != GattCommunicationStatus::Success {
        return None;
    }
    let list = chars.Characteristics().ok()?;
    if list.Size().ok()? == 0 {
        return None;
    }
    let characteristic = list.GetAt(0).ok()?;

    let waiter = Arc::new((Mutex::new(None::<Reading>), Condvar::new()));
    let handler = TypedEventHandler::<GattCharacteristic, GattValueChangedEventArgs>::new({
        let waiter = waiter.clone();
        move |_, args| {
            if let Some(args) = args
                && let Ok(buffer) = args.CharacteristicValue()
                && let Some(bytes) = buffer_bytes(&buffer)
                && let Some(reading) = parse(&bytes)
            {
                *waiter.0.lock().unwrap() = Some(reading);
                waiter.1.notify_all();
            }
            Ok(())
        }
    });
    let token = characteristic.ValueChanged(&handler).ok()?;

    let mut out = None;
    let enabled = characteristic
        .WriteClientCharacteristicConfigurationDescriptorAsync(GattClientCharacteristicConfigurationDescriptorValue::Notify)
        .ok()
        .and_then(|op| op.get().ok())
        == Some(GattCommunicationStatus::Success);
    if enabled {
        let value = buffer(&[0x01, 0x01]);
        let properties = characteristic.CharacteristicProperties().unwrap_or_default();
        let option = if properties.0 & GattCharacteristicProperties::WriteWithoutResponse.0 != 0 {
            GattWriteOption::WriteWithoutResponse
        } else {
            GattWriteOption::WriteWithResponse
        };
        if let Some(value) = value
            && let Ok(op) = characteristic.WriteValueWithOptionAsync(&value, option)
            && op.get().ok() == Some(GattCommunicationStatus::Success)
        {
            let (lock, cv) = &*waiter;
            let guard = lock.lock().unwrap();
            let (guard, _) = cv.wait_timeout_while(guard, Duration::from_secs(3), |v| v.is_none()).unwrap_or_else(|e| e.into_inner());
            out = *guard;
        }
        let _ = characteristic.WriteClientCharacteristicConfigurationDescriptorAsync(GattClientCharacteristicConfigurationDescriptorValue::None);
    }
    let _ = characteristic.RemoveValueChanged(token);
    let _ = dev.Close();
    out
}

fn buffer(bytes: &[u8]) -> Option<IBuffer> {
    let writer = DataWriter::new().ok()?;
    writer.WriteBytes(bytes).ok()?;
    writer.DetachBuffer().ok()
}

fn buffer_bytes(buffer: &IBuffer) -> Option<Vec<u8>> {
    let reader = DataReader::FromBuffer(buffer).ok()?;
    let len = reader.UnconsumedBufferLength().ok()? as usize;
    let mut out = vec![0u8; len.min(64)];
    reader.ReadBytes(&mut out).ok()?;
    Some(out)
}

/// Base-info packet, optionally prefixed with C9 on newer firmware.
fn parse(packet: &[u8]) -> Option<Reading> {
    let packet = match packet {
        [0xC9, rest @ ..] if rest.len() == 19 => rest,
        _ if packet.len() == 19 => packet,
        _ => return None,
    };
    match packet[18] {
        0xFF => Some(Reading { percent: None, charging: true }),
        v if v <= 100 => Some(Reading { percent: Some(v), charging: false }),
        _ => None,
    }
}

struct Pad {
    address: u64,
    connected: bool,
}

/// Paired Cyclone pads exposing a BTHLE device node, without touching WinRT.
fn pads() -> Vec<Pad> {
    let mut out = Vec::new();
    unsafe {
        let enumerator = wide("BTHLE");
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
            let mut needed = 0u32;
            if SetupDiGetDeviceInstanceIdW(set, &data, id.as_mut_ptr(), id.len() as u32, &mut needed) == 0 {
                continue;
            }
            let len = id.iter().position(|c| *c == 0).unwrap_or(id.len());
            let Some(address) = device_address(&String::from_utf16_lossy(&id[..len])) else { continue };
            let Some(name) = string_property(set, &data, &FRIENDLY_NAME_KEY) else { continue };
            if !name.starts_with("GameSir-Cyclone") {
                continue;
            }
            let connected = property(set, &data, &CONNECTED_KEY).and_then(|b| b.first().map(|v| *v != 0)) != Some(false);
            out.push(Pad { address, connected });
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

/// "BTHLE\DEV_F4EE25FBBA46\..." -> 0xF4EE25FBBA46
fn device_address(id: &str) -> Option<u64> {
    let mac = id.split('\\').nth(1)?.strip_prefix("DEV_")?;
    (mac.len() == 12).then(|| u64::from_str_radix(mac, 16).ok()).flatten()
}

unsafe fn property(set: HDEVINFO, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<Vec<u8>> {
    let mut buf = [0u8; 512];
    let (mut ptype, mut size) = (0u32, 0u32);
    let ok = SetupDiGetDevicePropertyW(set, data, key, &mut ptype, buf.as_mut_ptr(), buf.len() as u32, &mut size, 0);
    (ok != 0).then(|| buf[..(size as usize).min(buf.len())].to_vec())
}

unsafe fn string_property(set: HDEVINFO, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<String> {
    let bytes = property(set, data, key)?;
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|u| *u != 0).collect();
    Some(String::from_utf16_lossy(&units))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Vec<u8> {
        // MAC 11:22:33:44:55:66, hw 1, BT 2, fw 3, build date 2024-01-02, 96%.
        let mut p = vec![
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0xE8, 0x07, 0x01, 0x02, 0x00, 0x00,
            96,
        ];
        p[18] = 96;
        p
    }

    #[test]
    fn parses_level() {
        let r = parse(&base()).unwrap();
        assert_eq!(r.percent, Some(96));
        assert!(!r.charging);
    }

    #[test]
    fn charging_is_255_with_no_level() {
        let mut p = base();
        p[18] = 0xFF;
        let r = parse(&p).unwrap();
        assert!(r.charging);
        assert_eq!(r.percent, None);
    }

    #[test]
    fn accepts_the_c9_prefix_only_on_20_bytes() {
        let mut p = vec![0xC9];
        p.extend(base());
        assert_eq!(parse(&p).unwrap().percent, Some(96));
        // A 20-byte packet without the prefix follows another protocol.
        let mut p = base();
        p.push(0);
        assert!(parse(&p).is_none());
    }

    #[test]
    fn rejects_other_packets() {
        let mut p = base();
        p[18] = 101;
        assert!(parse(&p).is_none());
        assert!(parse(&base()[..18]).is_none());
    }

    #[test]
    fn instance_id_address() {
        assert_eq!(device_address(r"BTHLE\DEV_F4EE25FBBA46\7&16AA5920&0&F4EE25FBBA46"), Some(0xF4EE25FBBA46));
        assert_eq!(device_address(r"BTHLE\GENERIC_ATTRIBUTE\7&1"), None);
    }
}
