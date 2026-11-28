//! Akko V9 Dash Pro (38EE:000C) and Dash V9 Ultra (38EE:000F) on their 2.4 GHz receivers.
//!
//! Same 64-byte Evision-style channel as CHERRY/ATK on FF1C:0092: output report 0x04,
//! type 0x1A, payload `03 00 00 55`, checksum LE at bytes 1-2 = sum of bytes 3..63.
//! Reply: byte 3 = 0x1A, byte 8 = percent, byte 9 = charging.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/akko.cpp), verified with both receivers.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const VID: u16 = 0x38EE;
const LEN: usize = 64;
const REPORT_ID: u8 = 0x04;
const STATUS: u8 = 0x1A;
const MODELS: &[(u16, &str)] = &[(0x000C, "Akko V9 Dash Pro"), (0x000F, "Akko Dash V9 Ultra")];
const TIMEOUT: Duration = Duration::from_millis(400);

#[derive(Default)]
pub struct Akko {
    seen: HashSet<u16>,
}

impl Akko {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for &(pid, name) in MODELS {
            let before = out.len();
            for info in api
                .device_list()
                .filter(|d| d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0xFF1C && d.usage() == 0x0092)
            {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, REPORT_ID).unwrap_or(0) < LEN {
                    continue;
                }
                if let Some(reply) = query(&dev) {
                    self.seen.insert(pid);
                    out.push(Observation {
                        id: format!("akko:38ee:{pid:04x}"),
                        name: Some(name.into()),
                        kind: Some(Kind::Mouse),
                        state: State::Battery { percent: reply[8], charging: reply[9] != 0 },
                    });
                    break;
                }
            }
            if out.len() == before && self.seen.contains(&pid) {
                out.push(Observation {
                    id: format!("akko:38ee:{pid:04x}"),
                    name: Some(name.into()),
                    kind: Some(Kind::Mouse),
                    state: State::Unreachable,
                });
            }
        }
        out
    }
}

fn query(dev: &HidDevice) -> Option<[u8; LEN]> {
    let mut req = [0u8; LEN];
    req[0] = REPORT_ID;
    req[3] = STATUS;
    req[4] = 0x03;
    req[7] = 0x55;
    let sum: u16 = req[3..].iter().map(|b| u16::from(*b)).sum();
    req[1] = sum as u8;
    req[2] = (sum >> 8) as u8;
    let mut buf = [0u8; LEN];
    for _ in 0..2 {
        if let Err(e) = dev.write(&req) {
            diag::event("akko", format!("i/o failed: {e}"));
            return None;
        }
        let deadline = Instant::now() + TIMEOUT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match dev.read_timeout(&mut buf, left.as_millis() as i32) {
                Ok(n) if n >= 10 && buf[0] == REPORT_ID && buf[3] == STATUS && buf[8] <= 100 => {
                    return Some(buf);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_has_evision_checksum() {
        let mut req = [0u8; LEN];
        req[0] = REPORT_ID;
        req[3] = STATUS;
        req[4] = 0x03;
        req[7] = 0x55;
        let sum: u16 = req[3..].iter().map(|b| u16::from(*b)).sum();
        assert_eq!(sum, 0x1A + 0x03 + 0x55);
    }
}
