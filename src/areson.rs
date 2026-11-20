//! Compx/Areson 25A7 receivers (Redragon M693/M913/M915, Pulsar X2, ARDOR Edge Air Ultra and
//! other OEM mice): the charge isn't a standard HID usage, so the receiver has to be asked.
//!
//! Request: feature report 0x08, 17 bytes, on the page 0xFF02 collection — command 0x04 (GetPower)
//! then zeros, with a checksum in the last byte (0x55 minus the sum of the bytes before it):
//! `08 04 00 … 00 49`.
//! Reply: input report 0x09 on the sibling page 0xFF01 collection, same layout and checksum.
//! Byte 6 is the level, byte 7 the charging flag. FA7B/FA7C report a whole percentage, FA07/FA08
//! tenths (10 = 100%).
//!
//! Protocol from redragon-mouse-linux (PROTOCOL.md), Qehbr/m913-ctl and
//! Wireless-Device-Battery-Monitor (docs/areson.md). Only GetPower is ever sent.

use crate::device::{Kind, Observation, State};
use crate::diag;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::ffi::CString;
use std::time::Duration;

const VID: u16 = 0x25A7;
const REQUEST_PAGE: u16 = 0xFF02;
const REPLY_PAGE: u16 = 0xFF01;
const REPORT_ID: u8 = 0x08;
const REPLY_ID: u8 = 0x09;
const COMMAND: u8 = 0x04;
const LEN: usize = 17;
const ATTEMPTS: usize = 2;
const REPLY_TIMEOUT: Duration = Duration::from_millis(1000);

/// Receiver/wired product ids and whether the level is a whole percentage (FA7B/FA7C) or tenths
/// (FA07/FA08).
const PIDS: &[(u16, bool)] = &[(0xFA7B, true), (0xFA7C, true), (0xFA07, false), (0xFA08, false)];

struct Receiver {
    request: HidDevice,
    reply: HidDevice,
    key: String,
    whole_percent: bool,
    name: Option<String>,
}

#[derive(Default)]
pub struct Areson {
    open: HashMap<(u16, CString), Receiver>,
}

impl Areson {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut found: Vec<(u16, CString, CString, bool)> = Vec::new();
        for (pid, whole) in PIDS {
            let mut request = None;
            let mut reply = None;
            for d in api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == *pid) {
                match d.usage_page() {
                    REQUEST_PAGE => request = Some(d.path().to_owned()),
                    REPLY_PAGE => reply = Some(d.path().to_owned()),
                    _ => {}
                }
            }
            if let (Some(request), Some(reply)) = (request, reply) {
                found.push((*pid, request, reply, *whole));
            }
        }
        self.open.retain(|(pid, path), _| found.iter().any(|(p, r, _, _)| p == pid && r == path));

        let mut out = Vec::new();
        for (pid, request_path, reply_path, whole_percent) in found {
            let cache_key = (pid, request_path);
            if !self.open.contains_key(&cache_key) {
                let key = format!("areson:{pid:04x}");
                let open = |path: &CString| api.open_path(path).map_err(|e| e.to_string());
                match (open(&cache_key.1), open(&reply_path)) {
                    (Ok(request), Ok(reply)) => {
                        self.open.insert(cache_key.clone(), Receiver { request, reply, key, whole_percent, name: None });
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        diag::event(&key, format!("open failed: {e}"));
                        continue;
                    }
                }
            }
            let r = self.open.get_mut(&cache_key).unwrap();
            let state = match query(&r.request, &r.reply) {
                Ok(Some((level, charging))) => {
                    let percent = if r.whole_percent { Some(level) } else { level.checked_mul(10) };
                    match percent.filter(|p| *p <= 100) {
                        Some(percent) => {
                            r.name = Some("Wireless mouse".into());
                            diag::event(&r.key, format!("battery via report 0x08 (level {level}, charging {charging})"));
                            State::Battery { percent, charging }
                        }
                        None => {
                            diag::event(&r.key, format!("level {level} out of range"));
                            State::Unreachable
                        }
                    }
                }
                Ok(None) => {
                    diag::event(&r.key, "no reply (mouse asleep or off)");
                    State::Unreachable
                }
                Err(e) => {
                    diag::event(&r.key, format!("i/o failed: {e}"));
                    self.open.remove(&cache_key);
                    continue;
                }
            };
            // Until the mouse has answered once, there's no name to show.
            if let Some(name) = r.name.clone() {
                out.push(Observation { id: r.key.clone(), name: Some(name), kind: Some(Kind::Mouse), state });
            }
        }
        out
    }
}

fn query(request: &HidDevice, reply: &HidDevice) -> hidapi::HidResult<Option<(u8, bool)>> {
    let mut req = [0u8; LEN];
    req[0] = REPORT_ID;
    req[1] = COMMAND;
    req[LEN - 1] = checksum(&req[..LEN - 1]);

    let mut last_err = None;
    for _ in 0..ATTEMPTS {
        // A frame queued before the request isn't its answer.
        flush(reply);
        if let Err(e) = request.send_feature_report(&req) {
            last_err = Some(e);
            continue;
        }
        // The answer arrives on another collection; give the radio a moment.
        let mut buf = [0u8; LEN];
        let mut waited = Duration::ZERO;
        while waited < REPLY_TIMEOUT {
            match reply.read_timeout(&mut buf, 250) {
                Ok(0) => waited += Duration::from_millis(250),
                Ok(n) => {
                    if let Some(v) = parse(&buf[..n]) {
                        return Ok(Some(v));
                    }
                }
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            }
        }
    }
    match last_err {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

fn flush(reply: &HidDevice) {
    let mut buf = [0u8; LEN];
    while let Ok(n) = reply.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
}

/// A valid reply: report 0x09, command 0x04 and the 0x55 checksum. Byte 6 is the level, byte 7
/// the charging flag.
fn parse(raw: &[u8]) -> Option<(u8, bool)> {
    if raw.len() < LEN || raw[0] != REPLY_ID || raw[1] != COMMAND || checksum(&raw[..LEN - 1]) != raw[LEN - 1] {
        return None;
    }
    Some((raw[6], raw[7] != 0))
}

/// The receiver's frames sum to 0x55 (mod 256).
fn checksum(frame: &[u8]) -> u8 {
    0x55u8.wrapping_sub(frame.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_frame() {
        // Compx 25A7:FA7C receiver, Morcego mouse: 40%, on battery.
        let frame = [0x09, 0x04, 0x00, 0x00, 0x00, 0x02, 0x28, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0x1e];
        assert_eq!(checksum(&frame[..16]), 0x1e);
        assert_eq!(parse(&frame), Some((40, false)));
    }

    #[test]
    fn parses_charging_and_rejects_noise() {
        let mut frame = [0u8; LEN];
        frame[0] = REPLY_ID;
        frame[1] = COMMAND;
        frame[6] = 100;
        frame[7] = 0x10;
        frame[LEN - 1] = checksum(&frame[..LEN - 1]);
        assert_eq!(parse(&frame), Some((100, true)));

        frame[LEN - 1] ^= 0xFF; // bad checksum
        assert_eq!(parse(&frame), None);
        frame[LEN - 1] = checksum(&frame[..LEN - 1]);
        frame[1] = 0x0A; // another status frame, not a GetPower reply
        assert_eq!(parse(&frame), None);
    }

    #[test]
    fn request_is_get_power_with_its_checksum() {
        let mut req = [0u8; LEN];
        req[0] = REPORT_ID;
        req[1] = COMMAND;
        assert_eq!(checksum(&req[..LEN - 1]), 0x49);
    }
}
