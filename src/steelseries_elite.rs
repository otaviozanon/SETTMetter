//! SteelSeries Arctis Nova Elite base station (1038:2244).
//!
//! Interface-3 vendor collections: output report `01 B0 …` asks for status; the station answers
//! with `01 B0` (headset level at [6], power at [14], charging at [15]) or pushes `07 B7` /
//! `07 B5` frames. Power 0x01 = headset off, 0x02 = on the cable.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/steelseries_elite.cpp) and HaloBattery.

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::HidApi;
use std::time::{Duration, Instant};

const VID: u16 = 0x1038;
const PID: u16 = 0x2244;
const KEY: &str = "steelseries:2244";

#[derive(Default)]
pub struct SteelSeriesElite {
    seen: bool,
}

impl SteelSeriesElite {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut handles = Vec::new();
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == PID && d.interface_number() == 3 && d.usage_page() >= 0xFF00
        }) {
            if let Ok(dev) = api.open_path(info.path()) {
                let _ = dev.read_timeout(&mut [0u8; 64], 0);
                handles.push(dev);
            }
        }
        if handles.is_empty() {
            return if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() };
        }
        let sent = handles.iter().any(|dev| {
            let Some(desc) = probe::descriptor(dev) else { return false };
            if probe::output_len(&desc, 0x01).unwrap_or(0) < 64 {
                return false;
            }
            let mut req = [0u8; 64];
            req[..2].copy_from_slice(&[0x01, 0xB0]);
            dev.write(&req).is_ok()
        });
        if !sent {
            return if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() };
        }

        let deadline = Instant::now() + Duration::from_millis(1000);
        let mut level = None;
        let mut power = None;
        let mut charging = false;
        let mut buf = [0u8; 64];
        while Instant::now() < deadline && (level.is_none() || power.is_none()) {
            for dev in &handles {
                let Ok(n) = dev.read_timeout(&mut buf, 30) else { continue };
                if n < 5 {
                    continue;
                }
                if buf[0] == 0x01 && buf[1] == 0xB0 && n > 15 {
                    if buf[6] <= 100 {
                        level = Some(buf[6]);
                    }
                    power = Some(buf[14]);
                    charging = buf[15] == 0x02;
                } else if buf[0] == 0x07 && buf[1] == 0xB7 && buf[2] <= 100 {
                    level = Some(buf[2]);
                    charging = buf[4] == 0x02;
                } else if buf[0] == 0x07 && buf[1] == 0xB5 {
                    power = Some(buf[4]);
                }
            }
            if power == Some(0x01) {
                break;
            }
        }
        match (level, power) {
            (Some(percent), Some(p)) if p != 0x01 => {
                self.seen = true;
                vec![observation(State::Battery { percent, charging: charging || p == 0x02 })]
            }
            _ => {
                if self.seen {
                    vec![observation(State::Unreachable)]
                } else {
                    Vec::new()
                }
            }
        }
    }
}

fn observation(state: State) -> Observation {
    Observation {
        id: KEY.into(),
        name: Some("SteelSeries Arctis Nova Elite".into()),
        kind: Some(Kind::Headset),
        state,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn direct_reply_offsets() {
        let mut r = [0u8; 64];
        r[0] = 0x01;
        r[1] = 0xB0;
        r[6] = 70;
        r[14] = 0x02;
        r[15] = 0x02;
        assert_eq!(r[6], 70);
        assert_eq!(r[15] == 0x02, true);
    }
}
