//! ASUS ROG wireless mice: the battery command G-Helper sends, `12 07`.
//!
//! On the ROG SpeedNova receiver (0B05:1AD0) the frame goes out as report id 3 on the vendor
//! collection, a 64-byte packet, and the reply echoes `12 07` with the level in byte 5 and the
//! charging state in byte 10 (bytes 8-9 are the cell voltage in mV, unused):
//! `03 12 07 00 00 35 02 14 fd 0e 00 00 …` = 53%, not charging, on a Harpe II Ace.
//! `FF AA` means the command is unknown, and a level of 0 without charging is standby, not empty.
//!
//! Only that one id is claimed: the rest of ASUS's 0B05 range is motherboards, laptops and LED
//! controllers (an AURA controller on the probe of the machine this was written on), which must
//! never be opened. The cable id (0B05:1C69) is left for when someone can capture it, since its
//! report-id framing differs and is unverified.
//!
//! Protocol from G-Helper's AsusMouse, with the capture above taken on real hardware.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::Duration;

const VID: u16 = 0x0B05;
const PID: u16 = 0x1AD0;
const KEY: &str = "asus:1ad0";
/// "ROG SPEEDNOVA 8K RECEIVER": report id 3, 64-byte packets both ways.
const REPORT_ID: u8 = 0x03;
const PACKET: usize = 64;
const COMMAND: [u8; 2] = [0x12, 0x07];
const ATTEMPTS: usize = 2;
const REPLY_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct Asus {
    dev: Option<HidDevice>,
}

impl Asus {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        if !api.device_list().any(|d| d.vendor_id() == VID && d.product_id() == PID) {
            self.dev = None;
            return Vec::new();
        }
        if self.dev.is_none() {
            self.dev = open(api);
            if self.dev.is_none() {
                diag::event(KEY, "no vendor collection taking a 64-byte report 0x03");
                return Vec::new();
            }
        }
        let state = match query(self.dev.as_ref().unwrap()) {
            Ok(Some((level, charging))) => {
                diag::event(KEY, format!("battery via report 0x03 (level {level}, charging {charging})"));
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
            name: Some("ASUS ROG Harpe II Ace".into()),
            kind: Some(Kind::Mouse),
            state,
        }]
    }
}

fn open(api: &HidApi) -> Option<HidDevice> {
    for d in api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == PID && d.usage_page() >= 0xFF00) {
        let Ok(dev) = api.open_path(d.path()) else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if probe::output_len(&desc, REPORT_ID) == Some(PACKET) {
            return Some(dev);
        }
    }
    None
}

fn query(dev: &HidDevice) -> hidapi::HidResult<Option<(u8, bool)>> {
    let mut req = [0u8; PACKET];
    req[0] = REPORT_ID;
    req[1..3].copy_from_slice(&COMMAND);

    let mut last_err = None;
    for _ in 0..ATTEMPTS {
        // A frame queued before the request isn't its answer.
        let mut stale = [0u8; PACKET];
        while let Ok(n) = dev.read_timeout(&mut stale, 0) {
            if n == 0 {
                break;
            }
        }
        if let Err(e) = dev.send_output_report(&req) {
            last_err = Some(e);
            continue;
        }
        let mut buf = [0u8; PACKET];
        let mut waited = Duration::ZERO;
        while waited < REPLY_TIMEOUT {
            match dev.read_timeout(&mut buf, 250) {
                Ok(0) => waited += Duration::from_millis(250),
                Ok(n) => {
                    if let Some(v) = parse(&buf[..n]) {
                        return Ok(Some(v));
                    }
                }
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            }
        }
    }
    match last_err {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

/// A valid reply echoes `12 07` after the report id: level in byte 5, charging in byte 10.
fn parse(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 11 || r[0] != REPORT_ID || r[1..3] != COMMAND {
        return None;
    }
    let level = r[5];
    let charging = r[10] != 0;
    if level > 100 || (level == 0 && !charging) {
        return None;
    }
    Some((level, charging))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(level: u8, charge: u8) -> Vec<u8> {
        let mut r = vec![0u8; PACKET];
        r[0] = REPORT_ID;
        r[1..3].copy_from_slice(&COMMAND);
        r[5] = level;
        r[10] = charge;
        r
    }

    #[test]
    fn parses_the_captured_frame() {
        // Harpe II Ace on its receiver, 53%, not charging.
        let frame = [0x03, 0x12, 0x07, 0x00, 0x00, 0x35, 0x02, 0x14, 0xfd, 0x0e, 0x00, 0x00];
        assert_eq!(parse(&frame), Some((53, false)));
        assert_eq!(parse(&reply(100, 1)), Some((100, true)));
    }

    #[test]
    fn rejects_unknown_command_and_standby() {
        let mut unknown = reply(0, 0);
        unknown[1] = 0xFF;
        unknown[2] = 0xAA;
        assert_eq!(parse(&unknown), None);
        assert_eq!(parse(&reply(0, 0)), None); // standby, not empty
        assert_eq!(parse(&reply(0, 1)), Some((0, true)));
        assert_eq!(parse(&reply(101, 0)), None);
        assert_eq!(parse(&reply(50, 0)[..10]), None); // short
    }
}
