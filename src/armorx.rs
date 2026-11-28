//! BigBigWon Armor X Pro: the real level is only on the controller's Bluetooth LE link, never on
//! the 2.4 GHz dongle (which poses as a wired Xbox pad 045E:0B12 and always says "Full").
//!
//! The link carries the standard BLE Battery Service (0x180F / Battery Level 0x2A19). The
//! controller sends no charging flag, so charging is guessed from the trend of the readings
//! (`armorx_trend.h`): median-smoothed over the last five readings, and only "charging" after a
//! sustained rise, never at 99% or above. The LED stays the ground truth.
//!
//! Limitation: the upstream driver finds the controller through a BLE advertisement watcher
//! ("ARMOR-X..." in the scan response), which needs the `Devices_Bluetooth_Advertisement`
//! feature. Without it this module can only read a controller Windows already knows and reports
//! as connected (paired, i.e. what `src/bluetooth.rs` lists), plus the dongle gate: no dongle,
//! no Bluetooth work. The controller's BT side is weak, so reads fail often; a reading stays on
//! screen for 10 minutes, and a new one is tried at most once a minute.

use crate::device::{Kind, Observation, State};
use crate::diag;
use hidapi::HidApi;
use std::collections::VecDeque;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristicUuids, GattCommunicationStatus, GattServiceUuids,
};
use windows::Devices::Bluetooth::{BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice};
use windows::Storage::Streams::DataReader;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::Properties::DEVPROPKEY;

const XBOX_VID: u16 = 0x045E;
const XBOX_PID: u16 = 0x0B12;
const KEY: &str = "armorx";
const NAME: &str = "Armor X Pro";
const READ_EVERY: Duration = Duration::from_secs(60);
const STALE: Duration = Duration::from_secs(10 * 60);

/// DEVPKEY_Device_IsConnected.
const CONNECTED_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: windows_sys::core::GUID::from_u128(0x83DA6326_97A6_4088_9453_A1923F573B29),
    pid: 15,
};
/// DEVPKEY_Device_FriendlyName.
const FRIENDLY_NAME_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: windows_sys::core::GUID::from_u128(0xA45C254E_DF1C_4EFD_8020_67D146A850E0),
    pid: 14,
};

pub struct Armorx {
    trend: Trend,
    address: Option<u64>,
    percent: u8,
    charging: bool,
    last_read: Option<Instant>,
    last_ok: Option<Instant>,
}

impl Default for Armorx {
    fn default() -> Self {
        Self { trend: Trend::default(), address: None, percent: 0, charging: false, last_read: None, last_ok: None }
    }
}

impl Armorx {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        if !api.device_list().any(|d| d.vendor_id() == XBOX_VID && d.product_id() == XBOX_PID) {
            return Vec::new(); // no dongle: no Bluetooth scan for everyone else
        }
        if self.address.is_none() {
            self.address = find_controller();
        }
        let now = Instant::now();
        if let Some(address) = self.address
            && self.last_read.is_none_or(|t| now.duration_since(t) >= READ_EVERY)
        {
            self.last_read = Some(now);
            match read_level(address) {
                Some(raw) => {
                    let (percent, charging) = self.trend.add(raw);
                    self.percent = percent;
                    self.charging = charging;
                    self.last_ok = Some(now);
                    diag::event(KEY, format!("BLE 0x2A19: {raw}% -> {percent}%{}", if charging { " (rising)" } else { "" }));
                }
                None => diag::event(KEY, "no BLE battery reply"),
            }
        }
        match self.last_ok {
            Some(t) if now.duration_since(t) < STALE => vec![Observation {
                id: KEY.into(),
                name: Some(NAME.into()),
                kind: Some(Kind::Gamepad),
                state: State::Battery { percent: self.percent, charging: self.charging },
            }],
            Some(_) => vec![Observation {
                id: KEY.into(),
                name: Some(NAME.into()),
                kind: Some(Kind::Gamepad),
                state: State::Unreachable,
            }],
            None => Vec::new(),
        }
    }
}

/// Charging guess from a median-smoothed trend, as in the upstream `ArmorX::Trend`.
#[derive(Default)]
struct Trend {
    raw: VecDeque<i32>,
    smooth: VecDeque<i32>,
}

impl Trend {
    const SMOOTH_WINDOW: usize = 5;
    const TREND_WINDOW: usize = 4;
    const RISE_TO_CHARGE: i32 = 2;
    const FULL: i32 = 99;

    /// Adds one raw reading; returns the smoothed level and whether it is rising.
    fn add(&mut self, percent: u8) -> (u8, bool) {
        self.raw.push_back(i32::from(percent.min(100)));
        if self.raw.len() > Self::SMOOTH_WINDOW {
            self.raw.pop_front();
        }
        let mut v: Vec<i32> = self.raw.iter().copied().collect();
        v.sort_unstable();
        let n = v.len();
        let level = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2] + 1) / 2 };
        self.smooth.push_back(level);
        if self.smooth.len() > Self::TREND_WINDOW {
            self.smooth.pop_front();
        }
        let charging = level < Self::FULL
            && self.smooth.len() == Self::TREND_WINDOW
            && self.smooth.back().unwrap() - self.smooth.front().unwrap() >= Self::RISE_TO_CHARGE;
        (level as u8, charging)
    }
}

/// A BTHLE device node Windows already knows.
struct Node {
    id: String,
    name: Option<String>,
    connected: Option<bool>,
}

fn find_controller() -> Option<u64> {
    nodes("BTHLE")
        .into_iter()
        .find(|n| n.connected != Some(false) && n.name.as_deref().is_some_and(|name| name.to_ascii_lowercase().contains("armor")))
        .and_then(|n| address(&n.id))
}

/// Reads Battery Level (0x2A19) of an LE device connected to Windows.
fn read_level(addr: u64) -> Option<u8> {
    let dev = BluetoothLEDevice::FromBluetoothAddressAsync(addr).ok()?.get().ok()?;
    if !dev.ConnectionStatus().is_ok_and(|s| s == BluetoothConnectionStatus::Connected) {
        let _ = dev.Close();
        return None;
    }
    let level = read_service(&dev);
    let _ = dev.Close();
    level
}

fn read_service(dev: &BluetoothLEDevice) -> Option<u8> {
    let result = dev
        .GetGattServicesForUuidWithCacheModeAsync(GattServiceUuids::Battery().ok()?, BluetoothCacheMode::Cached)
        .ok()?
        .get()
        .ok()?;
    if result.Status().ok()? != GattCommunicationStatus::Success {
        return None;
    }
    let services = result.Services().ok()?;
    if services.Size().ok()? == 0 {
        return None;
    }
    let chars = services
        .GetAt(0)
        .ok()?
        .GetCharacteristicsForUuidWithCacheModeAsync(GattCharacteristicUuids::BatteryLevel().ok()?, BluetoothCacheMode::Cached)
        .ok()?
        .get()
        .ok()?;
    if chars.Status().ok()? != GattCommunicationStatus::Success {
        return None;
    }
    let characteristics = chars.Characteristics().ok()?;
    if characteristics.Size().ok()? == 0 {
        return None;
    }
    let value = characteristics.GetAt(0).ok()?.ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached).ok()?.get().ok()?;
    if value.Status().ok()? != GattCommunicationStatus::Success {
        return None;
    }
    Some(DataReader::FromBuffer(&value.Value().ok()?).ok()?.ReadByte().ok()?)
}

/// Present devices of an enumerator ("BTHLE"), with the name and connection flag Windows has.
fn nodes(enumerator: &str) -> Vec<Node> {
    let mut out = Vec::new();
    unsafe {
        let enumerator = crate::wide(enumerator);
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
            let name = property(set, &data, &FRIENDLY_NAME_KEY).map(|b| utf16(&b));
            let connected = property(set, &data, &CONNECTED_KEY).and_then(|b| b.first().map(|v| *v != 0));
            let len = id.iter().position(|c| *c == 0).unwrap_or(id.len());
            out.push(Node { id: String::from_utf16_lossy(&id[..len]), name, connected });
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

/// "BTHLE\DEV_F4EE25FBBA46\..." -> 0xF4EE25FBBA46.
fn address(id: &str) -> Option<u64> {
    let mac = id.split('\\').nth(1)?.strip_prefix("DEV_")?;
    (mac.len() == 12).then(|| u64::from_str_radix(mac, 16).ok()).flatten()
}

fn utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|u| *u != 0).collect();
    String::from_utf16_lossy(&units)
}

/// Raw bytes of a device property, if the node has it.
unsafe fn property(set: HDEVINFO, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<Vec<u8>> {
    let mut buf = [0u8; 512];
    let (mut ptype, mut size) = (0u32, 0u32);
    let ok = SetupDiGetDevicePropertyW(set, data, key, &mut ptype, buf.as_mut_ptr(), buf.len() as u32, &mut size, 0);
    (ok != 0).then(|| buf[..(size as usize).min(buf.len())].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trend_needs_a_sustained_rise() {
        let mut t = Trend::default();
        assert_eq!(t.add(50), (50, false));
        assert_eq!(t.add(50), (50, false));
        assert_eq!(t.add(50), (50, false));
        assert_eq!(t.add(50), (50, false));
        assert_eq!(t.add(52), (50, false));
        assert_eq!(t.add(53), (50, false));
        // [50 50 52 53 54] -> median 52; smooth [50 50 50 52]: +2 over the window.
        assert_eq!(t.add(54), (52, true));
    }

    #[test]
    fn full_or_single_bounce_is_not_charging() {
        let mut t = Trend::default();
        assert_eq!(t.add(99), (99, false));
        assert_eq!(t.add(99), (99, false));
        assert_eq!(t.add(99), (99, false));
        assert_eq!(t.add(99), (99, false));
        assert_eq!(t.add(100), (99, false));

        let mut b = Trend::default();
        for p in [60, 60, 60, 60, 60, 61, 60, 60] {
            assert!(!b.add(p).1);
        }
    }

    #[test]
    fn parses_the_ble_instance_id() {
        assert_eq!(address(r"BTHLE\DEV_F4EE25FBBA46\7&16AA5920&0&F4EE25FBBA46"), Some(0xF4EE25FBBA46));
        assert_eq!(address(r"BTHLE\GENERIC_ATTRIBUTE\7&1"), None);
    }
}
