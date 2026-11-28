//! Alienware AW920H Tri-Mode Wireless Gaming Headset on its 2.4 GHz dongle (413C:A515).
//!
//! Collection: interface 5, usage page FF13:0001; output report 0x06 / input report 0x07,
//! 62 bytes each. `06 C0 0A` asks for the battery; the reply is
//!
//! ```text
//! 07 C0 0A 09 00 <linked> <percent> <charging> <critical> .. <critical percent> <mode> <checksum>
//! ```
//!
//! `[13]` is the XOR of bytes 3..12, `[7]` is 1 while charging and `[8]` 1 when the battery is
//! critical (the level is then taken from `[11]` instead of `[6]`). `07 00 80` means "no
//! answer yet"; the dongle answers after 2-3 reads (30-100 ms) while the headset is on and
//! keeps answering `00 80` while it is off. About one query in a hundred is dropped, so it is
//! resent. Example: `07 C0 0A 09 00 01 5F 00 00 1C 10 5F 02 06` = 95%, not charging.
//!
//! Protocol from Wireless-Device-Battery-Monitor (drivers/alienware.cpp), decoded from Alienware
//! Command Center's AW920H plugin. Only this read-only query is sent. In Bluetooth mode
//! (413C:A514) Windows keeps the level itself, so it is not handled here.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice, HidResult};
use std::time::{Duration, Instant};

const VID: u16 = 0x413C;
const PID: u16 = 0xA515;
const KEY: &str = "alienware:a515";
const REQUEST_ID: u8 = 0x06;
const REPLY_ID: u8 = 0x07;
const CMD0: u8 = 0xC0;
const CMD1: u8 = 0x0A;
const ATTEMPTS: usize = 3;
const ATTEMPT_WAIT: Duration = Duration::from_millis(180);
const TOTAL_WAIT: Duration = Duration::from_millis(600);

#[derive(Default)]
pub struct Alienware {
    /// Whether the headset ever answered; until then a silent dongle isn't listed.
    seen: bool,
}

impl Alienware {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let infos: Vec<_> = api
            .device_list()
            .filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == PID
                    && d.interface_number() == 5
                    && d.usage_page() == 0xFF13
                    && d.usage() == 0x0001
            })
            .collect();
        if infos.is_empty() {
            return Vec::new();
        }

        let mut state = None;
        for info in infos {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, REQUEST_ID) != Some(62) || probe::input_len(&desc, REPLY_ID) != Some(62) {
                continue;
            }
            match read(&dev) {
                Ok(Some(s)) => {
                    state = Some(s);
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
        vec![Observation { id: KEY.into(), name: Some("Alienware AW920H".into()), kind: Some(Kind::Headset), state }]
    }
}

fn read(dev: &HidDevice) -> HidResult<Option<State>> {
    let mut req = [0u8; 62];
    req[0] = REQUEST_ID;
    req[1] = CMD0;
    req[2] = CMD1;

    let deadline = Instant::now() + TOTAL_WAIT;
    for _ in 0..ATTEMPTS {
        if Instant::now() >= deadline {
            break;
        }
        // The dongle has no interrupt OUT pipe: send the way Command Center does.
        if dev.send_output_report(&req).is_err() && dev.write(&req).is_err() {
            break;
        }
        let attempt_end = (Instant::now() + ATTEMPT_WAIT).min(deadline);
        while Instant::now() < attempt_end {
            std::thread::sleep(Duration::from_millis(15));
            // Take the reply from the interrupt pipe if it queued there.
            let mut buf = [0u8; 128];
            while let Ok(n) = dev.read_timeout(&mut buf, 0) {
                if n == 0 {
                    break;
                }
                if let Some(s) = decode(&buf[..n]) {
                    return Ok(Some(s));
                }
            }
            let mut buf = [0u8; 62];
            buf[0] = REPLY_ID;
            if let Ok(n) = dev.get_input_report(&mut buf)
                && let Some(s) = decode(&buf[..n])
            {
                return Ok(Some(s));
            }
        }
    }
    Ok(None)
}

/// `07 C0 0A 09`, the checksum, then `[6]` (or `[11]` when critical) and `[7]` charging.
fn decode(r: &[u8]) -> Option<State> {
    if r.len() < 14 || r[0] != REPLY_ID || r[1] != CMD0 || r[2] != CMD1 || r[3] != 0x09 {
        return None;
    }
    let mut x = 0u8;
    for b in &r[3..=12] {
        x ^= b;
    }
    if x != r[13] || r[6] == 0 {
        return None;
    }
    let percent = if r[8] == 1 { r[11] } else { r[6] };
    if percent > 100 {
        return None;
    }
    Some(State::Battery { percent, charging: r[7] == 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_captured_frame() {
        // 07 C0 0A 09 00 01 5F 00 00 1C 10 5F 02 06 = 95%, not charging.
        let frame = [0x07, 0xC0, 0x0A, 0x09, 0x00, 0x01, 0x5F, 0x00, 0x00, 0x1C, 0x10, 0x5F, 0x02, 0x06];
        assert_eq!(decode(&frame), Some(State::Battery { percent: 95, charging: false }));

        let mut charging = frame;
        charging[7] = 1;
        charging[13] = 0x06 ^ 0x01; // keep the XOR of bytes 3..12 valid
        assert_eq!(decode(&charging), Some(State::Battery { percent: 95, charging: true }));
    }

    #[test]
    fn rejects_a_bad_checksum_and_a_zero_level() {
        let mut frame = [0x07, 0xC0, 0x0A, 0x09, 0x00, 0x01, 0x5F, 0x00, 0x00, 0x1C, 0x10, 0x5F, 0x02, 0x06];
        frame[13] ^= 0xFF;
        assert_eq!(decode(&frame), None);
        frame[13] ^= 0xFF;
        frame[6] = 0;
        frame[13] = 0x06 ^ 0x5F;
        assert_eq!(decode(&frame), None); // Command Center also rejects a zero level
        assert_eq!(decode(&[0x07, 0x00, 0x80]), None); // "no answer yet"
    }

    #[test]
    fn takes_the_critical_level_from_byte_11() {
        let mut frame = [0x07, 0xC0, 0x0A, 0x09, 0x00, 0x01, 0x05, 0x01, 0x01, 0x1C, 0x10, 0x63, 0x02, 0x00];
        frame[13] = 0;
        for i in 3..=12 {
            frame[13] ^= frame[i];
        }
        assert_eq!(decode(&frame), Some(State::Battery { percent: 99, charging: true }));
    }
}
