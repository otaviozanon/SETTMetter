//! Bluetooth batteries, two ways:
//! - Bluetooth LE: the standard GATT Battery Service (0x180F / Battery Level 0x2A19). Most BLE
//!   mice and keyboards expose it.
//! - Classic Bluetooth (e.g. headsets over Hands-Free): Windows stores the level the device
//!   reports in a device property, the same one Settings > Bluetooth shows. Read with
//!   SetupAPI only, no WinRT.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::wide;
use std::collections::HashSet;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristicUuids, GattCommunicationStatus, GattDeviceService, GattServiceUuids,
};
use windows::Devices::Bluetooth::{BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice};
use windows::Foundation::Collections::IVectorView;
use windows::Storage::Streams::DataReader;
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::Properties::DEVPROPKEY;

/// DEVPKEY for the battery level Windows shows for Bluetooth (Hands-Free) devices.
const BATTERY_LEVEL_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x104EA319_6EE2_4701_BD47_8DDBF425BBE5),
    pid: 2,
};
/// DEVPKEY_Device_IsConnected (on BTHENUM\DEV_ and BTHLE\DEV_ nodes).
const CONNECTED_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x83DA6326_97A6_4088_9453_A1923F573B29),
    pid: 15,
};
/// DEVPKEY_Bluetooth_ClassOfDevice.
const CLASS_OF_DEVICE_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x2BD67D8B_8BEB_48D5_87E0_6CDA3428040A),
    pid: 4,
};
/// DEVPKEY_Device_FriendlyName: the name shown in Settings > Bluetooth.
const FRIENDLY_NAME_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0xA45C254E_DF1C_4EFD_8020_67D146A850E0),
    pid: 14,
};

pub fn scan() -> Vec<Observation> {
    let mut out = Vec::new();
    scan_ble(&mut out);
    scan_classic(&mut out);
    out
}

/// Paired LE devices are found through SetupAPI rather than a WinRT device query: the query
/// syntax (AQS) loads Windows' search and language stack, which costs more memory than the
/// rest of the app. Devices SetupAPI reports as disconnected are skipped without touching
/// WinRT, so its Bluetooth DLLs only load when an LE device is actually connected.
fn scan_ble(out: &mut Vec<Observation>) {
    let mut seen = HashSet::new();
    for node in nodes("BTHLE") {
        let Some(addr) = device_address(&node.id) else { continue };
        if node.connected == Some(false) || !seen.insert(addr) {
            continue;
        }
        let key = format!("ble#{}", diag::bt_tag(addr));
        let dev = match BluetoothLEDevice::FromBluetoothAddressAsync(addr).and_then(|op| op.get()) {
            Ok(dev) => dev,
            Err(e) => {
                diag::event(&key, format!("open failed: {e}"));
                continue;
            }
        };
        if dev.ConnectionStatus().is_ok_and(|s| s == BluetoothConnectionStatus::Connected) {
            match read_ble(&dev) {
                Ok(Ok(o)) => out.push(o),
                Ok(Err(reason)) => diag::event(&key, reason),
                Err(e) => diag::event(&key, format!("error: {e}")),
            }
        }
        let _ = dev.Close();
    }
}

/// Device nodes (not profile/service nodes):
/// "BTHLE\DEV_F4EE25FBBA46\..." or "BTHENUM\DEV_F5CFD77BCB0B\..." -> the address
fn device_address(id: &str) -> Option<u64> {
    let mac = id.split('\\').nth(1)?.strip_prefix("DEV_")?;
    (mac.len() == 12).then(|| u64::from_str_radix(mac, 16).ok()).flatten()
}

/// The device, or why it isn't shown (for diagnostics).
fn read_ble(dev: &BluetoothLEDevice) -> windows::core::Result<Result<Observation, String>> {
    // Appearance: skip phones (0x0040..) and computers (0x0080..); classify HID and audio.
    let appearance = dev.Appearance()?.RawValue()?;
    let skip = |why: &str| Ok(Err(format!("appearance 0x{appearance:04X}: {why}")));
    let kind = match appearance >> 6 {
        0x01 | 0x02 => return skip("phone or computer, skipped"),
        0x0F => match appearance & 0x3F {
            1 => Some(Kind::Keyboard),
            2 => Some(Kind::Mouse),
            3 | 4 => Some(Kind::Gamepad),
            _ => None,
        },
        // Wearable audio device: 0x0941 earbud, 0x0942 headset, 0x0943 headphones.
        0x25 => match appearance & 0x3F {
            1 => Some(Kind::Earbuds),
            2 | 3 => Some(Kind::Headset),
            _ => None, // generic: decide by name
        },
        _ => None,
    };

    let result = dev.GetGattServicesForUuidWithCacheModeAsync(GattServiceUuids::Battery()?, BluetoothCacheMode::Cached)?.get()?;
    let status = result.Status()?;
    if status != GattCommunicationStatus::Success {
        return skip(&format!("Battery Service lookup failed ({})", gatt_status(status)));
    }
    let services = result.Services()?;
    let level = read_level(&services);
    for i in 0..services.Size()? {
        let _ = services.GetAt(i)?.Close();
    }
    let percent = match level? {
        Ok(percent) => percent,
        Err(why) => return skip(&why),
    };

    let name = dev.Name()?.to_string();
    if name.to_ascii_lowercase().contains("xbox") {
        return skip("Xbox controller, reported by the Xbox source"); // more precise there
    }
    // Not the name: users rename Bluetooth devices.
    diag::event(&format!("ble#{}", diag::bt_tag(dev.BluetoothAddress()?)), format!("appearance 0x{appearance:04X}: battery via GATT"));
    Ok(Ok(Observation {
        id: format!("ble:{:012x}", dev.BluetoothAddress()?),
        kind: kind.or_else(|| Some(Kind::from_name(&name))),
        name: Some(name),
        state: State::Battery { percent: percent.min(100), charging: false },
    }))
}

fn read_level(services: &IVectorView<GattDeviceService>) -> windows::core::Result<Result<u8, String>> {
    if services.Size()? == 0 {
        return Ok(Err("no Battery Service (0x180F)".into()));
    }
    let chars = services
        .GetAt(0)?
        .GetCharacteristicsForUuidWithCacheModeAsync(GattCharacteristicUuids::BatteryLevel()?, BluetoothCacheMode::Cached)?
        .get()?;
    if chars.Status()? != GattCommunicationStatus::Success {
        return Ok(Err(format!("Battery Level lookup failed ({})", gatt_status(chars.Status()?))));
    }
    if chars.Characteristics()?.Size()? == 0 {
        return Ok(Err("no Battery Level characteristic (0x2A19)".into()));
    }
    let value = chars.Characteristics()?.GetAt(0)?.ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached)?.get()?;
    if value.Status()? != GattCommunicationStatus::Success {
        return Ok(Err(format!("reading Battery Level failed ({})", gatt_status(value.Status()?))));
    }
    Ok(Ok(DataReader::FromBuffer(&value.Value()?)?.ReadByte()?))
}

fn gatt_status(s: GattCommunicationStatus) -> &'static str {
    match s.0 {
        0 => "success",
        1 => "unreachable",
        2 => "protocol error",
        3 => "access denied",
        _ => "unknown status",
    }
}

/// Entirely SetupAPI: the battery level sits on the device's profile nodes
/// ("BTHENUM\{uuid}_..."), its name, class and connection state on the device node
/// ("BTHENUM\DEV_<address>").
fn scan_classic(out: &mut Vec<Observation>) {
    let nodes = nodes("BTHENUM");
    let mut seen = HashSet::new();
    for node in &nodes {
        let (Some(percent), Some(addr)) = (node.level, address_from_instance_id(&node.id)) else { continue };
        if !seen.insert(addr) {
            continue; // one device has several profile nodes
        }
        let Some(dev) = nodes.iter().find(|d| device_address(&d.id) == Some(addr)) else { continue };
        let connected = dev.connected == Some(true);
        let name = dev.name.clone().unwrap_or_default();
        let cod = dev.class.unwrap_or(0);
        let kind = match ((cod >> 8) & 0x1F, (cod >> 2) & 0x3F) {
            // Audio/video. The minor class rarely separates earbuds from headphones
            // (AirPods report "headphones"), so the name decides.
            (0x04, _) => match Kind::from_name(&name) {
                Kind::Earbuds => Kind::Earbuds,
                _ => Kind::Headset,
            },
            (0x05, minor) if minor & 0x20 != 0 => Kind::Mouse, // peripheral: pointing
            (0x05, minor) if minor & 0x10 != 0 => Kind::Keyboard,
            (0x05, _) => Kind::Gamepad,
            _ => Kind::from_name(&name),
        };
        diag::event(
            &format!("bt#{}", diag::bt_tag(addr)),
            format!("class 0x{cod:06X} ({kind:?}): battery via Windows, {}", if connected { "connected" } else { "not connected" }),
        );
        out.push(Observation {
            id: format!("bt:{addr:012x}"),
            // Paired-but-disconnected devices keep a stale level; only add them once seen connected.
            name: (connected && !name.is_empty()).then_some(name),
            kind: Some(kind),
            state: if connected { State::Battery { percent: percent.min(100), charging: false } } else { State::Off },
        });
    }
    // The usual "my headphones don't show up" case: Windows never got a level from them.
    for dev in &nodes {
        let Some(addr) = device_address(&dev.id) else { continue };
        if dev.connected == Some(true) && !seen.contains(&addr) {
            diag::event(&format!("bt#{}", diag::bt_tag(addr)), format!("class 0x{:06X}: connected, no battery level in Windows", dev.class.unwrap_or(0)));
        }
    }
}

/// A present device node and the Bluetooth properties it carries.
struct Node {
    id: String,
    level: Option<u8>,
    connected: Option<bool>,
    name: Option<String>,
    class: Option<u32>,
}

/// Every present device node of `enumerator`.
fn nodes(enumerator: &str) -> Vec<Node> {
    let mut out = Vec::new();
    unsafe {
        let enumerator = wide(enumerator);
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
            let prop = |key: &DEVPROPKEY| property(set, &data, key);
            let level = prop(&BATTERY_LEVEL_KEY).and_then(|b| b.first().copied());
            // DEVPROP_TYPE_BOOLEAN: 0x00 or 0xFF.
            let connected = prop(&CONNECTED_KEY).and_then(|b| b.first().map(|v| *v != 0));
            let class = prop(&CLASS_OF_DEVICE_KEY).and_then(|b| Some(u32::from_le_bytes(b.get(..4)?.try_into().ok()?)));
            let name = prop(&FRIENDLY_NAME_KEY).map(|b| {
                let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|u| *u != 0).collect();
                String::from_utf16_lossy(&units)
            });
            let len = id.iter().position(|c| *c == 0).unwrap_or(id.len());
            out.push(Node { id: String::from_utf16_lossy(&id[..len]), level, connected, name, class });
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

/// Raw bytes of a device property, if the node has it.
unsafe fn property(set: HDEVINFO, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<Vec<u8>> {
    let mut buf = [0u8; 512];
    let (mut ptype, mut size) = (0u32, 0u32);
    let ok = SetupDiGetDevicePropertyW(set, data, key, &mut ptype, buf.as_mut_ptr(), buf.len() as u32, &mut size, 0);
    (ok != 0).then(|| buf[..(size as usize).min(buf.len())].to_vec())
}

/// "BTHENUM\{...}_VID&..._PID&...\7&3057B904&0&F5CFD77BCB0B_C00000000" -> 0xF5CFD77BCB0B
fn address_from_instance_id(id: &str) -> Option<u64> {
    let last = id.rsplit('\\').next()?;
    let mac = last.rsplit('&').next()?.split('_').next()?;
    (mac.len() == 12).then(|| u64::from_str_radix(mac, 16).ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_addresses() {
        assert_eq!(device_address(r"BTHLE\DEV_F4EE25FBBA46\7&16AA5920&0&F4EE25FBBA46"), Some(0xF4EE25FBBA46));
        assert_eq!(
            device_address(r"BTHENUM\DEV_F5CFD77BCB0B\7&3057B904&0&BLUETOOTHDEVICE_F5CFD77BCB0B"),
            Some(0xF5CFD77BCB0B)
        );
        assert_eq!(device_address(r"BTHLE\GENERIC_ATTRIBUTE\7&1"), None);
        // Profile nodes carry the battery level but aren't device nodes.
        assert_eq!(device_address(r"BTHENUM\{0000111E-0000-1000-8000-00805F9B34FB}_VID&0001004C_PID&2014\7&3057B904&0&F5CFD77BCB0B_C00000000"), None);
        assert_eq!(
            address_from_instance_id(r"BTHENUM\{0000111E-0000-1000-8000-00805F9B34FB}_VID&0001004C_PID&2014\7&3057B904&0&F5CFD77BCB0B_C00000000"),
            Some(0xF5CFD77BCB0B)
        );
    }
}
