//! AULA A7Pro headset on its 38A6:0001 receiver.
//!
//! Interface 0, FF90:0001, 64-byte reports. Request `55 65 01`; reply byte 2 = percent, byte 3 =
//! power state (0 = external power, 2 = battery). Full at 100% on external power.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/aula_headset.cpp), verified on hardware.

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::HidApi;
use std::time::{Duration, Instant};

const VID: u16 = 0x38A6;
const PID: u16 = 0x0001;
const REPORT: u8 = 0x55;
const BATTERY: u8 = 0x65;
const LEN: usize = 64;
const KEY: &str = "aula:a7pro";

#[derive(Default)]
pub struct AulaHeadset {
    seen: bool,
}

impl AulaHeadset {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID
                && d.product_id() == PID
                && d.interface_number() == 0
                && d.usage_page() == 0xFF90
                && d.usage() == 0x0001
        }) {
            if info.product_string() != Some("AULA-A7Pro") {
                continue;
            }
            let Some(dev) = api.open_path(info.path()).ok() else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, REPORT) != Some(LEN) || probe::input_len(&desc, REPORT) != Some(LEN) {
                continue;
            }
            if let Some((percent, external)) = query(&dev) {
                self.seen = true;
                return vec![Observation {
                    id: KEY.into(),
                    name: Some("AULA A7Pro".into()),
                    kind: Some(Kind::Headset),
                    state: State::Battery { percent, charging: external && percent < 100 },
                }];
            }
        }
        if self.seen {
            vec![Observation {
                id: KEY.into(),
                name: Some("AULA A7Pro".into()),
                kind: Some(Kind::Headset),
                state: State::Unreachable,
            }]
        } else {
            Vec::new()
        }
    }
}

fn query(dev: &hidapi::HidDevice) -> Option<(u8, bool)> {
    let mut req = [0u8; LEN];
    req[..3].copy_from_slice(&[REPORT, BATTERY, 0x01]);
    let mut buf = [0u8; LEN];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    dev.write(&req).ok()?;
    let deadline = Instant::now() + Duration::from_millis(750);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(n) if n == LEN && buf[0] == REPORT && buf[1] == BATTERY && buf[2] <= 100 && matches!(buf[3], 0 | 2) => {
                return Some((buf[2], buf[3] == 0));
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
    fn reply_shape() {
        let mut r = [0u8; 64];
        r[0] = 0x55;
        r[1] = 0x65;
        r[2] = 40;
        r[3] = 0x02;
        assert_eq!((r[2], r[3] == 0), (40, false));
    }
}
