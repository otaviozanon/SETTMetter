//! Corsair Compx mice: Sabre V2 Pro Ultralight (1B1C:2B2A) and Sabre V2 Pro Wireless MG
//! (1B1C:2B34).
//!
//! FF02:0002, 17-byte output report 0x08 with the 0x55 checksum, commands 0x03 (link) and 0x04
//! (battery). Byte 6 = percent, byte 7 = charging.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/corsair.cpp), from HaloBattery captures.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::checksum55 as checksum;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const VID: u16 = 0x1B1C;
const MODELS: &[(u16, &str)] = &[
    (0x2B2A, "Corsair Sabre V2 Pro Ultralight"),
    (0x2B34, "Corsair Sabre v2 PRO WIRELESS MG"),
];
const REPORT_ID: u8 = 0x08;
const LEN: usize = 17;
const TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct CorsairCompx {
    seen: HashSet<u16>,
}

impl CorsairCompx {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for &(pid, name) in MODELS {
            let mut answered = false;
            let reading = crate::hidutil::open(api, VID, pid, |d, desc| {
                d.usage_page() == 0xFF02
                    && d.usage() == 0x0002
                    && probe::output_len(desc, REPORT_ID).is_some_and(|n| n >= LEN)
                    && probe::input_len(desc, REPORT_ID).unwrap_or(0) >= LEN
            })
            .and_then(|c| {
                let out_len = probe::output_len(&c.desc, REPORT_ID)?;
                query(&c.dev, out_len)
            });
            if let Some((percent, charging)) = reading {
                answered = true;
                self.seen.insert(pid);
                out.push(Observation {
                    id: format!("corsair:{pid:04x}"),
                    name: Some(name.into()),
                    kind: Some(Kind::Mouse),
                    state: State::Battery { percent, charging },
                });
            }
            if !answered && self.seen.contains(&pid) {
                out.push(Observation {
                    id: format!("corsair:{pid:04x}"),
                    name: Some(name.into()),
                    kind: Some(Kind::Mouse),
                    state: State::Unreachable,
                });
            }
        }
        out
    }
}

fn query(dev: &HidDevice, out_len: usize) -> Option<(u8, bool)> {
    let online = send(dev, out_len, 0x03);
    if online.is_some_and(|r| r[6] == 0) {
        return None;
    }
    let r = send(dev, out_len, 0x04).or(online)?;
    if r[6] > 100 || (r[6] == 0 && r[7] == 0) {
        return None;
    }
    Some((r[6], r[7] != 0))
}

fn send(dev: &HidDevice, out_len: usize, cmd: u8) -> Option<[u8; LEN]> {
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
        diag::event("corsair-compx", format!("i/o failed: {e}"));
        return None;
    }
    let deadline = Instant::now() + TIMEOUT;
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
            Err(_) => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_and_offsets() {
        let mut req = [0u8; LEN];
        req[0] = REPORT_ID;
        req[1] = 0x04;
        assert_eq!(checksum(&req[..LEN - 1]), 0x49);
        let mut r = [0u8; LEN];
        r[0] = REPORT_ID;
        r[1] = 0x04;
        r[6] = 76;
        r[7] = 1;
        assert_eq!((r[6], r[7] != 0), (76, true));
    }
}
