//! Chimera V2 wireless mouse (3554:F512), a Compx 17-byte variant that differs from the Kysona
//! protocol in the status byte positions.
//!
//! Request: output report 0x08 with the checksum at the end (`0x55` minus the sum of the first 16).
//! Reply: input report 0x08 with the same checksum. Command 0x03 answers the link state in byte 5;
//! command 0x04 answers byte 5 = 0x02 when connected, byte 6 = percent and byte 7 = charging.
//!
//! Protocol from Wireless-Device-Battery-Monitor (drivers/chimera.cpp), discovered by probing the
//! device. Only the two read queries are sent.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::checksum55 as checksum;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x3554;
const PID: u16 = 0xF512;
const REPORT_ID: u8 = 0x08;
const LEN: usize = 17;
const QUERY_TIMEOUT: Duration = Duration::from_millis(800);
const KEY: &str = "chimera:3554:f512";

#[derive(Default)]
pub struct Chimera {
    seen: bool,
}

impl Chimera {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut candidates = crate::hidutil::open_all(api, VID, PID, |d, desc| {
            d.usage_page() >= 0xFF00
                && probe::output_len(desc, REPORT_ID).is_some_and(|n| n >= LEN)
                && probe::input_len(desc, REPORT_ID).unwrap_or(0) >= LEN
        });
        candidates.sort_by_key(|c| !(c.page == 0xFF02 && c.usage == 0x0002));
        for c in candidates {
            let Some(out_len) = probe::output_len(&c.desc, REPORT_ID) else { continue };
            let Some(online) = query(&c.dev, out_len, 0x03) else { continue };
            if online[5] == 0 {
                return if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() };
            }
            let Some(reply) = query(&c.dev, out_len, 0x04) else {
                return if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() };
            };
            if reply[5] != 0x02 || reply[6] == 0 || reply[6] > 100 {
                return if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() };
            }
            self.seen = true;
            diag::event(KEY, format!("battery via report 0x08 (level {}, charging {})", reply[6], reply[7] != 0));
            return vec![observation(State::Battery { percent: reply[6], charging: reply[7] != 0 })];
        }
        // The mouse has been seen before but the receiver is silent right now: keep it as asleep.
        if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() }
    }
}

fn observation(state: State) -> Observation {
    Observation { id: KEY.into(), name: Some("Chimera V2".into()), kind: Some(Kind::Mouse), state }
}

fn query(dev: &HidDevice, out_len: usize, cmd: u8) -> Option<[u8; LEN]> {
    let mut req = vec![0u8; out_len.max(LEN)];
    req[0] = REPORT_ID;
    req[1] = cmd;
    req[LEN - 1] = checksum(&req[..LEN - 1]);
    let mut buf = [0u8; 64];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    if let Err(e) = dev.write(&req) {
        diag::event(KEY, format!("i/o failed: {e}"));
        return None;
    }
    let deadline = Instant::now() + QUERY_TIMEOUT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(n) if n >= LEN && buf[0] == REPORT_ID && buf[1] == cmd => {
                if checksum(&buf[..LEN - 1]) != buf[LEN - 1] {
                    continue;
                }
                let mut r = [0u8; LEN];
                r.copy_from_slice(&buf[..LEN]);
                return Some(r);
            }
            Ok(_) => {}
            Err(e) => {
                diag::event(KEY, format!("i/o failed: {e}"));
                return None;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_and_parse_fields() {
        let mut req = [0u8; LEN];
        req[0] = REPORT_ID;
        req[1] = 0x04;
        req[LEN - 1] = checksum(&req[..LEN - 1]);
        assert_eq!(req[LEN - 1], 0x49);
        let mut reply = [0u8; LEN];
        reply[0] = REPORT_ID;
        reply[1] = 0x04;
        reply[5] = 0x02;
        reply[6] = 73;
        reply[7] = 1;
        assert_eq!(reply[5], 0x02);
        assert_eq!(reply[6], 73);
        assert_eq!(reply[7], 1);
    }
}
