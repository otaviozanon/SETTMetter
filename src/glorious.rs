//! Glorious Model O Wireless (Sinowealth 2.4 GHz dongle, 258A:2022).
//!
//! The dongle answers a 65-byte feature report (report id 0) shaped
//! `00 | 00 00 02 02 00 83 | 00 …`: device select 0x02, command 0x02, function 0x83. The same
//! report read back about 100 ms later carries the level:
//! `[1]` link status (0xA1 awake, 0xA2 waking, 0xA4 asleep), `[6]` the 0x83 marker, `[7]` the
//! charge byte (0 not charging, 1 charging, 2 fully charged) and `[8]` the level.
//!
//! While on the cable the mouse leaves the 2.4 GHz link and the dongle goes quiet, so the wired
//! id (258A:2011) appearing is what marks it as charging.
//!
//! Wire format from GloriousBatteryMonitor-Go (HidDeviceService.cs), which reads a Model O
//! Wireless. Not verified in this project.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::Duration;

const VID: u16 = 0x258A;
const PID: u16 = 0x2022;
const WIRED_PID: u16 = 0x2011;
const KEY: &str = "glorious:2022";
/// Data bytes of the request/response (report id 0 is the extra leading byte).
const DATA: usize = 64;
const MARKER: u8 = 0x83;
/// The receiver needs this long to query the mouse over 2.4 GHz.
const RF_WAIT: Duration = Duration::from_millis(100);

#[derive(Default)]
pub struct Glorious {
    dev: Option<HidDevice>,
}

impl Glorious {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let devs: Vec<_> = api.device_list().filter(|d| d.vendor_id() == VID).collect();
        let present = devs.iter().any(|d| d.product_id() == PID);
        let wired = devs.iter().any(|d| d.product_id() == WIRED_PID);
        if !present {
            self.dev = None;
            return Vec::new();
        }
        if self.dev.is_none() {
            self.dev = open(api);
            if self.dev.is_none() {
                diag::event(KEY, "no vendor collection with a 65-byte feature report");
                return Vec::new();
            }
        }
        let state = match query(self.dev.as_ref().unwrap()) {
            Ok(Some((level, charging))) => {
                diag::event(KEY, format!("battery via report 0x83 (level {level}, charging {charging})"));
                State::Battery { percent: level, charging }
            }
            Ok(None) => {
                diag::event(KEY, "no valid reply (mouse asleep or off)");
                State::Unreachable
            }
            Err(e) => {
                diag::event(KEY, format!("i/o failed: {e}"));
                self.dev = None;
                return Vec::new();
            }
        };
        vec![Observation {
            id: KEY.into(),
            name: Some("Glorious Model O Wireless".into()),
            kind: Some(Kind::Mouse),
            // The wired id is the only reliable charging signal: off the dongle the reply's
            // charge byte reads 0 while the cable is in.
            state: match state {
                State::Battery { percent, charging: false } if wired => State::Battery { percent, charging: true },
                other => other,
            },
        }]
    }
}

/// The one vendor collection whose feature report 0 is 65 bytes; the receiver publishes more
/// than one vendor page and only this one answers the battery query.
fn open(api: &HidApi) -> Option<HidDevice> {
    for d in api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == PID && d.usage_page() >= 0xFF00) {
        let Ok(dev) = api.open_path(d.path()) else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if probe::feature_len(&desc, 0) == Some(DATA) {
            return Some(dev);
        }
    }
    None
}

fn query(dev: &HidDevice) -> hidapi::HidResult<Option<(u8, bool)>> {
    let mut req = [0u8; DATA + 1];
    req[1..7].copy_from_slice(&[0x00, 0x00, 0x02, 0x02, 0x00, MARKER]);
    dev.send_feature_report(&req)?;
    std::thread::sleep(RF_WAIT);
    let mut buf = [0u8; DATA + 1];
    let n = dev.get_feature_report(&mut buf)?;
    Ok(parse(&buf[..n]))
}

/// `[6]` must be the 0x83 marker, then `[7]` charging and `[8]` level. An asleep mouse answers
/// with status 0xA4 and a level of 0, which is not "empty".
fn parse(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 9 || r[6] != MARKER || r[8] > 100 {
        return None;
    }
    if r[1] == 0xA4 && r[8] == 0 {
        return None;
    }
    Some((r[8], r[7] != 0))
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
        assert_eq!(parse(&reply(87, 0, 0xA1)), Some((87, false)));
        assert_eq!(parse(&reply(64, 1, 0xA1)), Some((64, true)));
    }

    #[test]
    fn rejects_noise_and_sleeping_zero() {
        assert_eq!(parse(&reply(87, 0, 0xA1)[..8]), None); // short
        assert_eq!(parse(&reply(0, 0, 0xA4)), None); // asleep, level unknown
        assert_eq!(parse(&reply(101, 0, 0xA1)), None); // out of range
        let mut wrong = reply(50, 0, 0xA1);
        wrong[6] = 0x00; // no marker
        assert_eq!(parse(&wrong), None);
    }
}
