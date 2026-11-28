//! Wobkey Rainy75 V2 keyboard, on its 2.4 GHz receiver (36B0:3071) or USB cable (36B0:3075).
//!
//! Battery telemetry is pushed passively on the FF60:0061 raw HID collection as unnumbered
//! 256-byte reports: `BE BB 06 <percent> <charge> ED` every ~5 s once the 0x93 "enable" frame
//! `BE 93 05 01 ED` has been sent. No further query is needed, so the only write is the enable.
//!
//! Handles stay open between scans so reports queued in the meantime are read, and the cable
//! wins when both are connected. Protocol captured from WOBHub, Wobkey's WebHID configurator
//! (Wireless-Device-Battery-Monitor, drivers/wobkey.cpp).

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::ffi::CString;
use std::time::{Duration, Instant};

const VID: u16 = 0x36B0;
const RECEIVER: u16 = 0x3071;
const CABLE: u16 = 0x3075;
const PAGE: u16 = 0xFF60;
const USAGE: u16 = 0x0061;
const ENABLE: [u8; 5] = [0xBE, 0x93, 0x05, 0x01, 0xED];
const BUDGET: Duration = Duration::from_secs(4);
const KEY: &str = "wobkey:36b0:3071";

#[derive(Default)]
pub struct Wobkey {
    open: HashMap<CString, HidDevice>,
}

impl Wobkey {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let present: Vec<CString> = api
            .device_list()
            .filter(|d| {
                d.vendor_id() == VID
                    && matches!(d.product_id(), RECEIVER | CABLE)
                    && d.usage_page() == PAGE
                    && d.usage() == USAGE
            })
            .map(|d| d.path().to_owned())
            .collect();
        self.open.retain(|path, _| present.contains(path));

        let mut out = Vec::new();
        let mut unreachable = false;
        // Cable first: it powers the keyboard and keeps it awake.
        for pid in [CABLE, RECEIVER] {
            let Some(info) = api.device_list().find(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == PAGE && d.usage() == USAGE
            }) else {
                continue;
            };
            let path = info.path().to_owned();
            if !self.open.contains_key(&path) {
                let Ok(dev) = api.open_path(&path) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                let Some(payload) = probe::output_len(&desc, 0) else { continue };
                if payload < 255 || probe::input_len(&desc, 0).unwrap_or(0) < 255 {
                    continue;
                }
                let mut req = vec![0u8; payload + 1]; // report id 0
                req[1..6].copy_from_slice(&ENABLE);
                if let Err(e) = dev.write(&req) {
                    diag::event(KEY, format!("enable failed: {e}"));
                    continue;
                }
                self.open.insert(path.clone(), dev);
            }
            let dev = self.open.get(&path).unwrap();
            match read_battery(dev, Instant::now() + BUDGET) {
                Ok(Some((percent, charging))) => {
                    diag::event(KEY, format!("battery via 0xBB telemetry ({percent}%)"));
                    out.push(Observation {
                        id: KEY.into(),
                        name: Some("Rainy75 V2".into()),
                        kind: Some(Kind::Keyboard),
                        state: State::Battery { percent, charging },
                    });
                    return out;
                }
                Ok(None) => unreachable = true,
                Err(e) => {
                    // Unplugged or the handle went bad: reopen on the next scan.
                    diag::event(KEY, format!("i/o failed: {e}"));
                    self.open.remove(&path);
                }
            }
        }
        if unreachable {
            diag::event(KEY, "no 0xBB telemetry (asleep or off)");
            out.push(Observation {
                id: KEY.into(),
                name: Some("Rainy75 V2".into()),
                kind: Some(Kind::Keyboard),
                state: State::Unreachable,
            });
        }
        out
    }
}

fn read_battery(dev: &HidDevice, deadline: Instant) -> hidapi::HidResult<Option<(u8, bool)>> {
    let mut buf = [0u8; 1024];
    let mut newest = None;
    // Drain what arrived since the last scan first.
    for _ in 0..64 {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else { return Ok(newest) };
        let ms = (left.as_millis() as i32).clamp(1, 30);
        match dev.read_timeout(&mut buf, ms) {
            Ok(0) => {}
            Ok(n) => {
                if let Some(v) = telemetry(&buf[..n]) {
                    newest = Some(v);
                }
            }
            Err(e) => return Err(e),
        }
    }
    if newest.is_some() {
        return Ok(newest);
    }
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let ms = (left.as_millis() as i32).clamp(1, 250);
        match dev.read_timeout(&mut buf, ms) {
            Ok(0) => {}
            Ok(n) => {
                if let Some(v) = telemetry(&buf[..n]) {
                    return Ok(Some(v));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(newest)
}

/// `BE BB 06 <percent> <charge> ED`.
fn telemetry(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 6 || r[0] != 0xBE || r[1] != 0xBB || r[2] != 0x06 {
        return None;
    }
    Some((r[3].min(100), r[4] != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_telemetry() {
        // 78%, on battery, as the WOBHub battery tool logs it.
        assert_eq!(telemetry(&[0xBE, 0xBB, 0x06, 78, 0x00, 0xED]), Some((78, false)));
        assert_eq!(telemetry(&[0xBE, 0xBB, 0x06, 78, 0x01, 0xED]), Some((78, true)));
        assert_eq!(telemetry(&[0xBE, 0xBB, 0x06, 200, 0x00, 0xED]), Some((100, false)));
        assert_eq!(telemetry(&[0xBE, 0x93, 0x05, 1, 0xED]), None);
        assert_eq!(telemetry(&[0xBE, 0xBB]), None);
    }

    #[test]
    fn enable_frame() {
        assert_eq!(ENABLE, [0xBE, 0x93, 0x05, 0x01, 0xED]);
    }
}
