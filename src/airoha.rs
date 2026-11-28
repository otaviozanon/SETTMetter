//! Headsets behind Airoha RACE USB dongles: Audeze Maxwell, ENDORFY Viro and AKG N9 Hybrid.
//!
//! The vendor collection FF13:0001 carries a 62-byte output report 06 and an input report 07.
//! The only query sent is Airoha's read-only "get battery":
//! `06 07 80 | 05 5A 03 00 D6 0C 00` — RACE command 0x0A05, length 3, attribute 0x0CD6.
//! The answer arrives as an input report 07 holding RACE indications; the battery one is
//! `05 5D 05 00 D6 0C 00 00 <percent>`.
//!
//! Protocol from Wireless-Device-Battery-Monitor (drivers/audeze.cpp, endorfy.cpp, akg.cpp),
//! which checked the frames on hardware. There is no charging flag: a Maxwell on its USB-C
//! cable is charging, and the dongles report nothing.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::HidApi;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const OUT_ID: u8 = 0x06;
const IN_ID: u8 = 0x07;
const READ_DELAY: Duration = Duration::from_millis(60);
const READS: usize = 10;
/// A Maxwell reports 0% for a moment right after power-on.
const ZERO_GRACE: Duration = Duration::from_secs(90);

struct Model {
    vid: u16,
    pid: u16,
    name: &'static str,
    cable: bool,
}

/// Cables first: when both the cable and the dongle answer, the cable is the charging one.
const MODELS: &[Model] = &[
    Model { vid: 0x3329, pid: 0x4B1A, name: "Audeze Maxwell", cable: true },
    Model { vid: 0x3329, pid: 0x4B1E, name: "Audeze Maxwell", cable: true },
    Model { vid: 0x3329, pid: 0x4B19, name: "Audeze Maxwell", cable: false },
    Model { vid: 0x3329, pid: 0x4B18, name: "Audeze Maxwell", cable: false },
    Model { vid: 0x3329, pid: 0x4B29, name: "Audeze Maxwell 2", cable: false },
    Model { vid: 0x3329, pid: 0x4B28, name: "Audeze Maxwell 2", cable: false },
    Model { vid: 0x3738, pid: 0x0072, name: "ENDORFY Viro Wireless", cable: false },
    Model { vid: 0x0ECB, pid: 0x20D9, name: "AKG N9 Hybrid", cable: false },
];

#[derive(Default)]
pub struct Airoha {
    /// Keys that answered once: a sleeping headset keeps its last reading.
    seen: HashSet<String>,
    first_seen: HashMap<String, Instant>,
}

impl Airoha {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for model in MODELS {
            let key = format!("airoha:{:04x}", model.pid);
            let infos: Vec<_> = api
                .device_list()
                .filter(|d| d.vendor_id() == model.vid && d.product_id() == model.pid && d.usage_page() == 0xFF13)
                .collect();
            let Some(first) = infos.first() else { continue };
            // A dongle with no headset linked names itself "... Dongle" and keeps a stale level.
            let product = first.product_string().unwrap_or("");
            if !model.cable && product.to_ascii_lowercase().ends_with("dongle") {
                continue;
            }
            // Cables first: skip the dongle once the same headset already reported a level.
            let name = model.name.to_string();
            if out.iter().any(|o: &Observation| {
                o.name.as_deref() == Some(name.as_str()) && matches!(o.state, State::Battery { .. })
            }) {
                continue;
            }
            let state = infos.iter().find_map(|info| read(api, info.path(), &key, model.cable, &mut self.first_seen));
            let Some(state) = state else {
                if self.seen.contains(&key) {
                    out.push(observation(&key, &name, State::Unreachable));
                }
                continue;
            };
            self.seen.insert(key.clone());
            out.push(observation(&key, &name, state));
        }
        out
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Headset), state }
}

fn read(api: &HidApi, path: &std::ffi::CStr, key: &str, cable: bool, first_seen: &mut HashMap<String, Instant>) -> Option<State> {
    let dev = api.open_path(path).ok()?;
    let desc = probe::descriptor(&dev)?;
    let out_len = probe::output_len(&desc, OUT_ID)?;
    let in_len = probe::input_len(&desc, IN_ID)?;
    if out_len < 10 || in_len < 9 {
        return None;
    }
    let mut req = vec![0u8; out_len];
    req[..10].copy_from_slice(&[OUT_ID, OUT_ID, 0x80, 0x05, 0x5A, 0x03, 0x00, 0xD6, 0x0C, 0x00]);
    if let Err(e) = dev.write(&req) {
        diag::event(key, format!("i/o failed: {e}"));
        return None;
    }

    let mut best = None;
    for _ in 0..READS {
        std::thread::sleep(READ_DELAY);
        let mut buf = vec![0u8; in_len];
        buf[0] = IN_ID;
        let Ok(n) = dev.get_input_report(&mut buf) else { continue };
        if let Some(level) = level(&buf[..n]) {
            best = Some(level);
        }
        if best.is_some() {
            break;
        }
    }
    let level = best?;
    let first = *first_seen.entry(key.into()).or_insert_with(Instant::now);
    if level == 0 && !cable && first.elapsed() < ZERO_GRACE {
        return Some(State::Unreachable);
    }
    Some(State::Battery { percent: level, charging: cable })
}

/// The newest battery indication inside a report 07. Byte 1 is how many bytes of it are new;
/// with the headset off an old battery frame can still sit in the leftover tail.
fn level(r: &[u8]) -> Option<u8> {
    if r.len() < 9 || r[0] != IN_ID || r[1] == 0 {
        return None;
    }
    let end = r.len().min(3 + r[1] as usize);
    let mut i = 3;
    while i + 8 < end {
        if r[i] != 0x05 {
            i += 1;
            continue;
        }
        let len = usize::from(r[i + 2]) | (usize::from(r[i + 3]) << 8);
        if len < 5 || i + 4 + len > end {
            break;
        }
        let op = u16::from(r[i + 4]) | (u16::from(r[i + 5]) << 8);
        if r[i + 1] == 0x5D && op == 0x0CD6 && r[i + 6] == 0 && r[i + 8] <= 100 {
            return Some(r[i + 8]);
        }
        i += 4 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(frame: &[u8], valid: u8) -> Vec<u8> {
        let mut r = vec![IN_ID, valid, 0x80];
        r.extend_from_slice(frame);
        r.resize(62, 0);
        r
    }

    #[test]
    fn parses_the_akg_capture() {
        // AKG N9 Hybrid playing music: 50%, from the protocol notes.
        let r = report(&[0x05, 0x5D, 0x05, 0x00, 0xD6, 0x0C, 0x00, 0x00, 0x32], 9);
        assert_eq!(level(&r), Some(50));
    }

    #[test]
    fn ignores_stale_tail_and_acknowledgements() {
        let mut r = report(&[0x05, 0x5D, 0x05, 0x00, 0xD6, 0x0C, 0x00, 0x00, 0x64], 1);
        assert_eq!(level(&r), None); // byte 1 says nothing new
        r[1] = 9;
        assert_eq!(level(&r), Some(100));
        // A 5B acknowledgement with zeros is not a level.
        let ack = report(&[0x05, 0x5B, 0x03, 0x00, 0xD6, 0x0C, 0x00], 7);
        assert_eq!(level(&ack), None);
        assert_eq!(level(&[0x06, 0x01, 0x80]), None);
    }

    #[test]
    fn charging_only_on_the_cable() {
        assert!(!MODELS[2].cable);
        assert!(MODELS[0].cable);
        assert_eq!(MODELS[6].name, "ENDORFY Viro Wireless");
    }
}
