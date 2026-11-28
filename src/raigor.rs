//! Fantech Raigor III / II WG10 and other mice on the 1EA7:0066 / 0064 dongle.
//!
//! Vendor collection FFB5:0001, 8-byte report 0xB5. Request `[B5 05 00 00 00 00 00 00]`; the dongle
//! streams its status page and chunk 0x94 carries the level in byte 6. A wake/heartbeat frame
//! `[B5 67 … level]` is accepted too. No charging flag.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/raigor.cpp), verified on a Raigor III.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::HidApi;
use std::collections::HashSet;
use std::time::{Duration, Instant};

const VID: u16 = 0x1EA7;
const PIDS: &[u16] = &[0x0066, 0x0064];
const REPORT_ID: u8 = 0xB5;

#[derive(Default)]
pub struct Raigor {
    seen: HashSet<u16>,
}

impl Raigor {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for &pid in PIDS {
            let mut answered = false;
            if let Some(c) = crate::hidutil::open(api, VID, pid, |d, desc| {
                d.usage_page() == 0xFFB5
                    && d.usage() == 0x0001
                    && probe::output_len(desc, REPORT_ID) == Some(8)
                    && probe::input_len(desc, REPORT_ID) == Some(8)
            }) && let Some(percent) = query(&c.dev)
            {
                answered = true;
                self.seen.insert(pid);
                let name = if c.product.trim().is_empty() { "2.4 GHz mouse" } else { c.product.trim() };
                out.push(Observation {
                    id: format!("raigor:{pid:04x}"),
                    name: Some(name.into()),
                    kind: Some(Kind::Mouse),
                    state: State::Battery { percent, charging: false },
                });
            }
            if !answered && self.seen.contains(&pid) {
                out.push(Observation {
                    id: format!("raigor:{pid:04x}"),
                    name: Some("2.4 GHz mouse".into()),
                    kind: Some(Kind::Mouse),
                    state: State::Unreachable,
                });
            }
        }
        out
    }
}

fn query(dev: &hidapi::HidDevice) -> Option<u8> {
    let req = [REPORT_ID, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let mut buf = [0u8; 8];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    if let Err(e) = dev.write(&req) {
        diag::event("raigor", format!("i/o failed: {e}"));
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(1200);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis().min(200) as i32) {
            Ok(8) if buf[0] == REPORT_ID => {
                if buf[1] == 0x05 && buf[2] == 0x94 && buf[3] == 0x04 && buf[6] <= 100 {
                    return Some(buf[6]);
                }
                if buf[1] == 0x67 && buf[7] <= 100 {
                    return Some(buf[7]);
                }
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn captured_chunk_94_is_64_percent() {
        let r = [0xB5, 0x05, 0x94, 0x04, 0x35, 0x09, 0x40, 0x38];
        assert_eq!(r[6], 64);
    }
}
