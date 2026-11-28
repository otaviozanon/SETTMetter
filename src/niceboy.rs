//! Niceboy MW400 wireless mouse on its 248A:FA02 receiver.
//!
//! Vendor collection 0001:0000, report 0x05, 33-byte in/out: request `[05][24][FA][48]`, reply
//! `[05][xx][FA][48][mv lo][mv hi][charging]`. Millivolts = raw * 990 / 240, then the app's
//! 10%-step table. Protocol from Wireless-Device-Battery-Monitor (drivers/niceboy.cpp).

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::HidApi;
use std::time::{Duration, Instant};

const VID: u16 = 0x248A;
const PID: u16 = 0xFA02;
const REPORT_ID: u8 = 0x05;
const LEN: usize = 33;
const KEY: &str = "niceboy:fa02";

#[derive(Default)]
pub struct Niceboy {
    seen: bool,
}

impl Niceboy {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let Some(c) = crate::hidutil::open(api, VID, PID, |d, desc| {
            d.interface_number() == 0
                && d.usage_page() == 0x0001
                && d.usage() == 0x0000
                && probe::output_len(desc, REPORT_ID) == Some(LEN)
                && probe::input_len(desc, REPORT_ID) == Some(LEN)
        }) else {
            return if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() };
        };
        if let Some(r) = query(&c.dev) {
            if (r[4] | r[5] | r[6]) == 0 {
                return if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() };
            }
            self.seen = true;
            let mv = (u32::from(r[4]) | (u32::from(r[5]) << 8)) * 990 / 240;
            return vec![observation(State::Battery { percent: percent(mv as i32), charging: r[6] != 0 })];
        }
        if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() }
    }
}

fn observation(state: State) -> Observation {
    Observation { id: KEY.into(), name: Some("Niceboy MW400".into()), kind: Some(Kind::Mouse), state }
}

fn query(dev: &hidapi::HidDevice) -> Option<[u8; LEN]> {
    let mut req = [0u8; LEN];
    req[..4].copy_from_slice(&[REPORT_ID, 0x24, 0xFA, 0x48]);
    let mut buf = [0u8; LEN];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    if let Err(e) = dev.write(&req) {
        diag::event(KEY, format!("i/o failed: {e}"));
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(n) if n >= 7 && buf[0] == REPORT_ID && buf[2] == 0xFA && buf[3] == 0x48 => return Some(buf),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

fn percent(mv: i32) -> u8 {
    const MV: [i32; 9] = [4200, 4100, 4000, 3900, 3800, 3700, 3500, 3400, 3200];
    const PCT: [u8; 9] = [100, 90, 80, 70, 60, 50, 40, 30, 20];
    for (i, v) in MV.iter().enumerate() {
        if mv >= *v {
            return PCT[i];
        }
    }
    10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_reply_is_90_percent() {
        // 05 49 FA 48 EA 03 00 = raw 1002 = 4133 mV = 90%.
        let r: [u8; 7] = [0x05, 0x49, 0xFA, 0x48, 0xEA, 0x03, 0x00];
        let mv = (u32::from(r[4]) | (u32::from(r[5]) << 8)) * 990 / 240;
        assert_eq!(mv, 4133);
        assert_eq!(percent(mv as i32), 90);
    }

    #[test]
    fn table_edges() {
        assert_eq!(percent(4300), 100);
        assert_eq!(percent(3199), 10);
        assert_eq!(percent(3500), 40);
    }
}
