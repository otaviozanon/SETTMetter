//! MCHOSE mice that use the "classic" M HUB status query (A7, A7 V2, L7, M7, K7 Ultra…),
//! over their 2.4 GHz receiver or a cable.
//!
//! Protocol from MchoseBatteryTray (MIT, https://github.com/HifumiDaisuki01/MchoseBatteryTray),
//! which read it from MCHOSE's own web driver and confirmed it on an A7 V2 Pro:
//!
//! - Set feature report 0x11 with every payload byte inverted (XOR 0xFF): command 0x06, then
//!   zeros, so `11 F9 FF FF …` (21 bytes). Then get feature report 0x11.
//! - The reply echoes `11 F9`; the rest, inverted, is: vid u16 LE, the mouse's own model id
//!   u16 LE (not the receiver's USB id), firmware u32, flags (bits 0-2 connection mode,
//!   bit 3 linked), battery % and charge state (0 on battery, 1 charging, 2 full).
//!
//! Newer models (A7 V3, K7 V2, K5…) use a different protocol and aren't covered here.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::ffi::CString;
use std::time::Duration;

const VIDS: [u16; 2] = [0x3837, 0x5253];
const USAGE_PAGES: [u16; 2] = [0xFF01, 0xFF0B];
const REPORT_ID: u8 = 0x11;
const COMMAND: u8 = 0x06;
const MIN_LEN: usize = 21;
const ATTEMPTS: usize = 3;

/// USB product ids of classic-protocol receivers and wired mice.
const USB_PIDS: &[u16] = &[
    0x1000, 0x1001, 0x1009, 0x100A, 0x100B, 0x100C, 0x1020, 0x1021, // receivers
    16, 17, 32, 48, 49, 112, 161, 176, 177, 192, 4353, 16400, 16401, 16405, 16406, 16408, 16409, 16417, 16419,
    16656, 16720, // wired, same as the model ids below
];

/// Model ids (from the reply) and names, from MCHOSE's model table.
const MODELS: &[(u16, &str)] = &[
    (16, "A7 Pro"),
    (17, "A7 Ultra"),
    (32, "M7"),
    (48, "M7 Pro"),
    (49, "M7 Ultra"),
    (112, "A7"),
    (161, "AX5 V2"),
    (176, "L7 Pro"),
    (177, "L7 Ultra"),
    (192, "L7"),
    (4353, "A5 V2 Ultra"),
    (16400, "AX5 V2"),
    (16401, "A7X Ultra"),
    (16405, "L7 Pro+"),
    (16406, "L7 Ultra+"),
    (16408, "A7 V2 Pro"),
    (16409, "A7 V2 Ultra"),
    (16417, "A7 V2 Ultra+"),
    (16419, "A7 V2 Pro+"),
    (16656, "A7 Ultra"),
    (16720, "K7 Ultra"),
];

struct Receiver {
    dev: HidDevice,
    key: String,
    len: usize,
    name: Option<String>,
}

#[derive(Default)]
pub struct MchoseMice {
    open: HashMap<CString, Receiver>,
}

impl MchoseMice {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let present: Vec<_> = api
            .device_list()
            .filter(|d| VIDS.contains(&d.vendor_id()) && USB_PIDS.contains(&d.product_id()))
            .filter(|d| USAGE_PAGES.contains(&d.usage_page()))
            .map(|d| (d.path().to_owned(), d.product_id()))
            .collect();
        self.open.retain(|path, _| present.iter().any(|(p, _)| p == path));

        let mut out = Vec::new();
        for (path, pid) in present {
            if !self.open.contains_key(&path) {
                let key = format!("mchose-mouse:{pid:04x}");
                let dev = match api.open_path(&path) {
                    Ok(dev) => dev,
                    Err(e) => {
                        diag::event(&key, format!("open failed: {e}"));
                        continue;
                    }
                };
                // The feature report's length, from the descriptor; collections without a
                // long enough report 0x11 aren't the status channel.
                let len = probe::descriptor(&dev).and_then(|d| probe::feature_len(&d, REPORT_ID)).unwrap_or(MIN_LEN);
                if len < MIN_LEN {
                    continue;
                }
                self.open.insert(path.clone(), Receiver { dev, key, len, name: None });
            }
            let r = self.open.get_mut(&path).unwrap();
            let reading = match query(&r.dev, r.len) {
                Ok(reading) => reading,
                Err(e) => {
                    diag::event(&r.key, format!("i/o failed: {e}"));
                    self.open.remove(&path);
                    continue;
                }
            };
            let state = match reading {
                Some(s) if s.linked => {
                    r.name = Some(model_name(s.model));
                    diag::event(&r.key, format!("battery via report 0x11 (model {}, mode {})", s.model, s.mode));
                    State::Battery { percent: s.percent, charging: matches!(s.charge, 1 | 2) }
                }
                Some(_) => {
                    diag::event(&r.key, "mouse not linked (asleep or off)");
                    State::Unreachable
                }
                None => {
                    diag::event(&r.key, "no valid status reply");
                    State::Unreachable
                }
            };
            // Until the mouse has answered once, there's no name to show.
            if r.name.is_some() {
                out.push(Observation { id: r.key.clone(), name: r.name.clone(), kind: Some(Kind::Mouse), state });
            }
        }
        out
    }
}

fn model_name(model: u16) -> String {
    match MODELS.iter().find(|(id, _)| *id == model) {
        Some((_, name)) => format!("MCHOSE {name}"),
        None => "MCHOSE mouse".into(),
    }
}

fn query(dev: &HidDevice, len: usize) -> hidapi::HidResult<Option<Status>> {
    let mut req = vec![0xFF; len];
    req[0] = REPORT_ID;
    req[1] = !COMMAND;
    let mut last_err = None;
    for _ in 0..ATTEMPTS {
        if let Err(e) = dev.send_feature_report(&req) {
            last_err = Some(e);
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        std::thread::sleep(Duration::from_millis(100));
        let mut buf = vec![0u8; len];
        buf[0] = REPORT_ID;
        match dev.get_feature_report(&mut buf) {
            Ok(n) => {
                if let Some(s) = parse(&buf[..n.min(len)]) {
                    return Ok(Some(s));
                }
                last_err = None;
            }
            Err(e) => last_err = Some(e),
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    match last_err {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

#[derive(Debug, PartialEq)]
struct Status {
    model: u16,
    mode: u8,
    linked: bool,
    percent: u8,
    charge: u8,
}

fn parse(raw: &[u8]) -> Option<Status> {
    if raw.len() < 13 || raw[0] != REPORT_ID || raw[1] != !COMMAND {
        return None;
    }
    let d: Vec<u8> = raw[2..].iter().map(|b| !b).collect();
    let vid = u16::from_le_bytes([d[0], d[1]]);
    let model = u16::from_le_bytes([d[2], d[3]]);
    if !VIDS.contains(&vid) || model == 0 || model == 0xFFFF || d[9] > 100 {
        return None;
    }
    Some(Status { model, mode: d[8] & 7, linked: d[8] & 8 != 0, percent: d[9], charge: d[10] })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(vid: u16, model: u16, flags: u8, percent: u8, charge: u8) -> Vec<u8> {
        let mut d = [0u8; 19];
        d[0..2].copy_from_slice(&vid.to_le_bytes());
        d[2..4].copy_from_slice(&model.to_le_bytes());
        d[8] = flags;
        d[9] = percent;
        d[10] = charge;
        let mut r = vec![0x11, 0xF9];
        r.extend(d.iter().map(|b| !b));
        r
    }

    #[test]
    fn parses_inverted_status() {
        // A7 V2 Pro on the 2.4 GHz receiver (mode 1, linked), 64%, on battery.
        let s = parse(&reply(0x3837, 16408, 0x08 | 1, 64, 0)).unwrap();
        assert_eq!(s, Status { model: 16408, mode: 1, linked: true, percent: 64, charge: 0 });
        assert_eq!(model_name(s.model), "MCHOSE A7 V2 Pro");
    }

    #[test]
    fn rejects_bad_replies() {
        assert_eq!(parse(&reply(0x046D, 16408, 9, 64, 0)), None); // wrong vendor
        assert_eq!(parse(&reply(0x3837, 0, 9, 64, 0)), None); // empty identity
        assert_eq!(parse(&reply(0x3837, 16408, 9, 101, 0)), None); // out of range
        let mut r = reply(0x3837, 16408, 9, 64, 0);
        r[1] = 0x06; // not inverted: not a status reply
        assert_eq!(parse(&r), None);
    }

    #[test]
    fn request_is_inverted() {
        assert_eq!(!COMMAND, 0xF9);
    }
}
