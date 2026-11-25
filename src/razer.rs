//! Razer wireless mice and keyboards, over their receiver or a cable.
//!
//! Protocol as documented by OpenRazer (https://github.com/openrazer/openrazer). Commands are
//! a 90-byte feature report without a report id:
//!
//! ```text
//! [0] status (0 in a request; 2 = ok, 1 busy, 3 failed, 4 timed out, 5 not supported)
//! [1] transaction id (per model: 0xFF, 0x9F, 0x3F or 0x1F)
//! [2..4] remaining packets, [4] protocol type, [5] data size
//! [6] command class, [7] command id, [8..88] arguments
//! [88] XOR of bytes 2..88, [89] reserved
//! ```
//!
//! Battery is class 0x07 id 0x80 (argument 1, 0-255) and charging 0x07 0x84 (argument 1,
//! 0/1). After the request the receiver needs some time before the reply can be read, up
//! to 400 ms on some receivers.
//!
//! The model table (USB product id, transaction id, wait, whether it reports charging) is
//! taken from OpenRazer's mouse and keyboard drivers. The collection that carries the 90-byte feature
//! report differs between models, so it's found from the descriptors.

use crate::device::Kind::{self, Keyboard, Mouse};
use crate::device::{Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::ffi::CString;
use std::time::Duration;

const VID: u16 = 0x1532;
const REPORT_LEN: usize = 90;
const ATTEMPTS: usize = 3;

const STATUS_OK: u8 = 0x02;
const STATUS_BUSY: u8 = 0x01;
const CLASS_POWER: u8 = 0x07;
const CMD_BATTERY: u8 = 0x80;
const CMD_CHARGING: u8 = 0x84;

/// (USB product id, name, transaction id, wait in ms, reports charging, kind). Receivers
/// and the same device on a cable share a name, so they show as one device.
const MODELS: &[(u16, &str, u8, u64, bool, Kind)] = &[
    // Mice
    (0x001F, "Razer Naga Epic", 0xFF, 1, true, Mouse),
    (0x0024, "Razer Mamba 2012", 0xFF, 1, true, Mouse),
    (0x0025, "Razer Mamba 2012", 0xFF, 1, true, Mouse),
    (0x0032, "Razer Ouroboros", 0xFF, 1, true, Mouse),
    (0x003E, "Razer Naga Epic Chroma", 0xFF, 1, true, Mouse),
    (0x003F, "Razer Naga Epic Chroma", 0xFF, 1, true, Mouse),
    (0x0044, "Razer Mamba", 0xFF, 1, true, Mouse),
    (0x0045, "Razer Mamba", 0xFF, 1, true, Mouse),
    (0x0059, "Razer Lancehead", 0x3F, 1, true, Mouse),
    (0x005A, "Razer Lancehead", 0x3F, 1, true, Mouse),
    (0x0062, "Razer Atheris", 0x1F, 400, false, Mouse),
    (0x006F, "Razer Lancehead", 0x1F, 31, true, Mouse),
    (0x0070, "Razer Lancehead", 0x1F, 31, true, Mouse),
    (0x0072, "Razer Mamba", 0x3F, 31, true, Mouse),
    (0x0073, "Razer Mamba", 0x3F, 31, true, Mouse),
    (0x0077, "Razer Pro Click", 0x1F, 31, true, Mouse),
    (0x007A, "Razer Viper Ultimate", 0xFF, 60, true, Mouse),
    (0x007B, "Razer Viper Ultimate", 0xFF, 60, true, Mouse),
    (0x007C, "Razer DeathAdder V2 Pro", 0x3F, 60, true, Mouse),
    (0x007D, "Razer DeathAdder V2 Pro", 0x3F, 60, true, Mouse),
    (0x0080, "Razer Pro Click", 0x1F, 31, true, Mouse),
    (0x0083, "Razer Basilisk X HyperSpeed", 0xFF, 31, false, Mouse),
    (0x0086, "Razer Basilisk Ultimate", 0x1F, 31, true, Mouse),
    (0x0088, "Razer Basilisk Ultimate", 0x1F, 31, true, Mouse),
    (0x008F, "Razer Naga Pro", 0x1F, 1, true, Mouse),
    (0x0090, "Razer Naga Pro", 0x1F, 31, true, Mouse),
    (0x0094, "Razer Orochi V2", 0x1F, 400, false, Mouse),
    (0x009A, "Razer Pro Click Mini", 0x1F, 31, true, Mouse),
    (0x009C, "Razer DeathAdder V2 X HyperSpeed", 0x1F, 31, false, Mouse),
    (0x009E, "Razer Viper Mini SE", 0x1F, 60, true, Mouse),
    (0x009F, "Razer Viper Mini SE", 0x1F, 60, true, Mouse),
    (0x00A5, "Razer Viper V2 Pro", 0x1F, 31, true, Mouse),
    (0x00A6, "Razer Viper V2 Pro", 0x1F, 31, true, Mouse),
    (0x00A7, "Razer Naga V2 Pro", 0x1F, 31, true, Mouse),
    (0x00A8, "Razer Naga V2 Pro", 0x1F, 31, true, Mouse),
    (0x00AA, "Razer Basilisk V3 Pro", 0x1F, 31, true, Mouse),
    (0x00AB, "Razer Basilisk V3 Pro", 0x1F, 31, true, Mouse),
    (0x00AF, "Razer Cobra Pro", 0x1F, 31, true, Mouse),
    (0x00B0, "Razer Cobra Pro", 0x1F, 31, true, Mouse),
    (0x00B3, "Razer mouse (HyperPolling dongle)", 0x1F, 60, true, Mouse),
    (0x00B4, "Razer Naga V2 HyperSpeed", 0x1F, 31, false, Mouse),
    (0x00B6, "Razer DeathAdder V3 Pro", 0x1F, 31, true, Mouse),
    (0x00B7, "Razer DeathAdder V3 Pro", 0x1F, 31, true, Mouse),
    (0x00B8, "Razer Viper V3 HyperSpeed", 0x1F, 60, false, Mouse),
    (0x00B9, "Razer Basilisk V3 X HyperSpeed", 0x1F, 31, false, Mouse),
    (0x00BE, "Razer DeathAdder V4 Pro", 0x1F, 31, true, Mouse),
    (0x00BF, "Razer DeathAdder V4 Pro", 0x1F, 31, true, Mouse),
    (0x00C0, "Razer Viper V3 Pro", 0x1F, 31, true, Mouse),
    (0x00C1, "Razer Viper V3 Pro", 0x1F, 60, true, Mouse),
    (0x00C2, "Razer DeathAdder V3 Pro", 0x1F, 31, true, Mouse),
    (0x00C3, "Razer DeathAdder V3 Pro", 0x1F, 31, true, Mouse),
    (0x00C4, "Razer DeathAdder V3 HyperSpeed", 0x1F, 31, true, Mouse),
    (0x00C5, "Razer DeathAdder V3 HyperSpeed", 0x1F, 31, true, Mouse),
    (0x00C7, "Razer Pro Click V2 Vertical Edition", 0x1F, 31, true, Mouse),
    (0x00C8, "Razer Pro Click V2 Vertical Edition", 0x1F, 31, true, Mouse),
    (0x00CC, "Razer Basilisk V3 Pro 35K", 0x1F, 31, true, Mouse),
    (0x00CD, "Razer Basilisk V3 Pro 35K", 0x1F, 31, true, Mouse),
    (0x00D0, "Razer Pro Click V2", 0x1F, 31, true, Mouse),
    (0x00D1, "Razer Pro Click V2", 0x1F, 31, true, Mouse),
    (0x00D3, "Razer Basilisk Mobile", 0x1F, 31, false, Mouse),
    (0x00D4, "Razer Basilisk Mobile", 0x1F, 31, false, Mouse),
    (0x00D6, "Razer Basilisk V3 Pro 35K Phantom Green Edition", 0x1F, 31, true, Mouse),
    (0x00D7, "Razer Basilisk V3 Pro 35K Phantom Green Edition", 0x1F, 31, true, Mouse),
    (0x00DA, "Razer Cobra HyperSpeed", 0x1F, 31, true, Mouse),
    (0x00DB, "Razer Cobra HyperSpeed", 0x1F, 31, true, Mouse),
    (0x00DE, "Razer Viper V3 Pro SE", 0x1F, 31, true, Mouse),
    (0x00DF, "Razer Viper V3 Pro SE", 0x1F, 60, true, Mouse),
    (0x00E0, "Razer Orochi V2 2026", 0x1F, 400, false, Mouse),
    (0x00E5, "Razer Viper V4 Pro", 0x1F, 60, true, Mouse),
    (0x00E6, "Razer Viper V4 Pro", 0x1F, 60, true, Mouse),
    // Keyboards
    (0x0258, "Razer BlackWidow V3 Mini HyperSpeed", 0x1F, 1, true, Keyboard),
    (0x025A, "Razer BlackWidow V3 Pro", 0x3F, 1, true, Keyboard),
    (0x025C, "Razer BlackWidow V3 Pro", 0x9F, 5, true, Keyboard),
    (0x0271, "Razer BlackWidow V3 Mini HyperSpeed", 0x9F, 5, true, Keyboard),
    (0x0290, "Razer DeathStalker V2 Pro", 0x9F, 5, true, Keyboard),
    (0x0292, "Razer DeathStalker V2 Pro", 0x1F, 1, true, Keyboard),
    (0x0296, "Razer DeathStalker V2 Pro TKL", 0x9F, 5, true, Keyboard),
    (0x0298, "Razer DeathStalker V2 Pro TKL", 0x1F, 1, true, Keyboard),
    (0x02B9, "Razer BlackWidow V4 Mini HyperSpeed", 0x1F, 1, true, Keyboard),
    (0x02BA, "Razer BlackWidow V4 Mini HyperSpeed", 0x9F, 5, true, Keyboard),
    (0x02CD, "Razer Joro", 0x1F, 1, true, Keyboard),
    (0x02D2, "Razer BlackWidow V4 Low Profile Tenkeyless HyperSpeed", 0x9F, 5, true, Keyboard),
    (0x02D4, "Razer BlackWidow V4 Low Profile Tenkeyless HyperSpeed", 0x1F, 1, true, Keyboard),
    (0x02D5, "Razer BlackWidow V4 Tenkeyless HyperSpeed", 0x9F, 5, true, Keyboard),
    (0x02D7, "Razer BlackWidow V4 Tenkeyless HyperSpeed", 0x1F, 1, true, Keyboard),
];

struct Device {
    dev: HidDevice,
    pid: u16,
    name: &'static str,
    kind: Kind,
    tid: u8,
    wait: Duration,
    charging: bool,
    seen: bool,
}

#[derive(Default)]
pub struct Razer {
    open: HashMap<CString, Device>,
    /// Collections already checked and found without the command report.
    skip: Vec<CString>,
}

impl Razer {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let present: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == VID)
            .filter_map(|d| Some((d.path().to_owned(), MODELS.iter().find(|m| m.0 == d.product_id())?)))
            .collect();
        self.open.retain(|path, _| present.iter().any(|(p, _)| p == path));
        self.skip.retain(|path| present.iter().any(|(p, _)| p == path));

        // Each device has one collection with the 90-byte feature report.
        for (path, info) in &present {
            let (pid, name, tid, wait_ms, charging, kind) = **info;
            if self.open.contains_key(path) || self.skip.contains(path) || self.open.values().any(|m| m.pid == pid) {
                continue;
            }
            let Ok(dev) = api.open_path(path) else { continue };
            if probe::descriptor(&dev).and_then(|d| probe::feature_len(&d, 0)) != Some(REPORT_LEN) {
                self.skip.push(path.clone());
                continue;
            }
            let wait = Duration::from_millis(wait_ms);
            self.open.insert(path.clone(), Device { dev, pid, name, kind, tid, wait, charging, seen: false });
        }

        let mut by_name: HashMap<&'static str, (State, Kind)> = HashMap::new();
        let mut broken = Vec::new();
        for (path, m) in self.open.iter_mut() {
            let key = format!("razer:{:04x}", m.pid);
            let state = match battery(m) {
                Ok(Some((percent, charging))) => {
                    m.seen = true;
                    diag::event(&key, "battery via 07 80");
                    State::Battery { percent, charging }
                }
                Ok(None) => {
                    diag::event(&key, "no reply (off or asleep)");
                    if !m.seen {
                        continue;
                    }
                    State::Unreachable
                }
                Err(e) => {
                    diag::event(&key, format!("i/o failed: {e}"));
                    broken.push(path.clone());
                    continue;
                }
            };
            // The receiver and the cable can both be present; a reading wins.
            let slot = by_name.entry(m.name).or_insert((state, m.kind));
            if !matches!(slot.0, State::Battery { .. }) {
                slot.0 = state;
            }
        }
        for path in broken {
            self.open.remove(&path);
        }
        by_name
            .into_iter()
            .map(|(name, (state, kind))| Observation { id: format!("razer:{}", slug(name)), name: Some(name.into()), kind: Some(kind), state })
            .collect()
    }
}

fn slug(name: &str) -> String {
    name.to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join("-")
}

/// Battery and charging state, or None when the device doesn't answer.
fn battery(m: &Device) -> hidapi::HidResult<Option<(u8, bool)>> {
    let Some(level) = command(m, CMD_BATTERY)? else { return Ok(None) };
    let charging = m.charging && command(m, CMD_CHARGING)? == Some(1);
    Ok(Some((((u32::from(level) * 100 + 127) / 255) as u8, charging)))
}

/// Sends a power-class query and returns argument 1 of a successful reply.
fn command(m: &Device, id: u8) -> hidapi::HidResult<Option<u8>> {
    let req = request(m.tid, CLASS_POWER, id, 0x02);
    for _ in 0..ATTEMPTS {
        m.dev.send_feature_report(&req)?;
        std::thread::sleep(m.wait);
        let mut buf = [0u8; REPORT_LEN + 1];
        let n = m.dev.get_feature_report(&mut buf)?;
        match parse(&buf[1..n.clamp(1, buf.len())], CLASS_POWER, id) {
            Reply::Ok(v) => return Ok(Some(v)),
            Reply::Busy => continue,
            Reply::Failed => return Ok(None),
        }
    }
    Ok(None)
}

/// A request with report id 0 in front, as hidapi expects.
fn request(tid: u8, class: u8, id: u8, size: u8) -> [u8; REPORT_LEN + 1] {
    let mut r = [0u8; REPORT_LEN + 1];
    let p = &mut r[1..];
    p[1] = tid;
    p[5] = size;
    p[6] = class;
    p[7] = id;
    p[88] = p[2..88].iter().fold(0, |c, b| c ^ b);
    r
}

#[derive(Debug, PartialEq)]
enum Reply {
    Ok(u8),
    Busy,
    Failed,
}

fn parse(r: &[u8], class: u8, id: u8) -> Reply {
    if r.len() < 10 || r[6] != class || r[7] != id {
        return Reply::Busy; // not our reply (yet)
    }
    match r[0] {
        STATUS_OK => Reply::Ok(r[9]),
        STATUS_BUSY => Reply::Busy,
        _ => Reply::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout_and_crc() {
        let r = request(0x1F, 0x07, 0x80, 0x02);
        assert_eq!(r[0], 0); // report id
        assert_eq!(&r[1..9], &[0x00, 0x1F, 0x00, 0x00, 0x00, 0x02, 0x07, 0x80]);
        assert_eq!(r[1 + 88], 0x02 ^ 0x07 ^ 0x80);
        assert_eq!(r.len(), 91);
    }

    #[test]
    fn parses_replies() {
        let mut r = [0u8; 90];
        r[0] = STATUS_OK;
        r[6] = 0x07;
        r[7] = 0x80;
        r[9] = 0xFF;
        assert_eq!(parse(&r, 0x07, 0x80), Reply::Ok(255));
        r[0] = 0x04; // timed out: mouse off
        assert_eq!(parse(&r, 0x07, 0x80), Reply::Failed);
        r[0] = STATUS_BUSY;
        assert_eq!(parse(&r, 0x07, 0x80), Reply::Busy);
        r[0] = STATUS_OK;
        assert_eq!(parse(&r, 0x07, 0x84), Reply::Busy); // reply to another command
    }

    #[test]
    fn table_is_consistent() {
        for (i, m) in MODELS.iter().enumerate() {
            assert!(MODELS[i + 1..].iter().all(|n| n.0 != m.0), "pid {:04x} twice", m.0);
            assert!(matches!(m.2, 0xFF | 0x9F | 0x3F | 0x1F));
            assert!(m.3 <= 400);
        }
        assert_eq!(slug("Razer DeathAdder V3 Pro"), "razer-deathadder-v3-pro");
    }
}
