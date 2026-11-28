//! Logitech G435 on its Jieli LIGHTSPEED receiver (4C4A:4155).
//!
//! That receiver exposes no readable battery level at all. Upstream estimates it: the last
//! Bluetooth Hands-Free level Windows stored for the paired G435 is the reference, and while the
//! headset is in use over the receiver the estimate drops by a discharge rate learned from
//! successive Bluetooth readings (`g435_estimate.h`).
//!
//! Ported here: the estimate state machine and the read-only Bluetooth reference query
//! (`DEVPKEY_Device_BatteryLevel` / `DEVPKEY_Device_IsConnected` on the G435's BTHENUM nodes).
//! Skipped: the CoreAudio "is the receiver's USB endpoint playing" test, which decides when the
//! estimate may fall, and the HKCU persistence of the learned rate. Until that lands no
//! observation is emitted, so no second row is added next to the Bluetooth one that
//! `src/bluetooth.rs` already reports. Nothing is written to the receiver or the headset.
//!
//! From Wireless-Device-Battery-Monitor (drivers/logitech_g435_jieli.cpp, g435_estimate.h).

use crate::device::Observation;
use crate::diag;
use crate::wide;
use hidapi::HidApi;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::Properties::DEVPROPKEY;

const VID: u16 = 0x4C4A;
const PID: u16 = 0x4155;
const KEY: &str = "g435:jieli";

/// A gap longer than this is not counted as use.
const MAX_STEP_SEC: u64 = 300;
/// Use needed between two Bluetooth readings before a drain rate is learned.
const MIN_LEARN_SEC: u64 = 30 * 60;
/// 1 .. 50 % per hour, in thousandths.
const MIN_RATE_MILLI: i32 = 1000;
const MAX_RATE_MILLI: i32 = 50000;

/// DEVPKEY_Device_BatteryLevel, DEVPROP_TYPE_BYTE 0..100 (Hands-Free report).
const BATTERY_LEVEL_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: windows_sys::core::GUID::from_u128(0x104EA319_6EE2_4701_BD47_8DDBF425BBE5),
    pid: 2,
};
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

/// Learned estimate state; values as in the upstream `g435::State`.
struct Estimate {
    /// Last Bluetooth reading, 0..100 (-1: never calibrated).
    last_real: i32,
    /// Use over the receiver since that reading.
    usage_sec: u64,
    last_tick: u64,
    /// Learned drain, thousandths of a percent per hour (0: not yet).
    rate_milli: i32,
}

impl Default for Estimate {
    fn default() -> Self {
        Self { last_real: -1, usage_sec: 0, last_tick: 0, rate_milli: 0 }
    }
}

impl Estimate {
    /// A Bluetooth reading; true when the state changed.
    fn record(&mut self, percent: i32) -> bool {
        if !(0..=100).contains(&percent) || percent == self.last_real {
            return false;
        }
        if self.last_real >= 0 && percent < self.last_real && self.usage_sec >= MIN_LEARN_SEC {
            let rate = ((i64::from(self.last_real - percent) * 1000 * 3600) / self.usage_sec as i64) as i32;
            let rate = rate.clamp(MIN_RATE_MILLI, MAX_RATE_MILLI);
            self.rate_milli = if self.rate_milli == 0 { rate } else { (self.rate_milli * 7 + rate * 3) / 10 };
        }
        self.last_real = percent;
        self.usage_sec = 0;
        true
    }

    /// Counts the time since the last call as use only while `in_use`.
    fn accumulate(&mut self, in_use: bool, now: u64) -> bool {
        let mut changed = false;
        if in_use && self.last_tick != 0 && now > self.last_tick {
            self.usage_sec += (now - self.last_tick).min(MAX_STEP_SEC);
            changed = true;
        }
        if self.last_tick != now {
            self.last_tick = now;
            changed = true;
        }
        changed
    }

    /// -1 before the first Bluetooth reading, else 1..100.
    fn estimate(&self) -> i32 {
        if self.last_real < 0 {
            return -1;
        }
        if self.rate_milli == 0 {
            return self.last_real;
        }
        let drop = (self.usage_sec as f64 / 3600.0 * f64::from(self.rate_milli) / 1000.0) as i32;
        (self.last_real - drop).clamp(1, 100)
    }
}

#[derive(Default)]
pub struct G435Jieli {
    estimate: Estimate,
}

impl G435Jieli {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        if !api.device_list().any(|d| d.vendor_id() == VID && d.product_id() == PID) {
            return Vec::new();
        }
        match reference() {
            Some((connected, percent)) => {
                self.estimate.record(i32::from(percent));
                self.estimate.accumulate(false, seconds());
                let estimate = self.estimate.estimate();
                diag::event(
                    KEY,
                    format!(
                        "Bluetooth reference {percent}% ({}); estimate ~{estimate}% not shown: the CoreAudio in-use signal is not ported",
                        if connected { "connected" } else { "paired" }
                    ),
                );
            }
            // Without the CoreAudio in-use signal the estimate cannot fall; see the module note.
            None => diag::event(KEY, "no Bluetooth reference for the G435; CoreAudio in-use signal not ported"),
        }
        Vec::new()
    }
}

/// `(connected, percent)` of the G435 as Windows has it, from its BTHENUM nodes. The Hands-Free
/// Audio Gateway node carries the headset's own report and wins over the other profile nodes.
pub fn reference() -> Option<(bool, u8)> {
    let nodes = nodes();
    let device = nodes
        .iter()
        .find(|n| device_address(&n.id).is_some() && n.name.as_deref().is_some_and(is_g435))?;
    let address = device_address(&device.id)?;
    let level = nodes
        .iter()
        .filter(|n| device_address(&n.id) == Some(address) || address_from_instance_id(&n.id) == Some(address))
        .filter_map(|n| n.level.map(|p| (is_hands_free(&n.id), p)))
        .max_by_key(|(hands_free, _)| *hands_free)
        .map(|(_, p)| p)?;
    Some((device.connected == Some(true), level))
}

/// "g435" as a whole word, so no other device is taken for the headset.
fn is_g435(name: &str) -> bool {
    let low = name.to_ascii_lowercase();
    low.match_indices("g435").any(|(pos, _)| {
        let left = pos == 0 || !low.as_bytes()[pos - 1].is_ascii_alphanumeric();
        let right = low.as_bytes().get(pos + 4).is_none_or(|b| !b.is_ascii_alphanumeric());
        left && right
    })
}

fn is_hands_free(id: &str) -> bool {
    id.to_ascii_lowercase().contains("{0000111e-0000-1000-8000-00805f9b34fb}")
}

struct Node {
    id: String,
    level: Option<u8>,
    connected: Option<bool>,
    name: Option<String>,
}

/// Every present BTHENUM device node, with the battery level and name Windows holds for it.
fn nodes() -> Vec<Node> {
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
            let mut needed = 0u32;
            if SetupDiGetDeviceInstanceIdW(set, &data, id.as_mut_ptr(), id.len() as u32, &mut needed) == 0 {
                continue;
            }
            let level = property(set, &data, &BATTERY_LEVEL_KEY).and_then(|b| b.first().copied()).filter(|p| *p <= 100);
            let connected = property(set, &data, &CONNECTED_KEY).and_then(|b| b.first().map(|v| *v != 0));
            let name = property(set, &data, &FRIENDLY_NAME_KEY).map(|b| utf16(&b));
            let len = id.iter().position(|c| *c == 0).unwrap_or(id.len());
            out.push(Node { id: String::from_utf16_lossy(&id[..len]), level, connected, name });
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

fn utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|u| *u != 0).collect();
    String::from_utf16_lossy(&units)
}

/// "BTHENUM\DEV_F5CFD77BCB0B\..." -> 0xF5CFD77BCB0B.
fn device_address(id: &str) -> Option<u64> {
    let mac = id.split('\\').nth(1)?.strip_prefix("DEV_")?;
    (mac.len() == 12).then(|| u64::from_str_radix(mac, 16).ok()).flatten()
}

/// "BTHENUM\{...}_VID&..._PID&...\7&3057B904&0&F5CFD77BCB0B_C00000000" -> 0xF5CFD77BCB0B.
fn address_from_instance_id(id: &str) -> Option<u64> {
    let last = id.rsplit('\\').next()?;
    let mac = last.rsplit('&').next()?.split('_').next()?;
    (mac.len() == 12).then(|| u64::from_str_radix(mac, 16).ok()).flatten()
}

fn seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncalibrated_and_calibrated_estimates() {
        let mut e = Estimate::default();
        assert_eq!(e.estimate(), -1);
        assert!(e.record(80));
        assert_eq!(e.estimate(), 80); // one reading: the reference itself
        assert!(!e.record(80)); // unchanged reading
        assert!(!e.record(101));
    }

    #[test]
    fn learns_a_rate_and_drops_with_use() {
        let mut e = Estimate::default();
        e.record(80);
        // Two hours of use over the receiver, counted in poll-sized steps.
        let mut now = 1_000_000;
        e.accumulate(true, now);
        for _ in 0..24 {
            now += 300;
            e.accumulate(true, now);
        }
        assert_eq!(e.usage_sec, 2 * 3600);
        // Coming back on Bluetooth at 75%: 5% over 2 h = 2.5 %/h.
        e.record(75);
        assert_eq!(e.rate_milli, 2500);
        assert_eq!(e.estimate(), 75);
        // Another hour of receiver use: 2.5 points down, truncated.
        for _ in 0..12 {
            now += 300;
            e.accumulate(true, now);
        }
        assert_eq!(e.usage_sec, 3600);
        assert_eq!(e.estimate(), 73);
    }

    #[test]
    fn idle_time_is_not_use_and_steps_are_capped() {
        let mut e = Estimate::default();
        e.record(90);
        e.accumulate(false, 100);
        e.accumulate(false, 100_000);
        assert_eq!(e.usage_sec, 0);
        // A 10-minute gap counts as MAX_STEP_SEC only, then a normal poll fully.
        e.accumulate(true, 100_300);
        assert_eq!(e.usage_sec, 300);
        e.accumulate(true, 100_400);
        assert_eq!(e.usage_sec, 400);
    }

    #[test]
    fn learned_rate_is_clamped_to_one_to_fifty_percent_an_hour() {
        let mut e = Estimate::default();
        e.record(50);
        for tick in 1u64..=24 {
            e.accumulate(true, tick * 300);
        }
        e.record(49); // 1% over an hour: the minimum
        assert_eq!(e.rate_milli, MIN_RATE_MILLI);

        let mut f = Estimate::default();
        f.record(100);
        f.accumulate(true, 300);
        for tick in 2u64..=7 {
            f.accumulate(true, tick * 300); // 1800 s of use
        }
        f.record(0); // 100% over half an hour: clamped to the maximum
        assert_eq!(f.rate_milli, MAX_RATE_MILLI);
    }

    #[test]
    fn g435_name_is_a_whole_word() {
        assert!(is_g435("G435"));
        assert!(is_g435("Logitech G435"));
        assert!(is_g435("G435 Hands-Free AG Audio"));
        assert!(!is_g435("G4350"));
        assert!(!is_g435("XG435"));
        assert!(!is_g435("Logitech G733"));
    }

    #[test]
    fn parses_instance_ids() {
        assert_eq!(device_address(r"BTHENUM\DEV_F5CFD77BCB0B\7&3057B904&0&BLUETOOTHDEVICE_F5CFD77BCB0B"), Some(0xF5CFD77BCB0B));
        assert_eq!(address_from_instance_id(r"BTHENUM\{0000111E-0000-1000-8000-00805F9B34FB}_VID&0001004C_PID&2014\7&3057B904&0&F5CFD77BCB0B_C00000000"), Some(0xF5CFD77BCB0B));
        assert!(is_hands_free(r"BTHENUM\{0000111E-0000-1000-8000-00805F9B34FB}_VID&0001004C_PID&2014\7&1"));
        assert!(!is_hands_free(r"BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&0001004C_PID&2014\7&1"));
    }
}
