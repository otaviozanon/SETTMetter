//! Logitech G735 headset on its LIGHTSPEED dongle (046D:0AD8).
//!
//! The dongle has no classic HID++ collections. The headset is reached through the vendor
//! collection FF03:0001, report F2 (271 bytes in and out), which wraps Centurion-style HID++
//! 2.0 messages:
//!
//! ```text
//! request  F2 01 05 1E FA 82 40 CE 00 00 00 20 00 00 00 00 | idx  fn<<4|sw  params...
//! reply    F2 00 00 0C 1E 82 40 00 00 F2 00 len 00 00 00 00 | idx  fn<<4|sw  data...
//! error    ...                                              | FF   idx       fn<<4|sw  code
//! ```
//!
//! Root (index 0) getFeature(0x0104 CenturionBatterySoc) gives the battery feature index; its
//! function 0 returns `[level %][next level %][charging state]`, charging state 1 or 2. Error 5
//! = no such feature. Software id 0xB keeps replies apart from G HUB (0xD) and other apps (0xF).
//!
//! Source: USB-level capture of G HUB 2026.11 talking to the headset (G HUB showed 40 %, the
//! reply said 41), then reproduced with these two read-only requests only. Not verified here.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x046D;
const PID: u16 = 0x0AD8;
const KEY: &str = "logi:0ad8";
const NAME: &str = "Logitech G735";
const REPORT: u8 = 0xF2;
const SW_ID: u8 = 0x0B;
/// Offset of the HID++ message inside the report.
const PAYLOAD: usize = 16;
const REPORT_LEN: usize = 271;
const DEADLINE: Duration = Duration::from_millis(1000);
const HEADER: [u8; PAYLOAD] = [
    0xF2, 0x01, 0x05, 0x1E, 0xFA, 0x82, 0x40, 0xCE, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00,
];
const BATTERY_SOC: u16 = 0x0104;

#[derive(Default)]
pub struct LogitechG735 {}

impl LogitechG735 {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let found = api.device_list().find(|d| {
            d.vendor_id() == VID
                && d.product_id() == PID
                && d.usage_page() == 0xFF03
                && d.usage() == 0x0001
        });
        let Some(info) = found else { return Vec::new() };
        let Ok(dev) = api.open_path(info.path()) else {
            diag::event(KEY, "open failed");
            return Vec::new();
        };
        let Some(desc) = probe::descriptor(&dev) else { return Vec::new() };
        if probe::input_len(&desc, REPORT) != Some(REPORT_LEN) || probe::output_len(&desc, REPORT) != Some(REPORT_LEN) {
            return Vec::new();
        }

        let end = Instant::now() + DEADLINE;
        let feature = call(&dev, end, 0x00, 0x00, &BATTERY_SOC.to_be_bytes()).and_then(|p| p.first().copied());
        let state = match feature {
            Some(idx) if idx != 0 => match call(&dev, end, idx, 0x00, &[]) {
                Some(p) if p.first().is_some_and(|v| *v <= 100) => {
                    let percent = p[0];
                    let charging = p.get(2).is_some_and(|v| matches!(v, 1 | 2));
                    diag::event(KEY, format!("battery via CenturionBatterySoc 0x{idx:02x}: {percent}%"));
                    State::Battery { percent, charging }
                }
                _ => {
                    // The dongle answers for an absent headset with an error or not at all.
                    diag::event(KEY, "no battery reply (headset off or out of range)");
                    State::Off
                }
            },
            _ => {
                diag::event(KEY, "CenturionBatterySoc feature not found");
                State::Off
            }
        };
        vec![Observation { id: KEY.into(), name: Some(NAME.into()), kind: Some(Kind::Headset), state }]
    }
}

enum Reply {
    Data(Vec<u8>),
    Error,
    Other,
}

/// One request/response before `end`; other apps' reports and unrelated frames are skipped.
fn call(dev: &HidDevice, end: Instant, idx: u8, function: u8, params: &[u8]) -> Option<Vec<u8>> {
    let fnsw = (function << 4) | SW_ID;
    let mut req = vec![0u8; REPORT_LEN];
    req[..PAYLOAD].copy_from_slice(&HEADER);
    req[PAYLOAD] = idx;
    req[PAYLOAD + 1] = fnsw;
    let n = params.len().min(REPORT_LEN - PAYLOAD - 2);
    req[PAYLOAD + 2..PAYLOAD + 2 + n].copy_from_slice(&params[..n]);

    drain(dev);
    if dev.write(&req).is_err() {
        return None;
    }
    let mut buf = [0u8; 1024];
    while Instant::now() < end {
        let left = end.saturating_duration_since(Instant::now()).as_millis().max(1) as i32;
        let n = match dev.read_timeout(&mut buf, left.min(250)) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(_) => return None,
        };
        match parse(&buf[..n], idx, fnsw) {
            Reply::Data(p) => return Some(p),
            Reply::Error => return None,
            Reply::Other => {}
        }
    }
    None
}

fn parse(r: &[u8], idx: u8, fnsw: u8) -> Reply {
    if r.len() < PAYLOAD + 4 || r[0] != REPORT || r[1] != 0x00 {
        return Reply::Other;
    }
    let m = &r[PAYLOAD..];
    if m[0] == 0xFF && m[1] == idx && m[2] == fnsw {
        return Reply::Error;
    }
    if m[0] == idx && m[1] == fnsw {
        return Reply::Data(r[PAYLOAD + 2..].to_vec());
    }
    Reply::Other
}

fn drain(dev: &HidDevice) {
    let mut stale = [0u8; 1024];
    while let Ok(n) = dev.read_timeout(&mut stale, 0) {
        if n == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(msg: &[u8]) -> Vec<u8> {
        // Reply header from the capture: F2 00 00 0C 1E 82 40 00 00 F2 00 len ...
        let mut r = vec![0u8; REPORT_LEN];
        r[..PAYLOAD].copy_from_slice(&[0xF2, 0x00, 0x00, 0x0C, 0x1E, 0x82, 0x40, 0x00, 0x00, 0xF2, 0x00, 0x06, 0, 0, 0, 0]);
        r[PAYLOAD..PAYLOAD + msg.len()].copy_from_slice(msg);
        r
    }

    #[test]
    fn parses_the_captured_reply_header() {
        // Root getFeature(0x0104) answered feature index 2.
        let r = report(&[0x00, 0x0B, 0x02, 0x00, 0x00, 0x03]);
        assert!(matches!(parse(&r, 0x00, 0x0B), Reply::Data(p) if p[0] == 2));
        // Function 0 of that feature: 41 %, next 42 %, charging state 1.
        let r = report(&[0x02, 0x0B, 0x29, 0x2A, 0x01]);
        assert!(matches!(parse(&r, 0x02, 0x0B), Reply::Data(p) if p[0] == 41 && p[2] == 1));
    }

    #[test]
    fn rejects_other_apps_and_errors() {
        // G HUB uses software id 0xD: not our reply.
        let r = report(&[0x02, 0x0D, 0x29, 0x00, 0x00]);
        assert!(matches!(parse(&r, 0x02, 0x0B), Reply::Other));
        // HID++ error 5 (no such feature).
        let r = report(&[0xFF, 0x02, 0x0B, 0x05]);
        assert!(matches!(parse(&r, 0x02, 0x0B), Reply::Error));
        // Short and non-F2 frames are ignored.
        assert!(matches!(parse(&[0xF2, 0x00], 0x00, 0x0B), Reply::Other));
        let mut r = report(&[0x02, 0x0B, 0x29]);
        r[0] = 0x11;
        assert!(matches!(parse(&r, 0x02, 0x0B), Reply::Other));
    }
}
