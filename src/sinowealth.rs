//! Sinowealth receivers sharing 258A:002F (Redragon M690 Pro / M602-KS and others).
//!
//! Feature report 0x05, 8 bytes, commands 0x80 (link) and 0x90 (battery). Battery reply:
//! byte 2 = 0x11, byte 3 = percent. Read-only.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/sinowealth_receiver.cpp), OpenMouse docs.

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::HidApi;
use std::time::Duration;

const VID: u16 = 0x258A;
const PID: u16 = 0x002F;
const REPORT_ID: u8 = 0x05;
const LEN: usize = 8;
const KEY: &str = "sinowealth:258a:002f";

#[derive(Default)]
pub struct Sinowealth {
    seen: bool,
}

impl Sinowealth {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let Some(c) = crate::hidutil::open(api, VID, PID, |d, desc| {
            d.interface_number() == 1
                && d.usage_page() == 0xFF00
                && d.usage() == 0x0001
                && probe::feature_len(desc, REPORT_ID) == Some(LEN)
        }) else {
            return if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() };
        };
        if let Some(link) = query(&c.dev, 0x80)
            && link[2] == 1
            && let Some(battery) = query(&c.dev, 0x90)
            && battery[2] == 0x11
            && battery[3] <= 100
        {
            self.seen = true;
            return vec![observation(State::Battery { percent: battery[3], charging: false })];
        }
        if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() }
    }
}

fn observation(state: State) -> Observation {
    Observation { id: KEY.into(), name: Some("Sinowealth wireless mouse".into()), kind: Some(Kind::Mouse), state }
}

fn query(dev: &hidapi::HidDevice, command: u8) -> Option<[u8; LEN]> {
    let mut req = [0u8; LEN];
    req[..2].copy_from_slice(&[REPORT_ID, command]);
    dev.send_feature_report(&req).ok()?;
    std::thread::sleep(Duration::from_millis(100));
    let mut reply = [0u8; LEN];
    reply[0] = REPORT_ID;
    let n = dev.get_feature_report(&mut reply).ok()?;
    (n == LEN && reply[0] == REPORT_ID && reply[1] == command).then_some(reply)
}

#[cfg(test)]
mod tests {
    #[test]
    fn battery_reply_shape() {
        let r = [0x05, 0x90, 0x11, 87, 0, 0, 0, 0];
        assert_eq!((r[2], r[3]), (0x11, 87));
    }
}
