//! NZXT Lift Elite Wireless on its dongle (1E71:2101) or USB cable (1E71:2129), without
//! NZXT CAM.
//!
//! The vendor collection FFCA:0001 carries 64-byte unnumbered reports:
//!
//! ```text
//! request  4E 02 81 00 B0 00 …
//! reply    4E 02 97 …
//! ```
//!
//! `02` is the mouse, `81` reads a property, `B0` is telemetry. In the reply bytes 17-18 are
//! the level little-endian, bytes 8-9 the cell voltage big-endian and byte 22 bit 0 says the
//! charging cable is connected. The dongle first acknowledges with `4E E5`, which is skipped.
//! Only this read is sent.
//!
//! Protocol from HaloBattery (github.com/HeyOkay/HaloBattery, providers/nzxt.py), which
//! decoded it from USB captures of NZXT CAM 4.76.5. While NZXT CAM is open the dongle stops
//! answering; the last value is kept (grace logic skipped here).

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice, HidResult};
use std::time::Duration;

const VID: u16 = 0x1E71;
const RECEIVER: u16 = 0x2101;
const CABLE: u16 = 0x2129;
const KEY: &str = "nzxt:2101";
const READ_WAIT: Duration = Duration::from_millis(150);

#[derive(Default)]
pub struct Nzxt {
    /// Whether the mouse ever answered; until then a silent dongle isn't listed.
    seen: bool,
}

impl Nzxt {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut state = None;
        // One row for the mouse on both ids: the cable reading wins when both answer,
        // because that is the one charging.
        for pid in [CABLE, RECEIVER] {
            if state.is_some() {
                break;
            }
            for info in api
                .device_list()
                .filter(|d| d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0xFFCA && d.usage() == 0x0001)
            {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, 0).is_none_or(|n| n < 6) || probe::input_len(&desc, 0).is_none_or(|n| n < 27) {
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
        }

        let state = match state {
            Some(s) => {
                self.seen = true;
                s
            }
            None if self.seen => State::Unreachable,
            None => return Vec::new(),
        };
        vec![Observation { id: KEY.into(), name: Some("NZXT Lift Elite".into()), kind: Some(Kind::Mouse), state }]
    }
}

fn read(dev: &HidDevice) -> HidResult<Option<State>> {
    drain(dev)?;
    dev.write(&[0x00, 0x4E, 0x02, 0x81, 0x00, 0xB0])?;

    let mut buf = [0u8; 128];
    for _ in 0..10 {
        match dev.read_timeout(&mut buf, READ_WAIT.as_millis() as i32) {
            Ok(0) => continue,
            Ok(n) => {
                if let Some(s) = decode(&buf[..n]) {
                    return Ok(Some(s));
                }
            }
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

fn decode(r: &[u8]) -> Option<State> {
    if r.len() < 27 || r[0] != 0x4E {
        return None;
    }
    // E5 is the dongle's acknowledgement; only 02 97 is telemetry.
    if r[1] != 0x02 || r[2] != 0x97 {
        return None;
    }
    let level = u16::from(r[17]) | u16::from(r[18]) << 8;
    if level > 100 {
        return None; // not a percentage: refuse rather than show it
    }
    Some(State::Battery { percent: level as u8, charging: r[22] & 0x01 != 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn telemetry(level: u16, charging: u8) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[..3].copy_from_slice(&[0x4E, 0x02, 0x97]);
        r[8] = 0x0F; // cell mV, big-endian
        r[9] = 0x50;
        r[17] = level as u8;
        r[18] = (level >> 8) as u8;
        r[22] = charging;
        r
    }

    #[test]
    fn decodes_the_telemetry_reply() {
        assert_eq!(decode(&telemetry(87, 0)), Some(State::Battery { percent: 87, charging: false }));
        assert_eq!(decode(&telemetry(87, 1)), Some(State::Battery { percent: 87, charging: true }));
        assert_eq!(decode(&telemetry(100, 0)), Some(State::Battery { percent: 100, charging: false }));
    }

    #[test]
    fn skips_the_ack_and_implausible_levels() {
        let mut ack = vec![0u8; 64];
        ack[..2].copy_from_slice(&[0x4E, 0xE5]);
        assert_eq!(decode(&ack), None);
        assert_eq!(decode(&telemetry(101, 0)), None);
        assert_eq!(decode(&telemetry(87, 0)[..26]), None); // short
        assert_eq!(decode(&[0x4E, 0x02, 0x97]), None);
    }
}
