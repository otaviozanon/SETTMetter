//! Press Play ICARUS V2 Nearlink mouse (7768:6434) on its 2.4 GHz receiver.
//!
//! Vendor collection FFB2:0001 with 64-byte report 0x06 both ways. `06 05` asks for the
//! battery and the documented reply is
//!
//! ```text
//! 06 05 26 … <percent> …      (percent at byte 13, 0..100)
//! ```
//!
//! Only that one read-only request is sent.
//!
//! Protocol from Wireless-Device-Battery-Monitor (drivers/pressplay_icarus.cpp).
//!
//! Deliberately skipped: the AJAZZ AJ159NL P (7768:6425) that shares this file upstream. Its
//! level only shows up after replaying the vendor app's startup/config exchange, which is a
//! write sequence, so it is left out until someone captures a passive battery push alone.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice, HidResult};
use std::time::{Duration, Instant};

const VID: u16 = 0x7768;
const PID: u16 = 0x6434;
const KEY: &str = "pressplay:6434";
const PAGE: u16 = 0xFFB2;
const USAGE: u16 = 0x0001;
const REPORT_ID: u8 = 0x06;
const BATTERY_CMD: u8 = 0x05;
const REPLY_MARKER: u8 = 0x26;
const PERCENT_OFFSET: usize = 13;
const REPLY_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct Pressplay {
    /// Whether the mouse ever answered; until then a silent receiver isn't listed.
    seen: bool,
}

impl Pressplay {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let infos: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == PAGE && d.usage() == USAGE)
            .collect();
        if infos.is_empty() {
            return Vec::new();
        }

        let mut state = None;
        for info in infos {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::input_len(&desc, REPORT_ID) != Some(64) || probe::output_len(&desc, REPORT_ID) != Some(64) {
                continue;
            }
            match read(&dev) {
                Ok(Some(percent)) => {
                    state = Some(State::Battery { percent, charging: false });
                    break;
                }
                Ok(None) => {}
                Err(e) => diag::event(KEY, format!("i/o failed: {e}")),
            }
        }

        let state = match state {
            Some(s) => {
                self.seen = true;
                s
            }
            None if self.seen => State::Unreachable,
            None => return Vec::new(),
        };
        vec![Observation {
            id: KEY.into(),
            name: Some("Press Play ICARUS V2 Nearlink".into()),
            kind: Some(Kind::Mouse),
            state,
        }]
    }
}

fn read(dev: &HidDevice) -> HidResult<Option<u8>> {
    drain(dev)?;
    let mut req = [0u8; 64];
    req[0] = REPORT_ID;
    req[1] = BATTERY_CMD;
    // The collection has no interrupt OUT pipe on some receivers, so use SetOutputReport.
    if dev.send_output_report(&req).is_err() && dev.write(&req).is_err() {
        return Ok(None);
    }

    let mut buf = [0u8; 128];
    let deadline = Instant::now() + REPLY_TIMEOUT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis().max(1) as i32) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(percent) = parse(&buf[..n]) {
                    return Ok(Some(percent));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

fn drain(dev: &HidDevice) -> HidResult<()> {
    let mut buf = [0u8; 128];
    while dev.read_timeout(&mut buf, 0)? > 0 {}
    Ok(())
}

/// `06 05 26 …`, level at byte 13.
fn parse(r: &[u8]) -> Option<u8> {
    if r.len() < PERCENT_OFFSET + 1 || r[0] != REPORT_ID || r[1] != BATTERY_CMD || r[2] != REPLY_MARKER {
        return None;
    }
    let percent = r[PERCENT_OFFSET];
    (percent <= 100).then_some(percent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(percent: u8) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[..3].copy_from_slice(&[0x06, 0x05, 0x26]);
        r[PERCENT_OFFSET] = percent;
        r
    }

    #[test]
    fn parses_the_battery_reply() {
        assert_eq!(parse(&reply(87)), Some(87));
        assert_eq!(parse(&reply(0)), Some(0));
        assert_eq!(parse(&reply(100)), Some(100));
    }

    #[test]
    fn rejects_other_frames() {
        assert_eq!(parse(&reply(101)), None);
        assert_eq!(parse(&reply(87)[..13]), None); // no level byte
        let mut wrong = reply(87);
        wrong[2] = 0x27;
        assert_eq!(parse(&wrong), None);
        let mut echo = reply(87);
        echo[1] = 0x07; // another command's echo
        assert_eq!(parse(&echo), None);
    }
}
