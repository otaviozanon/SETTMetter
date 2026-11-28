//! CROCIRIS X7 series wireless mice through their 8K receiver (36F9:AA20, "X7 Receiver 8K").
//!
//! CROCIRIS is XIBERIA's mouse brand; the X7 and X7 Ultra Max ship with this receiver.
//!
//! The vendor collection FF0A:0001 carries unnumbered 64-byte reports:
//!
//! ```text
//! request  AA <cmd> <len> 00 <payload…>
//! reply    55 <cmd> <len> 00 <payload…>
//! ```
//!
//! Command 0x12 (GetDeviceStatus, empty payload, read-only) answers `payload[0]` charging
//! (1 = yes), `payload[1]` the level, `payload[2]` full and `payload[3] & 0x0F` the link mode
//! (1 = the mouse is linked through this receiver).
//!
//! While the mouse sleeps the receiver keeps answering but reports 0 for the level. A mouse
//! in use is never at 0%, so 0 is treated as a missing reading: the last value is kept, dimmed,
//! until a plausible level arrives.
//!
//! Protocol read from the vendor's WebHID driver (hub.crociris.com) and confirmed on hardware
//! by Wireless-Device-Battery-Monitor (drivers/crociris.cpp).

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice, HidResult};
use std::time::{Duration, Instant};

const VID: u16 = 0x36F9;
const PID: u16 = 0xAA20;
const KEY: &str = "crociris:aa20";
const REQUEST_HEADER: u8 = 0xAA;
const REPLY_HEADER: u8 = 0x55;
const GET_DEVICE_STATUS: u8 = 0x12;
const REPLY_TIMEOUT: Duration = Duration::from_millis(1000);

#[derive(Default)]
pub struct Crociris {
    /// Whether the mouse ever answered; until then a silent receiver isn't listed.
    seen: bool,
}

impl Crociris {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let infos: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == 0xFF0A && d.usage() == 0x0001)
            .collect();
        if infos.is_empty() {
            return Vec::new();
        }

        let mut found = None;
        for info in infos {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, 0).is_none_or(|n| n < 4) || probe::input_len(&desc, 0).is_none_or(|n| n < 5) {
                continue;
            }
            let name = match info.product_string().unwrap_or("").trim() {
                "" => "CROCIRIS X7".to_string(),
                product => product.to_string(),
            };
            match read(&dev) {
                Ok(Some(state)) => {
                    found = Some((name, state));
                    break;
                }
                Ok(None) => {}
                Err(e) => diag::event(KEY, format!("i/o failed: {e}")),
            }
        }

        let (name, state) = match found {
            Some((name, state)) => {
                self.seen = true;
                (Some(name), state)
            }
            // Keep the name the tracker already holds rather than clobbering it.
            None if self.seen => (None, State::Unreachable),
            None => return Vec::new(),
        };
        vec![Observation { id: KEY.into(), name, kind: Some(Kind::Mouse), state }]
    }
}

fn read(dev: &HidDevice) -> HidResult<Option<State>> {
    drain(dev)?;
    // Unnumbered report: the buffer still starts with the report-id byte.
    dev.write(&[0x00, REQUEST_HEADER, GET_DEVICE_STATUS, 0x00, 0x00])?;

    let mut buf = [0u8; 128];
    let deadline = Instant::now() + REPLY_TIMEOUT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let n = match dev.read_timeout(&mut buf, left.as_millis().min(100).max(1) as i32) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) => return Err(e),
        };
        if let Some(state) = parse(&buf[..n]) {
            return Ok(Some(state));
        }
    }
    Ok(None)
}

fn drain(dev: &HidDevice) -> HidResult<()> {
    let mut buf = [0u8; 128];
    while dev.read_timeout(&mut buf, 0)? > 0 {}
    Ok(())
}

/// `55 12 <len> 00 <charging> <level> <full>`.
fn parse(r: &[u8]) -> Option<State> {
    if r.len() < 6 || r[0] != REPLY_HEADER || r[1] != GET_DEVICE_STATUS {
        return None;
    }
    let len = usize::from(r[2]);
    if len < 2 || r.len() < 4 + len {
        return None;
    }
    let charging = r[4];
    let percent = r[5];
    // While the mouse sleeps the receiver answers 0; a mouse in use is never at 0%.
    if charging > 1 || percent == 0 || percent > 100 {
        return None;
    }
    Some(State::Battery { percent, charging: charging == 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(charging: u8, percent: u8) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[..7].copy_from_slice(&[0x55, 0x12, 0x02, 0x00, charging, percent, 0x01]);
        r
    }

    #[test]
    fn parses_the_captured_layout() {
        // Vendor WebHID capture: 97% while linked through the receiver.
        assert_eq!(parse(&reply(0, 97)), Some(State::Battery { percent: 97, charging: false }));
        assert_eq!(parse(&reply(1, 97)), Some(State::Battery { percent: 97, charging: true }));
    }

    #[test]
    fn rejects_sleeping_and_implausible_frames() {
        assert_eq!(parse(&reply(0, 0)), None); // asleep, not empty
        assert_eq!(parse(&reply(0, 101)), None);
        assert_eq!(parse(&reply(2, 50)), None);
        let mut wrong = reply(0, 50);
        wrong[0] = 0xAA;
        assert_eq!(parse(&wrong), None);
        let mut short = reply(0, 50);
        short[2] = 250; // length beyond the report
        assert_eq!(parse(&short), None);
        assert_eq!(parse(&[0x55, 0x12, 0x02, 0x00]), None);
    }
}
