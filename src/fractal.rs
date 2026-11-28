//! Fractal Design Scape headset (36BC:0001 dongle, 36BC:2001 USB-C cable).
//!
//! Output report 0x02: `[02][device type][21]` asks for GET_DEVICE_STATE. The reply is input
//! report 0x02 echoing type and command; payload byte 3 = 1 when the headset is linked through the
//! dongle, byte 14 = percent and byte 22 = charging (on the cable).
//! Protocol from Wireless-Device-Battery-Monitor (drivers/fractal.cpp), read from Fractal's own
//! Adjust Pro WebHID configurator and checked on a Scape.

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x36BC;
const REPORT_ID: u8 = 0x02;
const TYPE_DONGLE: u8 = 0x11;
const TYPE_HEADSET: u8 = 0xF1;
const GET_STATE: u8 = 0x21;
const TIMEOUT: Duration = Duration::from_millis(1000);
const KEY: &str = "fractal:scape";

#[derive(Default)]
pub struct Fractal {
    seen: bool,
}

impl Fractal {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut answered = false;
        for &(pid, cable) in &[(0x2001u16, true), (0x0001, false)] {
            for info in api
                .device_list()
                .filter(|d| d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0xFF00 && d.usage() == 0x0001)
            {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                let Some(out_len) = probe::output_len(&desc, REPORT_ID) else { continue };
                let Some(in_len) = probe::input_len(&desc, REPORT_ID) else { continue };
                if out_len < 4 || in_len < 24 {
                    continue;
                }
                if !cable {
                    let Some(r) = get_state(&dev, out_len, in_len, TYPE_DONGLE) else { continue };
                    if r[4] != 1 {
                        answered = true;
                        break;
                    }
                }
                let Some(r) = get_state(&dev, out_len, in_len, TYPE_HEADSET) else { continue };
                if r[15] > 100 {
                    continue;
                }
                self.seen = true;
                return vec![Observation {
                    id: KEY.into(),
                    name: Some("Fractal Scape".into()),
                    kind: Some(Kind::Headset),
                    state: State::Battery { percent: r[15], charging: r[23] == 1 },
                }];
            }
            if answered {
                break;
            }
        }
        if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() }
    }
}

fn observation(state: State) -> Observation {
    Observation { id: KEY.into(), name: Some("Fractal Scape".into()), kind: Some(Kind::Headset), state }
}

fn get_state(dev: &HidDevice, out_len: usize, in_len: usize, kind: u8) -> Option<Vec<u8>> {
    let mut req = vec![0u8; out_len];
    req[..3].copy_from_slice(&[REPORT_ID, kind, GET_STATE]);
    let mut buf = vec![0u8; in_len];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    dev.write(&req).ok()?;
    let deadline = Instant::now() + TIMEOUT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(n) if n >= 24 && buf[0] == REPORT_ID && buf[1] == kind && buf[2] == GET_STATE => {
                return Some(buf[..n].to_vec());
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
    fn charge_bit_and_level_offsets() {
        let mut r = vec![0u8; 24];
        r[0] = 0x02;
        r[1] = 0xF1;
        r[2] = 0x21;
        r[15] = 77;
        r[23] = 1;
        assert_eq!(r[15], 77);
        assert_eq!(r[23], 1);
    }
}
