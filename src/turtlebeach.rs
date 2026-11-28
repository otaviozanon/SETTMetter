//! Turtle Beach Stealth 700 Gen 3 (10F5) on its USB transmitter or the headset's own cable.
//!
//! Vendor collection FF13:0001 on interface 3: host -> device 62-byte output report 06 framed
//! `06 <len16 LE> <RACE bytes>`, device -> host input report 07 framed `07 <len> 00 <payload>`
//! (fetched with GetInputReport). Two RACE frames, both read-only:
//!
//! 1. `05 5A 02 00 42 99`: enable the Airoha chip's CoAP client (what Swarm II sends).
//! 2. `05 5A 0A 00 02 99 40 01 00 01 B3 47 53 49`: CoAP GET /GSI.
//!
//! The answer is a JSON document `{"OR":"GSI","KVP":{..., "240":"<percent>", ...}}` split
//! across reports. Key 250 (charging) stayed "0" with the cable in on hardware, so it is not
//! decoded. Nothing else is sent and nothing on the headset changes.
//!
//! Protocol from Wireless-Device-Battery-Monitor (drivers/turtlebeach.cpp), which credits
//! HaloBattery (MIT) and PaulRichez's Swarm II reverse engineering. Transmitter 10F5:2251 and
//! the cable 10F5:2253 were tested there; 10F5:2262 is the PC transmitter of the same headset.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::Duration;

const VID: u16 = 0x10F5;
/// Transmitters first: one headset, listed through whichever answers.
const PIDS: [u16; 3] = [0x2251, 0x2262, 0x2253];
fn key(pid: u16) -> String {
    format!("turtlebeach:{pid:04x}")
}
const FRAME: usize = 62;

const ENABLE_COAP: [u8; 6] = [0x05, 0x5A, 0x02, 0x00, 0x42, 0x99];
const GET_GSI: [u8; 14] = [0x05, 0x5A, 0x0A, 0x00, 0x02, 0x99, 0x40, 0x01, 0x00, 0x01, 0xB3, b'G', b'S', b'I'];

#[derive(Default)]
pub struct Turtlebeach {}

impl Turtlebeach {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut found = false;
        let mut found_pid = None;
        let mut state = None;
        for pid in PIDS {
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0xFF13 && d.usage() == 0x0001
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if !probe::has_output(&desc) {
                    continue;
                }
                found = true;
                found_pid = Some(pid);
                match read_level(&dev) {
                    Some(level) => {
                        state = Some(State::Battery { percent: level, charging: false });
                        break;
                    }
                    None => diag::event(&key(pid), "no GSI reply (headset off, or Swarm holding the channel)"),
                }
            }
            if state.is_some() {
                break;
            }
        }
        if !found {
            return Vec::new();
        }
        // The transmitter stays enumerated while the headset is off: keep the last level.
        vec![Observation {
            id: key(found_pid.unwrap_or(PIDS[0])),
            name: Some(name(api)),
            kind: Some(Kind::Headset),
            state: state.unwrap_or(State::Unreachable),
        }]
    }
}

fn name(api: &HidApi) -> String {
    let product = api
        .device_list()
        .find(|d| d.vendor_id() == VID && PIDS.contains(&d.product_id()))
        .and_then(|d| d.product_string())
        .unwrap_or("");
    if product.is_empty() {
        "Turtle Beach Stealth 700 Gen 3".into()
    } else if product.to_ascii_lowercase().starts_with("turtle beach") {
        product.into()
    } else {
        format!("Turtle Beach {product}")
    }
}

/// `06 <len> 00 <race...>`, zero-padded to the 62-byte output report.
fn frame(race: &[u8]) -> [u8; FRAME] {
    let mut f = [0u8; FRAME];
    f[0] = 0x06;
    f[1] = race.len() as u8;
    f[3..3 + race.len()].copy_from_slice(race);
    f
}

/// One GetInputReport of report 07.
fn input(dev: &HidDevice) -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    buf[0] = 0x07;
    dev.get_input_report(&mut buf).ok().map(|n| buf[..n].to_vec())
}

fn read_level(dev: &HidDevice) -> Option<u8> {
    for _ in 0..5 {
        let _ = input(dev);
    }
    if dev.write(&frame(&ENABLE_COAP)).is_err() {
        return None;
    }
    // The chip needs a moment after its CoAP client is enabled.
    std::thread::sleep(Duration::from_millis(300));
    for _ in 0..5 {
        let _ = input(dev);
    }
    if dev.write(&frame(&GET_GSI)).is_err() {
        return None;
    }

    let mut payload = String::new();
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(30));
        let Some(r) = input(dev) else { continue };
        if r.len() < 3 || r[0] != 0x07 {
            continue;
        }
        let n = usize::from(r[1]).min(r.len() - 3);
        payload.push_str(&String::from_utf8_lossy(&r[3..3 + n]));
        if let Some(level) = find_level(&payload) {
            return Some(level);
        }
    }
    None
}

/// Newest `"240":"<percent>"` of a GSI document, or None.
fn find_level(s: &str) -> Option<u8> {
    if !s.contains("\"GSI\"") {
        return None;
    }
    const KEY: &str = "\"240\":\"";
    let mut level = None;
    let mut rest = s;
    while let Some(p) = rest.find(KEY) {
        let tail = &rest[p + KEY.len()..];
        let (mut v, mut digits) = (0u32, 0usize);
        for c in tail.chars().take(4) {
            let Some(d) = c.to_digit(10) else { break };
            v = v * 10 + d;
            digits += 1;
        }
        if digits > 0 && v <= 100 && tail[digits..].starts_with('"') {
            level = Some(v as u8);
        }
        rest = tail;
    }
    level
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn race_frame_offsets() {
        let f = frame(&GET_GSI);
        assert_eq!(&f[..4], &[0x06, 0x0E, 0x00, 0x05]);
        assert_eq!(&f[3..17], &GET_GSI);
        assert!(f[17..].iter().all(|b| *b == 0));
        assert_eq!(f.len(), 62);
    }

    #[test]
    fn finds_the_level_across_split_reports() {
        // The GSI document arrives in chunks; the key can straddle a boundary.
        let first = r#"{"OR":"GSI","KVP":{"250":"0","24"#;
        let rest = r#"0":"87","241":"0"}}"#;
        assert_eq!(find_level(&format!("{first}{rest}")), Some(87));
        assert_eq!(find_level(r#"{"OR":"GSI","KVP":{"240":"100"}}"#), Some(100));
        // Out of range, no GSI marker, and a truncated value are refused.
        assert_eq!(find_level(r#"{"OR":"GSI","KVP":{"240":"101"}}"#), None);
        assert_eq!(find_level(r#"{"KVP":{"240":"87"}}"#), None);
        assert_eq!(find_level(r#"{"OR":"GSI","KVP":{"240":"8"#), None);
    }

    #[test]
    fn newest_level_wins() {
        assert_eq!(find_level(r#"{"OR":"GSI","240":"80","240":"79"}"#), Some(79));
    }
}
