//! VGN OMNISOUND (ZHOUTING) headset on its 372E:10BB dongle.
//!
//! Control collection 000C:0001, report 0x55. Request `[55 65 01]`; the reply is an input report
//! `[55 65 percent]` (or unnumbered `[65 percent]`). No charging bit is known.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/vgn.cpp), verified on a ZHOUTING dongle.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::HidApi;
use std::time::{Duration, Instant};

const VID: u16 = 0x372E;
const PID: u16 = 0x10BB;
const REPORT_ID: u8 = 0x55;
const READ_BATTERY: u8 = 0x65;
const KEY: &str = "vgn:zhou";

#[derive(Default)]
pub struct Vgn {
    seen: bool,
}

impl Vgn {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == 0x000C && d.usage() == 0x0001
        }) {
            let Some(dev) = api.open_path(info.path()).ok() else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            let Some(out_len) = probe::output_len(&desc, REPORT_ID) else { continue };
            if out_len < 3 || probe::input_len(&desc, REPORT_ID).unwrap_or(0) < 3 {
                continue;
            }
            if let Some(percent) = query(&dev, out_len) {
                self.seen = true;
                let product = info.product_string().unwrap_or("");
                let name = if product.trim().is_empty() { "VGN OMNISOUND (ZHOUTING)" } else { product };
                return vec![Observation {
                    id: KEY.into(),
                    name: Some(name.into()),
                    kind: Some(Kind::Headset),
                    state: State::Battery { percent, charging: false },
                }];
            }
        }
        if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() }
    }
}

fn observation(state: State) -> Observation {
    Observation { id: KEY.into(), name: Some("VGN OMNISOUND (ZHOUTING)".into()), kind: Some(Kind::Headset), state }
}

fn query(dev: &hidapi::HidDevice, out_len: usize) -> Option<u8> {
    let mut req = vec![0u8; out_len];
    req[..3].copy_from_slice(&[REPORT_ID, READ_BATTERY, 0x01]);
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
    let deadline = Instant::now() + Duration::from_millis(500);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(n) => {
                if let Some(percent) = parse(&buf[..n]) {
                    return Some(percent);
                }
            }
            Err(_) => return None,
        }
    }
    None
}

fn parse(r: &[u8]) -> Option<u8> {
    let percent = if r.len() >= 3 && r[0] == REPORT_ID && r[1] == READ_BATTERY {
        r[2]
    } else if r.len() >= 2 && r[0] == READ_BATTERY {
        r[1]
    } else {
        return None;
    };
    (percent <= 100).then_some(percent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_and_unnumbered_replies() {
        assert_eq!(parse(&[0x55, 0x65, 64]), Some(64));
        assert_eq!(parse(&[0x65, 64]), Some(64));
        assert_eq!(parse(&[0x55, 0x65, 101]), None);
        assert_eq!(parse(&[0x55, 0x10]), None);
    }
}
