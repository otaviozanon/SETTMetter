//! Logitech devices over HID++: Unifying / Lightspeed / Bolt receivers and directly attached
//! devices (e.g. headset dongles). Devices are discovered automatically.

mod curve;
pub mod hidpp;

use crate::device::{Kind, Observation, State};
use crate::diag;
use hidapi::HidApi;
use hidpp::{Endpoint, Error, Hidpp, Link, ERR_UNKNOWN_DEVICE};
use std::collections::HashMap;

/// Known receiver PIDs (Unifying, Nano, Lightspeed, Bolt). Anything named "Receiver" also counts.
const RECEIVER_PIDS: &[u16] = &[
    0xC517, 0xC518, 0xC51A, 0xC51B, 0xC521, 0xC525, 0xC526, 0xC52B, 0xC52E, 0xC52F, 0xC531, 0xC532,
    0xC534, 0xC537, 0xC539, 0xC53A, 0xC53D, 0xC53F, 0xC541, 0xC545, 0xC547, 0xC548, 0xC54D,
];

const DEVICE_FW_VERSION: u16 = 0x0003;
const DEVICE_NAME: u16 = 0x0005;
const BATTERY_STATUS: u16 = 0x1000;
const BATTERY_VOLTAGE: u16 = 0x1001;
const UNIFIED_BATTERY: u16 = 0x1004;
const ADC_MEASUREMENT: u16 = 0x1F20;

enum Raw {
    /// `feature`: the HID++ battery feature that answered, for diagnostics.
    Percent { percent: u8, charging: bool, feature: u16 },
    Millivolts { mv: u16, charging: bool, feature: u16 },
    Off,
    NoBattery,
}

#[derive(Clone)]
struct Identity {
    name: String,
    kind: Kind,
    /// Device PIDs to look up a G HUB discharge curve.
    pids: Vec<u16>,
}

pub struct Logitech {
    curves: curve::Curves,
    identities: HashMap<String, Identity>,
}

impl Logitech {
    pub fn new() -> Self {
        Self { curves: curve::Curves::load(), identities: HashMap::new() }
    }

    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for ep in hidpp::endpoints(api) {
            let receiver = RECEIVER_PIDS.contains(&ep.pid) || ep.product.to_ascii_lowercase().contains("receiver");
            let key = format!("logi:{:04x}:{}", ep.pid, ep.iface);
            let what = format!("\"{}\" ({})", ep.product, if receiver { "receiver" } else { "direct" });
            let link = match Link::open(api, &ep) {
                Ok(link) => link,
                Err(e) => {
                    diag::event(&key, format!("{what}: open failed: {e}"));
                    continue;
                }
            };
            diag::event(&key, format!("{what}: opened"));
            if receiver {
                out.extend((1..=6).filter_map(|i| self.probe(&link, &ep, i, true)));
            } else {
                out.extend(self.probe(&link, &ep, 0xFF, false));
            }
        }
        out
    }

    fn probe(&mut self, link: &dyn Hidpp, ep: &Endpoint, index: u8, via_receiver: bool) -> Option<Observation> {
        let id = format!("logi:{:04x}:{}:{:02x}", ep.pid, ep.iface, index);
        let mut result = battery(link, index);
        if via_receiver && !self.identities.contains_key(&id) {
            // A sleeping device times out on the first request but starts waking up from it.
            // Give it a moment and retry so it gets discovered. Known devices aren't woken
            // (it costs them battery); they keep their last reading instead.
            for _ in 0..2 {
                if !matches!(result, Err(Error::Timeout)) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
                result = battery(link, index);
            }
        }
        let answered = result.is_ok();
        let raw = match result {
            Ok(Raw::NoBattery) => {
                diag::event(&id, "no battery feature (0x1000, 0x1001, 0x1004, 0x1F20)");
                return None;
            }
            Ok(raw) => raw,
            Err(Error::Protocol(ERR_UNKNOWN_DEVICE)) if via_receiver => return None, // empty slot
            Err(e) => {
                if via_receiver {
                    // Paired but not answering (asleep/off). Only meaningful if we know it.
                    diag::event(&id, format!("no answer: {e}"));
                    let known = self.identities.get(&id)?;
                    return Some(Observation {
                        id,
                        name: Some(known.name.clone()),
                        kind: Some(known.kind),
                        state: State::Unreachable,
                    });
                }
                // Direct device that doesn't answer: report it only if it looks like a
                // wireless dongle (e.g. headset switched off), otherwise ignore it.
                let looks_wireless = ["wireless", "headset"].iter().any(|k| ep.product.to_ascii_lowercase().contains(k));
                if !self.identities.contains_key(&id) && !looks_wireless {
                    diag::event(&id, format!("no answer: {e} (ignored: not a wireless dongle)"));
                    return None;
                }
                diag::event(&id, format!("no answer: {e} (shown as off)"));
                Raw::Off
            }
        };

        if via_receiver && matches!(raw, Raw::Off) && !self.identities.contains_key(&id) {
            // "Off" comes from headset dongles; behind a receiver it means a garbled reading.
            // Without an identity the device would be listed under the receiver's name.
            diag::event(&id, "off, but not identified yet: skipped");
            return None;
        }
        let new = !self.identities.contains_key(&id) && !matches!(raw, Raw::Off);
        if new {
            let identity = identify(link, index, ep, via_receiver);
            diag::event(&format!("{id} identity"), format!("\"{}\", {:?}, model pids {:04x?}", identity.name, identity.kind, identity.pids));
            self.identities.insert(id.clone(), identity);
        }
        let identity = self.identities.get(&id).cloned().unwrap_or_else(|| Identity {
            name: ep.product.clone(),
            kind: Kind::from_name(&ep.product),
            pids: vec![ep.pid],
        });

        let state = match raw {
            Raw::Percent { percent, charging, feature } => {
                diag::event(&id, format!("battery via HID++ 0x{feature:04X} (percent)"));
                State::Battery { percent, charging }
            }
            Raw::Millivolts { mv, charging, feature } => {
                let percent = self.curves.percent(&identity.pids, mv);
                diag::event(&id, format!("battery via HID++ 0x{feature:04X} (voltage)"));
                if new {
                    // One sample per device: enough to check the curve without logging every reading.
                    diag::event(&format!("{id} sample"), format!("{mv} mV -> {percent}%"));
                }
                State::Battery { percent, charging }
            }
            Raw::Off => {
                if answered {
                    diag::event(&id, "off (dongle reports the device isn't linked)");
                }
                State::Off
            }
            Raw::NoBattery => State::Off,
        };
        Some(Observation { id, name: Some(identity.name), kind: Some(identity.kind), state })
    }
}

fn battery(link: &dyn Hidpp, index: u8) -> Result<Raw, Error> {
    if let Some(f) = link.feature_index(index, UNIFIED_BATTERY)? {
        let r = link.request(index, f, 0x1, &[])?; // get_status
        return Ok(Raw::Percent { percent: r[0], charging: matches!(r[2], 1 | 2), feature: UNIFIED_BATTERY });
    }
    if let Some(f) = link.feature_index(index, BATTERY_STATUS)? {
        let r = link.request(index, f, 0x0, &[])?;
        return Ok(Raw::Percent { percent: r[0], charging: matches!(r[2], 1 | 2 | 4), feature: BATTERY_STATUS });
    }
    if let Some(f) = link.feature_index(index, BATTERY_VOLTAGE)? {
        let r = link.request(index, f, 0x0, &[])?;
        return Ok(Raw::Millivolts { mv: u16::from_be_bytes([r[0], r[1]]), charging: r[2] & 0x80 != 0, feature: BATTERY_VOLTAGE });
    }
    if let Some(f) = link.feature_index(index, ADC_MEASUREMENT)? {
        let r = link.request(index, f, 0x0, &[])?;
        if r[2] & 0x01 == 0 {
            return Ok(Raw::Off); // dongle present, device off
        }
        return Ok(Raw::Millivolts { mv: u16::from_be_bytes([r[0], r[1]]), charging: r[2] & 0x02 != 0, feature: ADC_MEASUREMENT });
    }
    Ok(Raw::NoBattery)
}

fn identify(link: &dyn Hidpp, index: u8, ep: &Endpoint, via_receiver: bool) -> Identity {
    let mut name = None;
    let mut kind = None;
    if let Ok(Some(f)) = link.feature_index(index, DEVICE_NAME) {
        if let Ok(r) = link.request(index, f, 0x0, &[]) {
            let len = r[0] as usize;
            let mut bytes = Vec::with_capacity(len);
            while bytes.len() < len {
                let Ok(chunk) = link.request(index, f, 0x1, &[bytes.len() as u8]) else { break };
                let take = (len - bytes.len()).min(16);
                bytes.extend_from_slice(&chunk[..take]);
            }
            let s = String::from_utf8_lossy(&bytes).trim_matches(char::from(0)).trim().to_string();
            name = (!s.is_empty()).then_some(s);
        }
        if let Ok(r) = link.request(index, f, 0x2, &[]) {
            kind = match r[0] {
                0 | 2 => Some(Kind::Keyboard),
                3..=5 => Some(Kind::Mouse),
                8 => Some(Kind::Headset),
                10..=12 => Some(Kind::Gamepad),
                _ => None,
            };
        }
    }

    // Model ids hold the device's PIDs per transport; used to find its G HUB curve.
    let mut pids = if via_receiver { Vec::new() } else { vec![ep.pid] };
    if let Ok(Some(f)) = link.feature_index(index, DEVICE_FW_VERSION) {
        if let Ok(r) = link.request(index, f, 0x0, &[]) {
            pids.extend((0..3).map(|i| u16::from_be_bytes([r[7 + 2 * i], r[8 + 2 * i]])).filter(|p| *p != 0));
        }
    }

    let fallback = if via_receiver { "Logitech device".to_string() } else { ep.product.clone() };
    let name = name.unwrap_or(fallback);
    let kind = kind.unwrap_or_else(|| Kind::from_name(&name));
    Identity { name, kind, pids }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Clone)]
    enum Reply {
        Ok(Vec<u8>),
        Timeout,
        Protocol(u8),
    }

    /// Scripted device: feature table (id -> index) plus canned replies per (index, function).
    struct Fake {
        features: HashMap<u16, u8>,
        replies: HashMap<(u8, u8), Reply>,
    }

    impl Fake {
        fn new(features: &[(u16, u8)]) -> Self {
            Self { features: features.iter().copied().collect(), replies: HashMap::new() }
        }
        fn reply(mut self, feature_idx: u8, function: u8, r: Reply) -> Self {
            self.replies.insert((feature_idx, function), r);
            self
        }
    }

    impl Hidpp for Fake {
        fn request(&self, _index: u8, feature_idx: u8, function: u8, params: &[u8]) -> Result<[u8; 16], Error> {
            let pad = |v: &[u8]| {
                let mut out = [0u8; 16];
                out[..v.len()].copy_from_slice(v);
                out
            };
            if feature_idx == 0 {
                let id = u16::from_be_bytes([params[0], params[1]]);
                return Ok(pad(&[self.features.get(&id).copied().unwrap_or(0)]));
            }
            match self.replies.get(&(feature_idx, function)).cloned().unwrap_or(Reply::Timeout) {
                Reply::Ok(v) => Ok(pad(&v)),
                Reply::Timeout => Err(Error::Timeout),
                Reply::Protocol(c) => Err(Error::Protocol(c)),
            }
        }
    }

    fn raw(r: Result<Raw, Error>) -> Raw {
        r.unwrap_or_else(|e| panic!("unexpected error: {e}"))
    }

    // G733-style headset: only 0x1F20 (ADC measurement), at feature index 8 like HeadsetControl uses.
    fn g733(adc: Reply) -> Fake {
        Fake::new(&[(ADC_MEASUREMENT, 8)]).reply(8, 0, adc)
    }

    fn logi() -> Logitech {
        Logitech { curves: curve::Curves::load(), identities: HashMap::new() }
    }

    fn endpoint(pid: u16, product: &str) -> Endpoint {
        Endpoint { pid, iface: 3, product: product.into(), short: None, long: std::ffi::CString::new("x").unwrap() }
    }

    #[test]
    fn adc_headset_reports_millivolts() {
        // 4000 mV = 0x0FA0, flags: linked.
        let Raw::Millivolts { mv, charging, .. } = raw(battery(&g733(Reply::Ok(vec![0x0F, 0xA0, 0x01])), 0xFF)) else {
            panic!("not millivolts")
        };
        assert_eq!((mv, charging), (4000, false));
    }

    #[test]
    fn adc_headset_charging_flag() {
        let Raw::Millivolts { charging, .. } = raw(battery(&g733(Reply::Ok(vec![0x10, 0x68, 0x03])), 0xFF)) else {
            panic!("not millivolts")
        };
        assert!(charging);
    }

    #[test]
    fn adc_headset_not_linked_is_off() {
        assert!(matches!(raw(battery(&g733(Reply::Ok(vec![0x0F, 0xA0, 0x00])), 0xFF)), Raw::Off));
    }

    #[test]
    fn unified_battery_wins_over_other_features() {
        let dev = Fake::new(&[(UNIFIED_BATTERY, 5), (BATTERY_VOLTAGE, 6)]).reply(5, 1, Reply::Ok(vec![72, 0, 1]));
        assert!(matches!(raw(battery(&dev, 1)), Raw::Percent { percent: 72, charging: true, feature: UNIFIED_BATTERY }));
    }

    #[test]
    fn battery_status_percent() {
        let dev = Fake::new(&[(BATTERY_STATUS, 4)]).reply(4, 0, Reply::Ok(vec![55, 0, 0]));
        assert!(matches!(raw(battery(&dev, 1)), Raw::Percent { percent: 55, charging: false, feature: BATTERY_STATUS }));
    }

    #[test]
    fn battery_voltage_charging_bit() {
        let dev = Fake::new(&[(BATTERY_VOLTAGE, 3)]).reply(3, 0, Reply::Ok(vec![0x0E, 0x10, 0x80]));
        assert!(matches!(raw(battery(&dev, 1)), Raw::Millivolts { mv: 3600, charging: true, .. }));
    }

    #[test]
    fn device_without_battery_feature() {
        assert!(matches!(raw(battery(&Fake::new(&[]), 1)), Raw::NoBattery));
    }

    #[test]
    fn silent_device_times_out() {
        assert!(matches!(battery(&g733(Reply::Timeout), 0xFF), Err(Error::Timeout)));
    }

    #[test]
    fn direct_headset_is_observed_with_name_and_percent() {
        let dev = g733(Reply::Ok(vec![0x0F, 0xA0, 0x01]));
        let o = logi().probe(&dev, &endpoint(0x0AB5, "G733 Gaming Headset"), 0xFF, false).expect("observed");
        assert_eq!(o.name.as_deref(), Some("G733 Gaming Headset"));
        assert!(matches!(o.state, State::Battery { charging: false, .. }));
    }

    #[test]
    fn silent_headset_named_headset_shows_as_off() {
        let o = logi().probe(&g733(Reply::Timeout), &endpoint(0x0AB5, "G733 Gaming Headset"), 0xFF, false).expect("observed");
        assert!(matches!(o.state, State::Off));
    }

    #[test]
    fn silent_direct_device_without_wireless_name_is_ignored() {
        assert!(logi().probe(&g733(Reply::Timeout), &endpoint(0x0B1F, "USB Dongle"), 0xFF, false).is_none());
    }

    #[test]
    fn unidentified_receiver_device_reading_off_is_skipped() {
        let dev = Fake::new(&[(ADC_MEASUREMENT, 8)]).reply(8, 0, Reply::Ok(vec![0x0F, 0xA0, 0x00]));
        assert!(logi().probe(&dev, &endpoint(0xC539, "USB Receiver"), 1, true).is_none());
    }

    #[test]
    fn empty_receiver_slot_is_skipped() {
        let dev = Fake::new(&[(UNIFIED_BATTERY, 5)]).reply(5, 1, Reply::Protocol(ERR_UNKNOWN_DEVICE));
        assert!(logi().probe(&dev, &endpoint(0xC52B, "USB Receiver"), 2, true).is_none());
    }
}
