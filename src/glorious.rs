//! Wireless mice speaking Sinowealth's 65-byte vendor feature-report protocol: Glorious,
//! Hitscan and the WLMouse YING.
//!
//! The device answers a 65-byte feature report (report id 0) shaped
//! `00 | 00 00 02 02 00 83 | 00 …`: device select 0x02, command 0x02, function 0x83. Reading the
//! same report back about 100 ms later carries the answer: `[1]` link status (0xA1 awake,
//! 0xA0 waking, 0xA4 asleep), `[6]` the 0x83 marker, `[7]` the charge byte and `[8]` the level.
//!
//! A mouse on its USB cable leaves the 2.4 GHz link, so the cable's own product id answering is
//! what marks it as charging.
//!
//! Protocol and ids from Wireless-Device-Battery-Monitor (drivers/glorious.cpp), whose notes
//! credit GloriousBatteryMonitor-Go, glorious-ctl, Hitscan's web configurator and WLMouse's
//! device list. Not verified in this project.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::Duration;

/// Data bytes of the request/response (report id 0 is the extra leading byte).
const DATA: usize = 64;
const MARKER: u8 = 0x83;
/// The receiver needs this long to query the mouse over 2.4 GHz.
const RF_WAIT: Duration = Duration::from_millis(100);

struct Model {
    vid: u16,
    pid: u16,
    source: &'static str,
    name: &'static str,
    cable: bool,
}

const MODELS: &[Model] = &[
    // Glorious: USB cable ids first, then the wireless receiver.
    Model { vid: 0x258A, pid: 0x2011, source: "glorious", name: "Glorious Model O Wireless", cable: true },
    Model { vid: 0x258A, pid: 0x2022, source: "glorious", name: "Glorious Model O Wireless", cable: false },
    Model { vid: 0x258A, pid: 0x2012, source: "glorious", name: "Glorious Model D Wireless", cable: true },
    Model { vid: 0x258A, pid: 0x2023, source: "glorious", name: "Glorious Model D Wireless", cable: false },
    Model { vid: 0x258A, pid: 0x2013, source: "glorious", name: "Glorious Model O- Wireless", cable: true },
    Model { vid: 0x258A, pid: 0x2024, source: "glorious", name: "Glorious Model O- Wireless", cable: false },
    Model { vid: 0x258A, pid: 0x2014, source: "glorious", name: "Glorious Model D- Wireless", cable: true },
    Model { vid: 0x258A, pid: 0x2025, source: "glorious", name: "Glorious Model D- Wireless", cable: false },
    Model { vid: 0x258A, pid: 0x201A, source: "glorious", name: "Glorious Model D 2 PRO", cable: true },
    Model { vid: 0x258A, pid: 0x2034, source: "glorious", name: "Glorious Model D 2 PRO", cable: false },
    Model { vid: 0x258A, pid: 0x2036, source: "glorious", name: "Glorious Model D 2 PRO", cable: false },
    Model { vid: 0x258A, pid: 0x2033, source: "glorious", name: "Glorious Model O 2 Wireless", cable: false },
    Model { vid: 0x3770, pid: 0x0136, source: "hitscan", name: "Hitscan Hyperlight X", cable: true },
    Model { vid: 0x3770, pid: 0x0194, source: "hitscan", name: "Hitscan Hyperlight N", cable: true },
    Model { vid: 0x36A7, pid: 0xA874, source: "wlmouse", name: "WLMouse YING", cable: false },
    // WLMouse family: cables first, then their receivers.
    Model { vid: 0x36A7, pid: 0xA879, source: "wlmouse", name: "WLMouse Sword X 8K", cable: true },
    Model { vid: 0x36A7, pid: 0xA884, source: "wlmouse", name: "WLMouse Beast X 8K", cable: true },
    Model { vid: 0x36A7, pid: 0xA861, source: "wlmouse", name: "WLMouse Beast G", cable: true },
    Model { vid: 0x36A7, pid: 0xA866, source: "wlmouse", name: "WLMouse mouse", cable: true },
    Model { vid: 0x36A7, pid: 0xA867, source: "wlmouse", name: "WLMouse mouse", cable: true },
    Model { vid: 0x36A7, pid: 0xA871, source: "wlmouse", name: "WLMouse Beast X Pro", cable: true },
    Model { vid: 0x36A7, pid: 0xA856, source: "wlmouse", name: "WLMouse Beast X V2", cable: false },
    Model { vid: 0x36A7, pid: 0xA860, source: "wlmouse", name: "WLMouse Beast G", cable: false },
    Model { vid: 0x36A7, pid: 0xA868, source: "wlmouse", name: "WLMouse Beast X Mini Pro", cable: false },
    Model { vid: 0x36A7, pid: 0xA870, source: "wlmouse", name: "WLMouse Beast X Pro", cable: false },
    Model { vid: 0x36A7, pid: 0xA878, source: "wlmouse", name: "WLMouse Sword X 8K", cable: false },
    Model { vid: 0x36A7, pid: 0xA880, source: "wlmouse", name: "WLMouse Beast MAX", cable: false },
    Model { vid: 0x36A7, pid: 0xA883, source: "wlmouse", name: "WLMouse Beast X 8K", cable: false },
    Model { vid: 0x36A7, pid: 0xA885, source: "wlmouse", name: "WLMouse Beast X Mini", cable: false },
];

#[derive(Default)]
pub struct Glorious {
    seen: HashSet<String>,
}

impl Glorious {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        // An attached cable's product id is a charging signal even when the dongle answers.
        let cables: Vec<&str> = MODELS
            .iter()
            .filter(|m| m.cable && api.device_list().any(|d| d.vendor_id() == m.vid && d.product_id() == m.pid))
            .map(|m| m.name)
            .collect();
        for model in MODELS {
            let key = format!("{}:{:04x}", model.source, model.pid);
            // Cables come first: skip the dongle once the same mouse reported a level.
            if out.iter().any(|o: &Observation| {
                o.name.as_deref() == Some(model.name) && matches!(o.state, State::Battery { .. })
            }) {
                continue;
            }
            let right_collection = |d: &hidapi::DeviceInfo| {
                if model.vid == 0x36A7 {
                    d.usage_page() == 0xFFFF && d.usage() == 0
                } else {
                    d.usage_page() >= 0xFF00
                }
            };
            let mut found = None;
            for info in api
                .device_list()
                .filter(|d| d.vendor_id() == model.vid && d.product_id() == model.pid && right_collection(d))
            {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::feature_len(&desc, 0) != Some(DATA + 1) {
                    continue;
                }
                match query(&dev) {
                    Ok(Some(reply)) => {
                        found = Some(reply);
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => diag::event(&key, format!("i/o failed: {e}")),
                }
            }
            match found {
                Some((status, charging, level)) if matches!(status, 0xA1 | 0xA2) && level <= 100 => {
                    let percent = level.max(1);
                    self.seen.insert(key.clone());
                    out.push(observation(
                        &key,
                        model.name,
                        State::Battery { percent, charging: model.cable || cables.contains(&model.name) || charging },
                    ));
                }
                Some(_) => {
                    self.seen.insert(key.clone());
                    out.push(observation(&key, model.name, State::Unreachable));
                }
                None => {
                    if self.seen.contains(&key) {
                        out.push(observation(&key, model.name, State::Unreachable));
                    }
                }
            }
        }
        out
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

fn query(dev: &HidDevice) -> hidapi::HidResult<Option<(u8, bool, u8)>> {
    let mut req = [0u8; DATA + 1];
    req[1..7].copy_from_slice(&[0x00, 0x00, 0x02, 0x02, 0x00, MARKER]);
    dev.send_feature_report(&req)?;
    std::thread::sleep(RF_WAIT);
    let mut buf = [0u8; DATA + 1];
    let n = dev.get_feature_report(&mut buf)?;
    if let Some((0xA0, charging, level)) = parse(&buf[..n]) {
        // Waking up: the receiver needs a moment before it has a level.
        std::thread::sleep(Duration::from_millis(400));
        dev.send_feature_report(&req)?;
        std::thread::sleep(RF_WAIT);
        let n = dev.get_feature_report(&mut buf)?;
        return Ok(parse(&buf[..n]).or(Some((0xA0, charging, level))));
    }
    Ok(parse(&buf[..n]))
}

/// `[6]` must be the 0x83 marker and `[1]` a link status in the 0xA0 range, then `[7]` charging
/// and `[8]` level.
fn parse(r: &[u8]) -> Option<(u8, bool, u8)> {
    if r.len() < 9 || r[6] != MARKER || (r[1] & 0xF0) != 0xA0 {
        return None;
    }
    Some((r[1], r[7] != 0, r[8]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(level: u8, charge: u8, status: u8) -> Vec<u8> {
        let mut r = vec![0u8; DATA + 1];
        r[1] = status;
        r[6] = MARKER;
        r[7] = charge;
        r[8] = level;
        r
    }

    #[test]
    fn parses_a_wireless_reply() {
        assert_eq!(parse(&reply(87, 0, 0xA1)), Some((0xA1, false, 87)));
        assert_eq!(parse(&reply(64, 1, 0xA1)), Some((0xA1, true, 64)));
        assert_eq!(parse(&reply(0, 0, 0xA4)), Some((0xA4, false, 0))); // asleep, not empty
    }

    #[test]
    fn rejects_noise_and_other_protocols() {
        assert_eq!(parse(&reply(87, 0, 0xA1)[..8]), None); // short
        let mut wrong = reply(50, 0, 0xA1);
        wrong[6] = 0x00; // no marker
        assert_eq!(parse(&wrong), None);
        assert_eq!(parse(&reply(50, 0, 0x01)), None); // not a link status
    }
}
