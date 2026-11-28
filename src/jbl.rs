//! JBL Quantum wireless headsets, two read-only protocols.
//!
//! **HIDV1** (feature reports, one payload byte each): GET 68 = wireless link (`01` = headset
//! connected), GET 49 = battery percent. Quantum 350 (0ECB:206B, interface 3), Quantum 610
//! (0ECB:205C), Quantum 360 (0ECB:2115) and Quantum 810 (0ECB:2069), all on interface 5. The
//! firmware answers an unimplemented report id with the previous implemented one, so the reply's
//! first byte must match the id asked for.
//!
//! **HIDV3** (vendor collection, usage page `FF12`, 64-byte reports): request on feature `0x88`,
//! reply on input `0x86`:
//!
//! ```text
//! request: [88][00 DD][01 00][01][00][02 00][feature lo][feature hi]
//! reply:   [86][00 DD][01 00][01][00][len lo][len hi][..][status ...]
//!          [feature lo][feature hi][size lo][size hi][value...]
//! ```
//!
//! Battery = feature `000D`, one byte: bit 7 charging, bits 0..6 percent.
//!
//! From JBL QuantumENGINE 2.9.4 and the quantum-linux captures; checked against a Quantum 650
//! Wireless on its 2.4 GHz dongle. Ported from jbl.cpp.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x0ECB;
const BATTERY_FEATURE: u16 = 0x000D;
const FEATURE_REPORT: u8 = 0x88;
const INPUT_REPORT: u8 = 0x86;

struct Model {
    pid: u16,
    name: &'static str,
    #[allow(dead_code)]
    dongle: bool,
}

/// HIDV3 headsets (Quantum 520/650/850/950 and X variants).
const MODELS: &[Model] = &[
    Model { pid: 0x2125, name: "JBL Quantum 650 Wireless", dongle: true },
    Model { pid: 0x2126, name: "JBL Quantum 650 Wireless", dongle: false },
    Model { pid: 0x213B, name: "JBL Quantum 650X Wireless", dongle: true },
    Model { pid: 0x213D, name: "JBL Quantum 650X Wireless", dongle: false },
    Model { pid: 0x2127, name: "JBL Quantum 950 Wireless", dongle: true },
    Model { pid: 0x2128, name: "JBL Quantum 950 Wireless", dongle: false },
    Model { pid: 0x213E, name: "JBL Quantum 950X Wireless", dongle: true },
    Model { pid: 0x2140, name: "JBL Quantum 950X Wireless", dongle: false },
    Model { pid: 0x218D, name: "JBL Quantum 520 Wireless", dongle: true },
    Model { pid: 0x218E, name: "JBL Quantum 520 Wireless", dongle: false },
    Model { pid: 0x21A7, name: "JBL Quantum 520X Wireless", dongle: true },
    Model { pid: 0x21A9, name: "JBL Quantum 520X Wireless", dongle: false },
    Model { pid: 0x219A, name: "JBL Quantum 850 Wireless", dongle: true },
    Model { pid: 0x219B, name: "JBL Quantum 850 Wireless", dongle: false },
];

/// HIDV1 headsets: `page` 0 = any collection on that interface.
struct Hidv1 {
    pid: u16,
    iface: i32,
    page: u16,
    name: &'static str,
}

const HIDV1: &[Hidv1] = &[
    Hidv1 { pid: 0x206B, iface: 3, page: 0x0001, name: "JBL Quantum 350 Wireless" },
    Hidv1 { pid: 0x205C, iface: 5, page: 0, name: "JBL Quantum 610 Wireless" },
    Hidv1 { pid: 0x2115, iface: 5, page: 0x0001, name: "JBL Quantum 360 Wireless" },
    Hidv1 { pid: 0x2069, iface: 5, page: 0, name: "JBL Quantum 810 Wireless" }, // FF13:0001
];

#[derive(Default)]
pub struct Jbl;

impl Jbl {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();

        for model in HIDV1 {
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == model.pid
                    && d.interface_number() == model.iface
                    && (model.page == 0 || (d.usage_page() == model.page && d.usage() == 0))
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::feature_len(&desc, 0x68).unwrap_or(0) < 2 || probe::feature_len(&desc, 0x49).unwrap_or(0) < 2 {
                    continue;
                }
                drain(&dev);
                let Some(1) = get_feature_byte(&dev, 0x68) else { continue };
                let Some(percent) = get_feature_byte(&dev, 0x49).filter(|p| *p <= 100) else { continue };
                diag::event(&format!("jbl:{:04x}", model.pid), format!("HIDV1 link up, {percent}%"));
                out.push(observation(model.pid, model.name, percent, false));
                break;
            }
        }

        for model in MODELS {
            for info in api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == model.pid && d.usage_page() == 0xFF12) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                let feature = probe::feature_len(&desc, FEATURE_REPORT).unwrap_or(0);
                let input = probe::input_len(&desc, INPUT_REPORT).unwrap_or(0);
                if feature < 11 || input < 16 {
                    continue;
                }
                let Some(info) = get_battery(&dev, feature, input) else { continue };
                let percent = info & 0x7F;
                diag::event(
                    &format!("jbl:{:04x}", model.pid),
                    format!("HIDV3 battery {percent}%{}", if info & 0x80 != 0 { ", charging" } else { "" }),
                );
                out.push(observation(model.pid, model.name, percent, info & 0x80 != 0));
                break;
            }
        }
        out
    }
}

fn observation(pid: u16, name: &str, percent: u8, charging: bool) -> Observation {
    Observation { id: format!("jbl:{pid:04x}"), name: Some(name.into()), kind: Some(Kind::Headset), state: State::Battery { percent, charging } }
}

/// GET feature report `id`: the reply must repeat the id, and carries one payload byte.
fn get_feature_byte(dev: &HidDevice, id: u8) -> Option<u8> {
    let mut buf = [0u8; 64];
    buf[0] = id;
    let n = dev.get_feature_report(&mut buf).ok()?;
    (n >= 2 && buf[0] == id).then_some(buf[1])
}

/// The battery feature value, or None: a switched-off headset behind its dongle does not answer.
fn get_battery(dev: &HidDevice, feature_len: usize, input_len: usize) -> Option<u8> {
    drain(dev);
    let mut request = vec![0u8; feature_len];
    request[..9].copy_from_slice(&[FEATURE_REPORT, 0x00, 0xDD, 0x01, 0x00, 0x01, 0x00, 0x02, 0x00]);
    request[9..11].copy_from_slice(&BATTERY_FEATURE.to_le_bytes());
    dev.send_feature_report(&request).ok()?;

    let deadline = Instant::now() + Duration::from_millis(800);
    let mut buf = vec![0u8; input_len.max(16)];
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        if dev.read_timeout(&mut buf, left.as_millis().max(1) as i32).ok()? < 16 {
            return None;
        }
        if let Some(info) = parse_reply(&buf) {
            return Some(info);
        }
    }
}

fn parse_reply(r: &[u8]) -> Option<u8> {
    // Only the reply to "get device info" sent to the device itself.
    if r.len() < 16 || r[0] != INPUT_REPORT || r[1] != 0x00 || r[2] != 0xDD || r[3] != 0x01 || r[4] != 0x00 || r[5] != 0x01 {
        return None;
    }
    if r[10] & 0x80 != 0 {
        return None; // error status (bit 15)
    }
    if r[11] != BATTERY_FEATURE as u8 || r[12] != (BATTERY_FEATURE >> 8) as u8 {
        return None;
    }
    if r[13] == 0 && r[14] == 0 {
        return None; // empty feature value
    }
    ((r[15] & 0x7F) <= 100).then_some(r[15])
}

fn drain(dev: &HidDevice) {
    let mut buf = [0u8; 128];
    while dev.read_timeout(&mut buf, 0).is_ok_and(|n| n > 0) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(feature: u16, value: u8) -> [u8; 16] {
        let mut r = [0u8; 16];
        r[..6].copy_from_slice(&[INPUT_REPORT, 0x00, 0xDD, 0x01, 0x00, 0x01]);
        r[11] = feature as u8;
        r[12] = (feature >> 8) as u8;
        r[13] = 0x01;
        r[15] = value;
        r
    }

    #[test]
    fn hidv3_battery_byte() {
        assert_eq!(parse_reply(&reply(BATTERY_FEATURE, 75)), Some(75));
        // Bit 7 is the charging flag; the level is the low 7 bits.
        assert_eq!(parse_reply(&reply(BATTERY_FEATURE, 0x80 | 40)), Some(0x80 | 40));
        assert_eq!((0x80 | 40) & 0x7F, 40);
        // A different feature in the answer is not the battery.
        assert_eq!(parse_reply(&reply(0x0001, 75)), None);
        // Error status bit 15.
        let mut r = reply(BATTERY_FEATURE, 75);
        r[10] = 0x80;
        assert_eq!(parse_reply(&r), None);
        // Empty feature value.
        let mut r = reply(BATTERY_FEATURE, 0);
        r[13] = 0;
        r[14] = 0;
        assert_eq!(parse_reply(&r), None);
    }

    #[test]
    fn model_tables_are_consistent() {
        assert!(MODELS.iter().any(|m| m.pid == 0x2125 && m.dongle));
        assert!(HIDV1.iter().any(|m| m.pid == 0x206B && m.iface == 3));
    }
}
