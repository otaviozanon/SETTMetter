//! Machenike L7 Arctic / L7 Mix (248A:FF4C receiver, 248A:FF4E cable).
//!
//! Feature report 0x51, command 0x06, 21 bytes. Reply byte 10 = link mode, byte 11 = percent,
//! byte 12 = charging. On the cable the USB connection is authoritative.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/machenike.cpp), verified on an L7 Arctic.

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::HidApi;
use std::time::{Duration, Instant};

const VID: u16 = 0x248A;
const CABLE: u16 = 0xFF4E;
const RECEIVER: u16 = 0xFF4C;
const REPORT_ID: u8 = 0x51;
const COMMAND: u8 = 0x06;
const LEN: usize = 21;
const KEY: &str = "machenike:l7";

#[derive(Default)]
pub struct Machenike {
    seen: bool,
}

impl Machenike {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        for &pid in &[CABLE, RECEIVER] {
            let cable = pid == CABLE;
            for info in api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == pid) {
                let product = info.product_string().unwrap_or("");
                if !product.to_ascii_lowercase().contains("l7") {
                    continue;
                }
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::feature_len(&desc, REPORT_ID).unwrap_or(0) < LEN {
                    continue;
                }
                if let Some(reply) = query(&dev, cable) {
                    if !reply.0 {
                        return if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() };
                    }
                    self.seen = true;
                    return vec![observation(State::Battery { percent: reply.1, charging: reply.2 })];
                }
            }
        }
        if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() }
    }
}

fn observation(state: State) -> Observation {
    Observation {
        id: KEY.into(),
        name: Some("Machenike L7 Arctic / L7 Mix".into()),
        kind: Some(Kind::Mouse),
        state,
    }
}

/// Returns (connected, percent, charging).
fn query(dev: &hidapi::HidDevice, cable: bool) -> Option<(bool, u8, bool)> {
    let mut req = [0u8; LEN];
    req[..2].copy_from_slice(&[REPORT_ID, COMMAND]);
    let mut buf = [0u8; LEN];
    buf[0] = REPORT_ID;
    let deadline = Instant::now() + Duration::from_millis(200);
    dev.send_feature_report(&req).ok()?;
    loop {
        std::thread::sleep(Duration::from_millis(10));
        let n = dev.get_feature_report(&mut buf).ok()?;
        if n == LEN
            && buf[0] == REPORT_ID
            && buf[1] == COMMAND
            && buf[4] == 0x8A
            && buf[5] == 0x24
            && buf[6] == 0x4E
            && buf[7] == 0xFF
            && (buf[10] & 7) == if cable { 0 } else { 1 }
        {
            let connected = cable || (buf[10] & 8) != 0;
            if connected && (buf[11] > 100 || buf[12] > 1) {
                return None;
            }
            return Some((connected, buf[11], connected && buf[12] == 1));
        }
        if Instant::now() >= deadline {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_status_frame() {
        let mut r = [0u8; LEN];
        r[0] = 0x51;
        r[1] = 0x06;
        r[4] = 0x8A;
        r[5] = 0x24;
        r[6] = 0x4E;
        r[7] = 0xFF;
        r[10] = 9;
        r[11] = 82;
        r[12] = 1;
        assert_eq!(r[10] & 8, 8);
        assert_eq!(r[11], 82);
    }
}
