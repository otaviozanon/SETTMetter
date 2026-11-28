//! Cooler Master wireless mice (VID 2516): MM712 (cable 0169) and MM731 (cable 0167), both
//! pairing with the "Cooler Master Mice Dongle" (01B7).
//!
//! Vendor interface 3 on usage page FF0A, no report id: 32-byte reports on the dongle and
//! 64-byte reports over the cable. Writing `00 46 81 00` asks for the status; the reply starts
//! `47 81`, `[2]` tells the models apart (50 = MM712, 70 = MM731), `[10]` is the level, `[11]`
//! the charging flag and `[12]` the link (0 = cable, 1 = dongle). The mouse also sends the same
//! frame on its own when the level changes.
//!
//! Protocol from syntactic-spice/coolermaster-usb-protocol (docs/PROTOCOL.md) and
//! riVFerd/MM731-Controller, via Wireless-Device-Battery-Monitor (drivers/coolermaster.cpp).
//! MM712 verified on hardware; the MM731 battery and model bytes are not. Only the status
//! request is sent.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::drain as flush;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const VID: u16 = 0x2516;
const DONGLE: u16 = 0x01B7;
const MM712_CABLE: u16 = 0x0169;
const MM731_CABLE: u16 = 0x0167;
const PAGE: u16 = 0xFF0A;
const WAIT: Duration = Duration::from_millis(500);
const MIN_LEN: usize = 13;

/// Sorted so the cable ids come before the shared dongle.
const PIDS: &[u16] = &[MM731_CABLE, MM712_CABLE, DONGLE];

#[derive(Default)]
pub struct CoolerMaster {
    /// Last name each receiver answered with, so a sleeping mouse keeps its model.
    seen: HashMap<String, &'static str>,
}

impl CoolerMaster {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for &pid in PIDS {
            let key = format!("coolermaster:{pid:04x}");
            let mut state = None;
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.interface_number() == 3 && d.usage_page() == PAGE
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                match query(&dev) {
                    Ok(Some((model, percent, charging))) => {
                        let name = model_name(pid, model);
                        diag::event(&key, format!("battery via report 0x47 (model 0x{model:02X})"));
                        self.seen.insert(key.clone(), name);
                        state = Some(observation(&key, name, battery(percent, charging)));
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => diag::event(&key, format!("i/o failed: {e}")),
                }
            }
            match state {
                Some(o) => out.push(o),
                None => {
                    if let Some(&name) = self.seen.get(&key) {
                        out.push(observation(&key, name, State::Unreachable));
                    }
                }
            }
        }
        out
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

fn battery(percent: u8, charging: bool) -> State {
    if (1..=100).contains(&percent) {
        State::Battery { percent, charging }
    } else {
        // Switched off or still waking: no level yet.
        State::Unreachable
    }
}

fn model_name(pid: u16, model: u8) -> &'static str {
    if pid == MM712_CABLE || (pid == DONGLE && model == 0x50) {
        "Cooler Master MM712"
    } else if pid == MM731_CABLE || (pid == DONGLE && model == 0x70) {
        "Cooler Master MM731"
    } else {
        "Cooler Master wireless mouse"
    }
}

fn query(dev: &HidDevice) -> hidapi::HidResult<Option<(u8, u8, bool)>> {
    flush(dev);
    dev.write(&[0x00, 0x46, 0x81, 0x00])?;

    let deadline = Instant::now() + WAIT;
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(r) = decode(&buf[..n]) {
                    return Ok(Some(r));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// `47 81 <model> … [10] level [11] charging …`.
fn decode(r: &[u8]) -> Option<(u8, u8, bool)> {
    if r.len() < MIN_LEN || r[0] != 0x47 || r[1] != 0x81 {
        return None;
    }
    Some((r[2], r[10], r[11] != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_status_reply() {
        let mut frame = [0u8; 32];
        frame[0] = 0x47;
        frame[1] = 0x81;
        frame[2] = 0x50; // MM712
        frame[10] = 90;
        frame[11] = 1;
        assert_eq!(decode(&frame), Some((0x50, 90, true)));
        assert_eq!(model_name(DONGLE, 0x50), "Cooler Master MM712");

        frame[2] = 0x70; // MM731
        frame[11] = 0;
        assert_eq!(decode(&frame), Some((0x70, 90, false)));
        assert_eq!(model_name(DONGLE, 0x70), "Cooler Master MM731");

        frame[0] = 0x46; // another report
        assert_eq!(decode(&frame), None);
        assert_eq!(decode(&frame[..12]), None);
        assert_eq!(model_name(DONGLE, 0x00), "Cooler Master wireless mouse");
        assert_eq!(model_name(MM731_CABLE, 0x00), "Cooler Master MM731");
    }

    #[test]
    fn out_of_range_level_is_not_a_reading() {
        assert_eq!(battery(0, false), State::Unreachable);
        assert_eq!(battery(101, false), State::Unreachable);
        assert_eq!(battery(1, false), State::Battery { percent: 1, charging: false });
    }
}
