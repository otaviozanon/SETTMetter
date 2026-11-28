//! Zebronics / Compx 3554:FC0A wireless mice.
//!
//! The FF02:0002 collection carries a 7-byte feature report 0x06; reading it returns
//! `06 00 02 00 <counter> <percent> …`. Read-only, no charging flag.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/zebronics.cpp), captured on hardware.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::HidApi;

const VID: u16 = 0x3554;
const PID: u16 = 0xFC0A;
const REPORT_ID: u8 = 0x06;

#[derive(Default)]
pub struct Zebronics {
    seen: bool,
}

impl Zebronics {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let Some(c) = crate::hidutil::open(api, VID, PID, |d, desc| {
            d.usage_page() == 0xFF02 && d.usage() == 0x0002 && probe::feature_len(desc, REPORT_ID).unwrap_or(0) >= 8
        }) else {
            return if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() };
        };
        let mut buf = [0u8; 8];
        buf[0] = REPORT_ID;
        match c.dev.get_feature_report(&mut buf) {
            Ok(n) if n >= 8 && buf[1] == 0x00 && buf[2] == 0x02 && buf[5] <= 100 => {
                self.seen = true;
                return vec![observation(State::Battery { percent: buf[5], charging: false })];
            }
            Ok(_) => {}
            Err(e) => diag::event("zebronics", format!("i/o failed: {e}")),
        }
        if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() }
    }
}

fn observation(state: State) -> Observation {
    Observation { id: "zebronics:3554:fc0a".into(), name: Some("Zebronics wireless mouse".into()), kind: Some(Kind::Mouse), state }
}

#[cfg(test)]
mod tests {
    #[test]
    fn captured_reply_shape() {
        let r = [0x06, 0x00, 0x02, 0x00, 0xDB, 0x64, 0x14, 0x80];
        assert_eq!(r[5], 100);
        assert_eq!(r[2], 0x02);
    }
}
