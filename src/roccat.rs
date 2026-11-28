//! ROCCAT Syn Max Air headset (1E7D:3A3F) on its 2.4 GHz receiver, a Turtle Beach
//! "Stellar Wireless" platform sold under the ROCCAT brand.
//!
//! The receiver's vendor collection FF00:0001 relays one request to the headset, the same
//! read-only status request Swarm sends every 6 s:
//!
//! ```text
//! request  8A 07 06 01
//! reply    8A 83 06 02 LL 00 V1 V1 V2 V2 CC
//! ```
//!
//! `LL` is the coarse level 0..5 (6 while charging), `V1 V1` the cell voltage in mV
//! little-endian with 20 mV resolution (`50 0F` = 3920 mV), `V2 V2` a filtered copy and
//! `CC` 01 while charging. Byte 0 is a tag the headset echoes, which tells the reply from the
//! unsolicited `E0..E6` event reports (mic arm, wheels) on the same collection. No reply
//! within 1.5 s means the headset is off or asleep while the receiver stays enumerated.
//!
//! Discharging percent comes from a voltage curve measured on one unit over a full discharge
//! under typical load (audio and RGB on). While charging the terminal voltage says little
//! about the state of charge, so the device's own coarse level is shown then.
//!
//! Protocol captured with USBPcap from ROCCAT Swarm (firmware 1.08) and first implemented in
//! Wireless-Device-Battery-Monitor (drivers/roccat.cpp). Only the status request is sent.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice, HidResult};
use std::time::{Duration, Instant};

const VID: u16 = 0x1E7D;
const PID: u16 = 0x3A3F;
const KEY: &str = "roccat:3a3f";
const REPLY_TIMEOUT: Duration = Duration::from_millis(1500);

/// Discharging under load: voltage (mV) -> percent, measured on hardware.
const DISCHARGE: &[(i32, i32)] = &[
    (4080, 100),
    (4040, 97),
    (4000, 92),
    (3960, 86),
    (3920, 78),
    (3900, 74),
    (3880, 70),
    (3840, 66),
    (3820, 63),
    (3800, 60),
    (3780, 53),
    (3760, 45),
    (3740, 34),
    (3720, 26),
    (3700, 20),
    (3680, 15),
    (3660, 11),
    (3640, 7),
    (3600, 3),
    (3500, 0),
];

#[derive(Default)]
pub struct Roccat {
    /// Whether the headset ever answered; until then a silent receiver isn't listed.
    seen: bool,
}

impl Roccat {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let infos: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == 0xFF00 && d.usage() == 0x0001)
            .collect();
        if infos.is_empty() {
            return Vec::new();
        }

        let mut state = None;
        for info in infos {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, 0).is_none_or(|n| n < 63) || probe::input_len(&desc, 0).is_none_or(|n| n < 63) {
                continue;
            }
            match read(&dev) {
                Ok(Some(s)) => {
                    state = Some(s);
                    break;
                }
                Ok(None) => {}
                Err(e) => diag::event(KEY, format!("i/o failed: {e}")),
            }
        }

        let state = match state {
            Some(s) => {
                self.seen = true;
                s
            }
            None if self.seen => State::Unreachable,
            None => return Vec::new(),
        };
        vec![Observation { id: KEY.into(), name: Some("ROCCAT Syn Max Air".into()), kind: Some(Kind::Headset), state }]
    }
}

fn read(dev: &HidDevice) -> HidResult<Option<State>> {
    drain(dev)?;
    dev.write(&[0x00, 0x8A, 0x07, 0x06, 0x01])?;

    let mut buf = [0u8; 128];
    let deadline = Instant::now() + REPLY_TIMEOUT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis().max(1) as i32) {
            Ok(0) => break,
            Ok(n) if buf[0] == 0x8A => return Ok(decode(&buf[..n])),
            Ok(_) => {} // E0..E6 event reports
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

fn drain(dev: &HidDevice) -> HidResult<()> {
    let mut buf = [0u8; 128];
    while dev.read_timeout(&mut buf, 0)? > 0 {}
    Ok(())
}

/// `8A 83 06 02 LL 00 V1 V1 V2 V2 CC`; false when it is not that frame.
fn decode(r: &[u8]) -> Option<State> {
    if r.len() < 10 || r[0] != 0x8A || r[1] != 0x83 || r[2] != 0x06 {
        return None;
    }
    let charging = r.len() > 10 && r[10] == 0x01;
    let level = i32::from(r[4]).min(5);
    let mv = i32::from(r[6]) | i32::from(r[7]) << 8;
    let percent = if charging {
        // Terminal voltage says little while charging: the device's own coarse level.
        (level * 20) as u8
    } else {
        percent_from_millivolts(mv)
    };
    Some(State::Battery { percent, charging })
}

fn percent_from_millivolts(mv: i32) -> u8 {
    if mv >= DISCHARGE[0].0 {
        return DISCHARGE[0].1 as u8;
    }
    let last = DISCHARGE[DISCHARGE.len() - 1];
    if mv <= last.0 {
        return last.1 as u8;
    }
    for pair in DISCHARGE.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if mv <= a.0 && mv >= b.0 {
            return (b.1 + (mv - b.0) * (a.1 - b.1) / (a.0 - b.0)) as u8;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(level: u8, mv: i32, charging: u8) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[..6].copy_from_slice(&[0x8A, 0x83, 0x06, 0x02, level, 0x00]);
        r[6] = mv as u8;
        r[7] = (mv >> 8) as u8;
        r[8] = mv as u8;
        r[9] = (mv >> 8) as u8;
        r[10] = charging;
        r
    }

    #[test]
    fn decodes_the_captured_layout() {
        // 3920 mV on the measured curve is 78%.
        assert_eq!(decode(&reply(4, 3920, 0)), Some(State::Battery { percent: 78, charging: false }));
        assert_eq!(decode(&reply(5, 3660, 1)), Some(State::Battery { percent: 100, charging: true }));
        // While charging the coarse level is the level: 2/5.
        assert_eq!(decode(&reply(2, 3900, 1)), Some(State::Battery { percent: 40, charging: true }));
    }

    #[test]
    fn rejects_events_and_short_frames() {
        assert_eq!(decode(&[0xE0, 0x01, 0x00]), None);
        assert_eq!(decode(&reply(4, 3920, 0)[..9]), None);
        let mut wrong = reply(4, 3920, 0);
        wrong[1] = 0x84;
        assert_eq!(decode(&wrong), None);
    }

    #[test]
    fn voltage_curve_endpoints() {
        assert_eq!(percent_from_millivolts(4200), 100);
        assert_eq!(percent_from_millivolts(4080), 100);
        assert_eq!(percent_from_millivolts(3700), 20);
        assert_eq!(percent_from_millivolts(3500), 0);
        assert_eq!(percent_from_millivolts(3000), 0);
        // Between 3780 (53%) and 3760 (45%).
        assert_eq!(percent_from_millivolts(3770), 49);
    }
}
