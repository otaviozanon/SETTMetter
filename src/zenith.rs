//! Zenith Pro mouse (260D:1114 on its receiver, 260D:1143 on the USB cable).
//!
//! The FF02:0002 collection answers a 17-byte output report 0x08 with command 0x04 and the
//! 0x55 checksum: byte 6 is the level, byte 7 charging, bytes 8-9 the cell voltage in mV.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/zenith.cpp), observed on hardware.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::checksum55 as checksum;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const VID: u16 = 0x260D;
const PIDS: &[u16] = &[0x1143, 0x1114];
const REPORT_ID: u8 = 0x08;
const LEN: usize = 17;
const TIMEOUT: Duration = Duration::from_millis(1000);

#[derive(Default)]
pub struct Zenith {
    seen: HashSet<u16>,
}

impl Zenith {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for &pid in PIDS {
            if out.iter().any(|o: &Observation| o.name.as_deref() == Some("Zenith Pro")) {
                continue;
            }
            let mut answered = false;
            for info in api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == pid) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                let Some(out_len) = probe::output_len(&desc, REPORT_ID) else { continue };
                if out_len < LEN || probe::input_len(&desc, REPORT_ID).unwrap_or(0) < LEN {
                    continue;
                }
                if let Some(reply) = query(&dev, out_len) {
                    answered = true;
                    self.seen.insert(pid);
                    out.push(Observation {
                        id: format!("zenith:{pid:04x}"),
                        name: Some("Zenith Pro".into()),
                        kind: Some(Kind::Mouse),
                        state: State::Battery { percent: reply[6], charging: reply[7] != 0 },
                    });
                    break;
                }
            }
            if !answered && self.seen.contains(&pid) {
                out.push(Observation {
                    id: format!("zenith:{pid:04x}"),
                    name: Some("Zenith Pro".into()),
                    kind: Some(Kind::Mouse),
                    state: State::Unreachable,
                });
            }
        }
        out
    }
}

fn query(dev: &HidDevice, out_len: usize) -> Option<[u8; LEN]> {
    let mut req = vec![0u8; out_len.max(LEN)];
    req[0] = REPORT_ID;
    req[1] = 0x04;
    req[LEN - 1] = checksum(&req[..LEN - 1]);
    let mut buf = [0u8; 64];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    if let Err(e) = dev.write(&req) {
        diag::event("zenith", format!("i/o failed: {e}"));
        return None;
    }
    let deadline = Instant::now() + TIMEOUT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(n) if n == LEN && buf[0] == REPORT_ID && buf[1] == 0x04 && buf[6] <= 100 => {
                if buf.iter().fold(0u8, |s, b| s.wrapping_add(*b)) != 0x55 {
                    continue;
                }
                let mut r = [0u8; LEN];
                r.copy_from_slice(&buf);
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
    fn parsed_frame_is_a_valid_0x55_page() {
        let mut reply = [0u8; LEN];
        reply[0] = REPORT_ID;
        reply[1] = 0x04;
        reply[6] = 64;
        reply[7] = 1;
        reply[LEN - 1] = checksum(&reply[..LEN - 1]);
        assert_eq!(reply.iter().fold(0u8, |s, b| s.wrapping_add(*b)), 0x55);
        assert_eq!(reply[6], 64);
    }
}
