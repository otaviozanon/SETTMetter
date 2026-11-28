//! Corsair wireless headsets over their dongle. Both protocols are as documented by
//! HeadsetControl.
//!
//! **VOID family** (`corsair_void_rich`: VOID, VOID PRO, VOID RGB ELITE, HS70…): on the
//! collection with usage page 0xFFC5 (interface 3), write report `C9 64`; the reply is
//! `64 00 <level> B1 <status>`, level 0-100 with bit 7 = mic up, status 0 = headset off,
//! 1 normal, 2 low, 4/5 charging.
//!
//! **Wireless V2** (`corsair_void_v2w`: VOID WIRELESS V2, VIRTUOSO MAX, HS80 MAX). Packets
//! are 64 bytes on interface 4, written without a report id: `02 <endpoint> <op> <command> …`, where
//! endpoint 0x08 is the dongle and 0x09 the headset.
//!
//! 1. Wake: ask the dongle for its firmware (`02 08 02 13`) and heartbeat it (`02 08 02 12`),
//!    then heartbeat the headset (`02 09 02 12`). No answer means the headset is off. This
//!    is the light handshake that doesn't switch the headset to software mode (which pops).
//! 2. Battery: `02 09 02 0F`; bytes 4-5 of the reply are 1-1000, little endian. The dongle
//!    sometimes answers with something else (the paired headset's id), so out-of-range
//!    replies are retried.
//!
//! No charging flag is known for these.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::drain_buf as flush;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::ffi::CString;
use std::time::Duration;

const VID: u16 = 0x1B1C;
const V2_INTERFACE: i32 = 4;
const VOID_USAGE_PAGE: u16 = 0xFFC5;
const DONGLE: u8 = 0x08;
const HEADSET: u8 = 0x09;
const WAIT: Duration = Duration::from_millis(500);
const ATTEMPTS: usize = 3;

/// VOID-family dongles (and some wired headsets, which are skipped by name).
const VOID_PIDS: &[u16] = &[
    0x0A14, 0x0A16, 0x0A17, 0x0A1A, 0x0A1D, 0x0A2B, 0x0A38, 0x0A4F, 0x0A51, 0x0A52, 0x0A55, 0x0A56, 0x0A75,
    0x1B23, 0x1B27, 0x1B29, 0x1B2A,
];

/// Wireless V2 dongles: (product id, name)
const V2_DEVICES: &[(u16, &str)] = &[
    (0x2A08, "Corsair VOID WIRELESS V2"),
    (0x2A02, "Corsair VIRTUOSO MAX WIRELESS"),
    (0x0A97, "Corsair HS80 MAX WIRELESS"),
    (0x0A64, "Corsair HS80 RGB Wireless"),
    (0x0A62, "Corsair wireless headset"),
    (0x0A3E, "Corsair wireless headset"),
    (0x0A3D, "Corsair wireless headset"),
];

#[derive(Clone, Copy, PartialEq)]
enum Protocol {
    Void,
    V2,
}

struct Dongle {
    dev: HidDevice,
    key: String,
    name: String,
    protocol: Protocol,
}

#[derive(Default)]
pub struct Corsair {
    open: HashMap<CString, Dongle>,
}

impl Corsair {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        // One collection per dongle.
        let mut present: Vec<(CString, u16, String, Protocol, bool)> = Vec::new();
        for d in api.device_list().filter(|d| d.vendor_id() == VID) {
            let pid = d.product_id();
            if let Some((_, name)) = V2_DEVICES.iter().find(|(p, _)| *p == pid) {
                if d.interface_number() != V2_INTERFACE {
                    continue;
                }
                // The vendor-page collection on interface 4, if there are several.
                let vendor = d.usage_page() >= 0xFF00;
                let entry = (d.path().to_owned(), pid, name.to_string(), Protocol::V2, vendor);
                match present.iter_mut().find(|e| e.1 == pid) {
                    Some(slot) if vendor && !slot.4 => *slot = entry,
                    Some(_) => {}
                    None => present.push(entry),
                }
            } else if VOID_PIDS.contains(&pid) && d.usage_page() == VOID_USAGE_PAGE {
                let product = d.product_string().unwrap_or("");
                if !product.to_ascii_lowercase().contains("wireless") {
                    diag::event(&format!("corsair:{pid:04x}"), format!("skipped: {product:?} looks wired"));
                    continue;
                }
                present.push((d.path().to_owned(), pid, void_name(product), Protocol::Void, true));
            }
        }
        self.open.retain(|path, _| present.iter().any(|e| &e.0 == path));

        let mut out = Vec::new();
        for (path, pid, name, protocol, _) in present {
            if !self.open.contains_key(&path) {
                let key = format!("corsair:{pid:04x}");
                match api.open_path(&path) {
                    Ok(dev) => {
                        self.open.insert(path.clone(), Dongle { dev, key, name, protocol });
                    }
                    Err(e) => {
                        diag::event(&key, format!("open failed: {e}"));
                        continue;
                    }
                }
            }
            let d = &self.open[&path];
            let reading = match d.protocol {
                Protocol::V2 => query(&d.dev),
                Protocol::Void => query_void(&d.dev),
            };
            let state = match reading {
                Ok(Reading::Off) => {
                    diag::event(&d.key, "headset off (no answer, or status 0)");
                    State::Off
                }
                Ok(Reading::Percent(percent, charging)) => {
                    diag::event(&d.key, "battery reply");
                    State::Battery { percent, charging }
                }
                Ok(Reading::Invalid(raw)) => {
                    diag::event(&d.key, format!("no plausible battery value (last: {raw})"));
                    State::Unreachable
                }
                Err(e) => {
                    // Unplugged or the handle went bad: reopen on the next scan.
                    diag::event(&d.key, format!("i/o failed: {e}"));
                    self.open.remove(&path);
                    continue;
                }
            };
            out.push(Observation { id: d.key.clone(), name: Some(d.name.clone()), kind: Some(Kind::Headset), state });
        }
        out
    }
}

enum Reading {
    Off,
    Percent(u8, bool),
    /// The last raw 1-1000 value, which was out of range.
    Invalid(u16),
}

fn query(dev: &HidDevice) -> hidapi::HidResult<Reading> {
    let mut buf = [0u8; 64];
    flush(dev, &mut buf)?;
    send(dev, DONGLE, 0x13)?; // dongle firmware
    send(dev, DONGLE, 0x12)?; // dongle heartbeat
    flush(dev, &mut buf)?;
    send(dev, HEADSET, 0x12)?; // headset heartbeat
    if dev.read_timeout(&mut buf, WAIT.as_millis() as i32)? == 0 {
        return Ok(Reading::Off);
    }
    flush(dev, &mut buf)?;

    let mut raw = 0;
    for _ in 0..ATTEMPTS {
        send(dev, HEADSET, 0x0F)?;
        let n = dev.read_timeout(&mut buf, WAIT.as_millis() as i32)?;
        raw = if n >= 6 { u16::from_le_bytes([buf[4], buf[5]]) } else { 0 };
        if let Some(p) = percent(raw) {
            return Ok(Reading::Percent(p, false));
        }
    }
    Ok(Reading::Invalid(raw))
}

fn query_void(dev: &HidDevice) -> hidapi::HidResult<Reading> {
    let mut buf = [0u8; 64];
    flush(dev, &mut buf)?;
    dev.write(&[0xC9, 0x64])?;
    let deadline = std::time::Instant::now() + WAIT;
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        let n = dev.read_timeout(&mut buf, left.as_millis() as i32)?;
        if n == 0 {
            break;
        }
        if let Some(r) = parse_void(&buf[..n]) {
            return Ok(r);
        }
    }
    Ok(Reading::Invalid(0))
}

fn parse_void(r: &[u8]) -> Option<Reading> {
    let [0x64, _, level, _, status, ..] = *r else { return None };
    let level = level & 0x7F; // bit 7: mic up
    match status {
        0 => Some(Reading::Off),
        1 | 2 if level <= 100 => Some(Reading::Percent(level, false)),
        4 | 5 if level <= 100 => Some(Reading::Percent(level, true)),
        _ => None,
    }
}

/// "CORSAIR VOID PRO Wireless Gaming Dongle" -> "CORSAIR VOID PRO Wireless"
fn void_name(product: &str) -> String {
    let mut name = product.trim();
    for suffix in [" Dongle", " Receiver", " Headset", " Gaming"] {
        name = name.strip_suffix(suffix).unwrap_or(name);
    }
    if name.is_empty() { "Corsair headset".into() } else { name.into() }
}

/// Writes `02 <endpoint> 02 <command>` (a read request) as a 64-byte report without an id.
fn send(dev: &HidDevice, endpoint: u8, command: u8) -> hidapi::HidResult<()> {
    let mut r = [0u8; 65];
    r[1..5].copy_from_slice(&[0x02, endpoint, 0x02, command]);
    dev.write(&r).map(|_| ())
}

fn percent(raw: u16) -> Option<u8> {
    (1..=1000).contains(&raw).then_some((raw / 10) as u8)
}

#[cfg(test)]
mod tests {
    use super::{Reading, parse_void, void_name};

    #[test]
    fn parses_void_status() {
        assert!(matches!(parse_void(&[0x64, 0, 0x80 | 73, 0xB1, 1]), Some(Reading::Percent(73, false))));
        assert!(matches!(parse_void(&[0x64, 0, 40, 0xB1, 5]), Some(Reading::Percent(40, true))));
        assert!(matches!(parse_void(&[0x64, 0, 0, 0xB1, 0]), Some(Reading::Off)));
        assert!(parse_void(&[0x65, 0, 40, 0xB1, 1]).is_none()); // another report
        assert!(parse_void(&[0x64, 0, 40]).is_none());
    }

    #[test]
    fn void_names() {
        assert_eq!(void_name("CORSAIR VOID PRO Wireless Gaming Dongle"), "CORSAIR VOID PRO Wireless");
        assert_eq!(void_name("CORSAIR HS70 Pro Wireless Gaming Headset"), "CORSAIR HS70 Pro Wireless");
        assert_eq!(void_name(""), "Corsair headset");
    }

    #[test]
    fn percent() {
        assert_eq!(super::percent(1000), Some(100));
        assert_eq!(super::percent(735), Some(73));
        assert_eq!(super::percent(1), Some(0));
        assert_eq!(super::percent(0), None);
        assert_eq!(super::percent(0x0A97), None); // a headset id instead of a level
    }
}
