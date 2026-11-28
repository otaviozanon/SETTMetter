//! GameSir Tarantula Pro / NovaPro (Super Nova, T4NPro): battery over the SAPJoy "device info"
//! frame.
//!
//! These pads look like a wired Xbox controller to XInput, so the level comes from their vendor
//! collection (usage page 0xFFA0, output report 0x07 / 32 bytes, input report 0x06 / 64 bytes,
//! no feature channel) using the read-only GetDeviceInfo query GameSir's own client sends:
//!   request 07 03 0A 01, zero padded to 32 bytes
//!   reply   [0]=06 [1]=len [2]=0A [3]=01 [4]=id [5..6]=firmware [7..12]=MAC
//!           [13]=battery (0..100 percent, FF = charging) [14]=type
//! 3537:103E is shared by the Tarantula Pro and the NovaPro on their receivers and both answer
//! the same frame, so the product string names the device. 3537:100E is the NovaPro on a cable.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/gamesir_sapjoy.cpp and
//! gamesir_sapjoy_protocol.cpp), captured from GameSir's client.

use crate::device::{Kind, Observation, State};
use crate::{diag, probe};
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x3537;
const OUTPUT_REPORT_ID: u8 = 0x07;
const INPUT_REPORT_ID: u8 = 0x06;
const CMD_DEVICE_INFO: u8 = 0x0A;
const SUB_DEVICE_INFO: u8 = 0x01;
const OUTPUT_REPORT_LEN: usize = 32;
const INPUT_REPORT_LEN: usize = 64;

/// Cable and receiver ids that answer the SAPJoy frame. Nova / Nova 2 / Nova 2 Lite (0x1098,
/// 0x100F, 0x1040) and Cyclone 2 (0x100B) have their own drivers.
const PRODUCTS: &[(u16, &str)] = &[(0x100E, "USB cable"), (0x103E, "2.4 GHz receiver"), (0x1050, "2.4 GHz receiver")];

#[derive(Default)]
pub struct GameSirSapjoy {
    seen: Vec<(String, String)>,
    /// Last level per device, kept while a charging reply carries none.
    levels: Vec<(String, u8)>,
}

#[derive(Default)]
struct Decoded {
    address: [u8; 6],
    percent: Option<u8>,
    charging: bool,
    answered: bool,
}

impl GameSirSapjoy {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out: Vec<Observation> = Vec::new();
        for &(pid, via) in PRODUCTS {
            // A cable reading wins over the same pad's receiver: it is polled first.
            for info in api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0xFFA0) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, OUTPUT_REPORT_ID) != Some(OUTPUT_REPORT_LEN) || probe::input_len(&desc, INPUT_REPORT_ID) != Some(INPUT_REPORT_LEN) {
                    continue;
                }
                // Upstream also wants this collection to have no feature channel; the page and
                // the exact 32/64-byte report pair already select the vendor collection.
                let Some(decoded) = query(&dev) else { continue };
                let key = identity(&decoded.address, pid);
                if out.iter().any(|o| o.id == key) {
                    break;
                }
                let name = info.product_string().filter(|n| !n.trim().is_empty()).unwrap_or("GameSir controller").to_string();
                // While charging the pad sends no level; the last one is kept next to the bolt.
                let percent = decoded.percent.or_else(|| self.levels.iter().rev().find(|(k, _)| *k == key).map(|(_, p)| *p));
                if let Some(percent) = decoded.percent {
                    self.levels.retain(|(k, _)| *k != key);
                    self.levels.push((key.clone(), percent));
                }
                let state = match percent {
                    Some(percent) => State::Battery { percent, charging: decoded.charging },
                    None => State::Unreachable,
                };
                let level = match percent {
                    Some(p) => format!("{p}%{}", if decoded.charging { " charging" } else { "" }),
                    None => "no level".to_string(),
                };
                diag::event(&format!("gamesir-sapjoy#{}", tag(&decoded.address)), format!("{via}: {level}"));
                self.seen.push((key.clone(), name.clone()));
                out.push(Observation { id: key, name: Some(name), kind: Some(Kind::Gamepad), state });
                break;
            }
        }
        for (key, name) in &self.seen {
            if !out.iter().any(|o| o.id == *key) {
                out.push(Observation { id: key.clone(), name: Some(name.clone()), kind: Some(Kind::Gamepad), state: State::Unreachable });
            }
        }
        out
    }
}

fn identity(address: &[u8; 6], pid: u16) -> String {
    if address.iter().all(|b| *b == 0) || address.iter().all(|b| *b == 0xFF) {
        return format!("gamesir:{pid:04x}");
    }
    let mac: String = address.iter().map(|b| format!("{b:02x}")).collect();
    format!("gamesir:sapjoy:{mac}")
}

/// Diagnostics never carry the address itself.
fn tag(address: &[u8; 6]) -> String {
    let mut bytes = [0u8; 8];
    bytes[2..].copy_from_slice(address);
    diag::bt_tag(u64::from_be_bytes(bytes))
}

/// Sends GetDeviceInfo and waits up to 900 ms for a reply. The pad also streams state frames on
/// the same collection, so every header field is checked.
fn query(dev: &HidDevice) -> Option<Decoded> {
    let mut buf = [0u8; INPUT_REPORT_LEN];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    let mut req = [0u8; OUTPUT_REPORT_LEN];
    req[..4].copy_from_slice(&[OUTPUT_REPORT_ID, 0x03, CMD_DEVICE_INFO, SUB_DEVICE_INFO]);
    dev.write(&req).ok()?;
    let deadline = Instant::now() + Duration::from_millis(900);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis().min(200).max(1) as i32) {
            Ok(n) if n >= 15 => {
                let decoded = decode(&buf[..n]);
                if decoded.answered {
                    return Some(decoded);
                }
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

fn decode(reply: &[u8]) -> Decoded {
    let mut r = Decoded::default();
    if reply.len() < 15 || reply[0] != INPUT_REPORT_ID || reply[2] != CMD_DEVICE_INFO || reply[3] != SUB_DEVICE_INFO {
        return r;
    }
    if reply[1] < 14 || usize::from(reply[1]) + 1 > reply.len() {
        return r;
    }
    r.address.copy_from_slice(&reply[7..13]);
    r.answered = true;
    if reply[13] == 0xFF {
        r.charging = true;
    } else {
        // The vendor app clamps anything above 100.
        r.percent = Some(reply[13].min(100));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured GetDeviceInfo reply: 0x60 = 96%, as GameSir's own UI showed.
    fn frame() -> Vec<u8> {
        let mut r = vec![0u8; INPUT_REPORT_LEN];
        r[..15].copy_from_slice(&[0x06, 0x0E, 0x0A, 0x01, 0x02, 0x01, 0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x60, 0x03]);
        r
    }

    #[test]
    fn decodes_level_and_address() {
        let r = decode(&frame());
        assert!(r.answered);
        assert_eq!(r.percent, Some(96));
        assert!(!r.charging);
        assert_eq!(r.address, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(identity(&r.address, 0x103E), "gamesir:sapjoy:aabbccddeeff");
    }

    #[test]
    fn charging_and_clamping() {
        let mut f = frame();
        f[13] = 0xFF;
        let r = decode(&f);
        assert!(r.answered && r.charging && r.percent.is_none());

        let mut f = frame();
        f[13] = 0xC8; // 200: the vendor app clamps
        assert_eq!(decode(&f).percent, Some(100));
    }

    #[test]
    fn rejects_state_frames_and_bad_lengths() {
        let mut f = frame();
        f[0] = 0x05;
        assert!(!decode(&f).answered);
        let mut f = frame();
        f[1] = 13; // len shorter than the fields we read
        assert!(!decode(&f).answered);
        assert!(!decode(&f[..14]).answered);
        f[1] = 63; // len claims more than the buffer holds
        assert!(!decode(&f[..15]).answered);
    }

    #[test]
    fn blank_addresses_share_the_pid_key() {
        assert_eq!(identity(&[0; 6], 0x100E), "gamesir:100e");
        assert_eq!(identity(&[0xFF; 6], 0x100E), "gamesir:100e");
    }
}
