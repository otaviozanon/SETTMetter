//! VAXEE 4K/8K receivers and the ZYGEN NP-01S V2 USB cable.
//!
//! Protocol from stuffz/mouse-battery-monitor (docs/VAXEE.md): the vendor collection 0xFF05:0001
//! has a 64-byte feature report 0x0E carrying
//! `0E A5 <command> 01 01 00 …`. The same report read back 100 ms later answers
//! `0E A5 <command> … 01 <value>`; command 0x0B is the battery in 5% steps (0…20) and 0x10 the
//! charging flag. Command byte 3 is 0x01 (read) so nothing is ever written to the mouse.
//!
//! Only these two read-only queries are sent. Ids and frames from Wireless-Device-Battery-Monitor
//! (drivers/vaxee.cpp).

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::Duration;

const VID: u16 = 0x3057;
const REPORT_ID: u8 = 0x0E;
const LEN: usize = 64;
const BATTERY: u8 = 0x0B;
const CHARGING: u8 = 0x10;
const WAIT: Duration = Duration::from_millis(100);
/// The battery answers in 5% steps, 0 to 20.
const MAX_STEP: u8 = 20;

struct Model {
    pid: u16,
    name: &'static str,
}

const MODELS: &[Model] = &[
    Model { pid: 0x1002, name: "VAXEE wireless mouse" }, // 4K receiver
    Model { pid: 0x2002, name: "VAXEE 8K Wireless" },    // 8K receiver
    Model { pid: 0x1011, name: "ZYGEN NP-01S V2 Wireless" }, // USB cable
];

#[derive(Default)]
pub struct Vaxee {
    /// A receiver that answered once is reported asleep instead of disappearing.
    seen: HashSet<String>,
}

impl Vaxee {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out: Vec<Observation> = Vec::new();
        for model in MODELS {
            let key = format!("vaxee:{:04x}", model.pid);
            if out.iter().any(|o| o.id == key) {
                continue;
            }
            let mut found = None;
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID && d.product_id() == model.pid && d.usage_page() == 0xFF05 && d.usage() == 0x0001
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::feature_len(&desc, REPORT_ID) != Some(LEN) {
                    continue;
                }
                let mut level = None;
                for _ in 0..2 {
                    match query(&dev, BATTERY) {
                        Ok(Some(v)) if v <= MAX_STEP => {
                            level = Some(v);
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            diag::event(&key, format!("i/o failed: {e}"));
                            break;
                        }
                    }
                }
                if let Some(level) = level {
                    // A failed charging read leaves the flag unknown, not false.
                    let charging = query(&dev, CHARGING).ok().flatten().is_some_and(|v| v != 0);
                    found = Some((level, charging));
                    break;
                }
            }
            match found {
                Some((level, charging)) => {
                    self.seen.insert(key.clone());
                    out.push(observation(&key, model.name, State::Battery { percent: level * 5, charging }));
                }
                None if self.seen.contains(&key) => {
                    out.push(observation(&key, model.name, State::Unreachable));
                }
                None => {}
            }
        }
        out
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

fn query(dev: &HidDevice, command: u8) -> hidapi::HidResult<Option<u8>> {
    let mut req = [0u8; LEN];
    req[0] = REPORT_ID;
    req[1] = 0xA5;
    req[2] = command;
    req[3] = 0x01; // READ, not WRITE (0x02)
    req[4] = 0x01;
    dev.send_feature_report(&req)?;
    std::thread::sleep(WAIT);
    let mut buf = [0u8; LEN];
    buf[0] = REPORT_ID;
    let n = dev.get_feature_report(&mut buf)?;
    Ok(parse(&buf[..n], command))
}

/// `0E A5 <command> … 01 <value>`.
fn parse(r: &[u8], command: u8) -> Option<u8> {
    (r.len() >= 6 && r[0] == REPORT_ID && r[1] == 0xA5 && r[2] == command && r[4] == 0x01).then(|| r[5])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(command: u8, value: u8) -> Vec<u8> {
        let mut r = vec![0u8; LEN];
        r[0] = REPORT_ID;
        r[1] = 0xA5;
        r[2] = command;
        r[4] = 0x01;
        r[5] = value;
        r
    }

    #[test]
    fn parses_battery_and_charging() {
        assert_eq!(parse(&reply(BATTERY, 12), BATTERY), Some(12));
        assert_eq!(parse(&reply(CHARGING, 1), CHARGING), Some(1));
        assert_eq!(parse(&reply(BATTERY, 12), CHARGING), None); // answer to another command
        assert_eq!(parse(&reply(BATTERY, 12)[..5], BATTERY), None); // short
    }

    #[test]
    fn battery_step_is_five_percent() {
        assert_eq!(parse(&reply(BATTERY, 20), BATTERY).map(|v| v * 5), Some(100));
        assert_eq!(parse(&reply(BATTERY, 0), BATTERY).map(|v| v * 5), Some(0));
        // A receiver answers 0 before it hears the mouse; anything above 20 isn't a level.
        assert!(parse(&reply(BATTERY, 21), BATTERY).is_some_and(|v| v > MAX_STEP));
    }
}
