//! Razer Barracuda Pro on its 2.4 GHz receiver (1532:053A), part of the Razer "PA" protocol
//! family. Layout read off a USB capture of Razer Synapse refreshing the battery page; ported
//! from HaloBattery (github.com/HeyOkay/HaloBattery, providers/barracuda.py, MIT, tested there
//! on a real Barracuda Pro).
//!
//! ```text
//! query:  [01][80][08]['P']['A'][08][08][03][cmd]            (64 bytes, report id 01)
//! remote: [01][80][07]['P']['A'][0E][08][02][E1][1 on / 0 off]
//! reply:  [01][80][len]['P']['I'][08][..][13] command echo, [14] 01/02,
//!         [15] length, [16..] data
//! ```
//!
//! cmd 21 = battery percent, cmd 2A = charging. Remote mode is switched on for one query and
//! off again, as Synapse does; it is not stored on the headset. Only those frames are sent.
//!
//! The Barracuda X (2022) and BlackShark headsets are in `crate::razer_headset`.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::Duration;

const VID: u16 = 0x1532;
const PID: u16 = 0x053A;
const KEY: &str = "razer:053a";
const NAME: &str = "Razer Barracuda Pro";

#[derive(Default)]
pub struct BarracudaPro {}

impl BarracudaPro {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let found = api.device_list().find(|d| d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == 0xFF00);
        let Some(info) = found else { return Vec::new() };
        let Ok(dev) = api.open_path(info.path()) else {
            diag::event(KEY, "open failed");
            return Vec::new();
        };
        let Some(desc) = probe::descriptor(&dev) else { return Vec::new() };
        if probe::output_len(&desc, 0x01).unwrap_or(0) < 64 {
            return Vec::new();
        }

        let mut remote = false;
        for attempt in 0..3 {
            drain(&dev);
            remote = dev.write(&remote_frame(true)).is_ok();
            if remote {
                break;
            }
            std::thread::sleep(Duration::from_millis(150 * (attempt + 1)));
        }

        let (mut level, mut charging) = (None, None);
        if remote {
            std::thread::sleep(Duration::from_millis(50));
            level = ask(&dev, 0x21);
            if level.is_some() {
                charging = ask(&dev, 0x2A);
            }
            drain(&dev);
            let _ = dev.write(&remote_frame(false));
        }

        let state = match level {
            Some(level) if level <= 100 => {
                let charging = charging.is_some_and(|v| v > 0);
                diag::event(KEY, format!("battery via PA GET 21: {level}%"));
                State::Battery { percent: level, charging }
            }
            _ => {
                // Receiver present, headset off or out of range.
                diag::event(KEY, "no battery reply (headset off or out of range)");
                State::Off
            }
        };
        vec![Observation { id: KEY.into(), name: Some(NAME.into()), kind: Some(Kind::Headset), state }]
    }
}

fn query_frame(cmd: u8) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[..9].copy_from_slice(&[0x01, 0x80, 0x08, b'P', b'A', 0x08, 0x08, 0x03, cmd]);
    b
}

fn remote_frame(on: bool) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[..10].copy_from_slice(&[0x01, 0x80, 0x07, b'P', b'A', 0x0E, 0x08, 0x02, 0xE1, u8::from(on)]);
    b
}

/// The first data byte of the reply to `cmd`, or None.
fn ask(dev: &HidDevice, cmd: u8) -> Option<u8> {
    for _ in 0..4 {
        drain(dev);
        if dev.write(&query_frame(cmd)).is_err() {
            return None;
        }
        let mut buf = [0u8; 1024];
        for _ in 0..6 {
            let n = match dev.read_timeout(&mut buf, 150) {
                Ok(0) => continue,
                Ok(n) => n,
                Err(_) => return None,
            };
            if let Some(value) = parse_reply(&buf[..n], cmd) {
                return Some(value);
            }
        }
    }
    None
}

fn parse_reply(r: &[u8], cmd: u8) -> Option<u8> {
    if r.len() < 17 || r[13] != cmd || !matches!(r[14], 0x01 | 0x02) || r[15] == 0 || r[15] > 32 {
        return None;
    }
    Some(r[16])
}

fn drain(dev: &HidDevice) {
    let mut stale = [0u8; 1024];
    for _ in 0..8 {
        match dev.read_timeout(&mut stale, 20) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_and_remote_frames() {
        // [01][80][08]['P']['A'][08][08][03][21]
        let q = query_frame(0x21);
        assert_eq!(&q[..3], &[0x01, 0x80, 0x08]);
        assert_eq!(&q[3..5], b"PA");
        assert_eq!(&q[5..9], &[0x08, 0x08, 0x03, 0x21]);
        // [01][80][07]['P']['A'][0E][08][02][E1][1]
        let r = remote_frame(true);
        assert_eq!(&r[3..5], b"PA");
        assert_eq!(&r[5..10], &[0x0E, 0x08, 0x02, 0xE1, 0x01]);
        assert_eq!(remote_frame(false)[9], 0);
    }

    #[test]
    fn parses_the_battery_reply() {
        // [01][80][len]['P']['I'][08]..[13]=cmd echo [14]=01 [15]=len [16]=55
        let mut r = [0u8; 64];
        r[..6].copy_from_slice(&[0x01, 0x80, 0x0C, b'P', b'I', 0x08]);
        r[13] = 0x21;
        r[14] = 0x01;
        r[15] = 0x01;
        r[16] = 55;
        assert_eq!(parse_reply(&r, 0x21), Some(55));
        // Wrong command echo, unsupported status or impossible length.
        assert_eq!(parse_reply(&r, 0x2A), None);
        r[14] = 0x03;
        assert_eq!(parse_reply(&r, 0x21), None);
        r[14] = 0x02;
        r[15] = 0x21; // 33 bytes: too long for this reply
        assert_eq!(parse_reply(&r, 0x21), None);
        // Short frames are not replies.
        assert_eq!(parse_reply(&r[..16], 0x21), None);
    }
}
