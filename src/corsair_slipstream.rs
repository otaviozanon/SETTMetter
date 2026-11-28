//! Corsair wireless mice and keyboards on a SLIPSTREAM dongle (Harpoon, Dark Core, Ironclaw,
//! M65/M75, Sabre, Scimitar, K57, K70 Core TKL, K100 Air…).
//!
//! Protocol from OpenLinkHub (https://github.com/jurkovic-nikola/OpenLinkHub). Packets are
//! 64 bytes, written without a report id on the dongle's interface 1: `<endpoint> <command…>`,
//! where endpoint 0x08 is the dongle and 0x09, 0x0A… the paired devices (0x09 when only one
//! is paired). It's the same command set as the Wireless V2 headsets, with one header byte
//! less:
//!
//! - heartbeat `<ep> 12`: no reply means the device is off or asleep;
//! - battery `<ep> 02 0F`: bytes 3-4 of the reply are 1-1000, little endian.
//!
//! OpenLinkHub switches the dongle to software mode first; this module doesn't (that would
//! take the device away from iCUE), reading the battery the light way the headsets allow.
//! Not yet confirmed on hardware.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::drain_buf as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::ffi::CString;
use std::time::Duration;

const VID: u16 = 0x1B1C;
const INTERFACE: i32 = 1;
const ENDPOINTS: [u8; 2] = [0x09, 0x0A];
const WAIT: Duration = Duration::from_millis(150);
const ATTEMPTS: usize = 3;

/// SLIPSTREAM dongles (product ids from OpenLinkHub's device list).
const DONGLES: &[u16] = &[
    0x1B65, // HARPOON RGB WIRELESS dongle
    0x1B7F, // DARK CORE RGB PRO SE dongle
    0x1B81, // DARK CORE RGB PRO dongle
    0x1BA6, // SLIPSTREAM WIRELESS USB Receiver
    0x1BDC, // SLIPSTREAM WIRELESS USB Receiver
    0x2B00, // SLIPSTREAM WIRELESS USB Receiver
];

/// Corsair mouse lines, to tell mice from keyboards by the dongle's name.
const MOUSE_WORDS: &[&str] = &["HARPOON", "DARK CORE", "IRONCLAW", "SABRE", "SCIMITAR", "NIGHTSABRE", "DARKSTAR", "M55", "M65", "M75"];

struct Dongle {
    dev: HidDevice,
    pid: u16,
    name: String,
    kind: Kind,
    /// Endpoints that have answered at least once.
    seen: Vec<u8>,
}

#[derive(Default)]
pub struct CorsairSlipstream {
    open: HashMap<CString, Dongle>,
    skip: Vec<CString>,
}

impl CorsairSlipstream {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let present: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == VID && DONGLES.contains(&d.product_id()) && d.interface_number() == INTERFACE)
            .filter(|d| d.usage_page() >= 0xFF00)
            .map(|d| (d.path().to_owned(), d.product_id(), d.product_string().unwrap_or("").to_string()))
            .collect();
        self.open.retain(|path, _| present.iter().any(|(p, ..)| p == path));
        self.skip.retain(|path| present.iter().any(|(p, ..)| p == path));

        for (path, pid, product) in &present {
            if self.open.contains_key(path) || self.skip.contains(path) || self.open.values().any(|d| d.pid == *pid) {
                continue;
            }
            let Ok(dev) = api.open_path(path) else { continue };
            if !probe::descriptor(&dev).is_some_and(|d| probe::has_output(&d)) {
                self.skip.push(path.clone());
                continue;
            }
            let (name, kind) = identify(product);
            self.open.insert(path.clone(), Dongle { dev, pid: *pid, name, kind, seen: Vec::new() });
        }

        let mut out = Vec::new();
        let mut broken = Vec::new();
        for (path, d) in self.open.iter_mut() {
            for (i, ep) in ENDPOINTS.into_iter().enumerate() {
                let key = format!("corsair:{:04x}:{ep:02x}", d.pid);
                let state = match query(&d.dev, ep) {
                    Ok(Some(percent)) => {
                        if !d.seen.contains(&ep) {
                            d.seen.push(ep);
                        }
                        diag::event(&key, "battery via 02 0F");
                        State::Battery { percent, charging: false }
                    }
                    Ok(None) => {
                        diag::event(&key, "no answer (off, asleep or not paired)");
                        if !d.seen.contains(&ep) {
                            continue;
                        }
                        State::Unreachable
                    }
                    Err(e) => {
                        diag::event(&key, format!("i/o failed: {e}"));
                        broken.push(path.clone());
                        break;
                    }
                };
                let name = if i == 0 { d.name.clone() } else { format!("{} ({})", d.name, i + 1) };
                out.push(Observation { id: key, name: Some(name), kind: Some(d.kind), state });
            }
        }
        for path in broken {
            self.open.remove(&path);
        }
        out
    }
}

/// Display name and kind from the dongle's product string.
fn identify(product: &str) -> (String, Kind) {
    let upper = product.to_ascii_uppercase();
    if product.is_empty() || upper.contains("SLIPSTREAM") {
        return ("Corsair wireless device".into(), Kind::Other);
    }
    let mut name = product.trim();
    for suffix in [" Dongle", " Receiver", " USB", " Gaming"] {
        name = name.strip_suffix(suffix).unwrap_or(name);
    }
    let kind = if MOUSE_WORDS.iter().any(|w| upper.contains(w)) {
        Kind::Mouse
    } else {
        match Kind::from_name(name) {
            Kind::Other if upper.starts_with("CORSAIR K") || upper.starts_with('K') => Kind::Keyboard,
            k => k,
        }
    };
    (name.to_string(), kind)
}

/// Battery percentage of the device on `ep`, or None when it doesn't answer.
fn query(dev: &HidDevice, ep: u8) -> hidapi::HidResult<Option<u8>> {
    let mut buf = [0u8; 64];
    flush(dev, &mut buf)?;
    send(dev, &[ep, 0x12])?; // heartbeat
    if dev.read_timeout(&mut buf, WAIT.as_millis() as i32)? == 0 {
        return Ok(None);
    }
    flush(dev, &mut buf)?;
    for _ in 0..ATTEMPTS {
        send(dev, &[ep, 0x02, 0x0F])?;
        let n = dev.read_timeout(&mut buf, WAIT.as_millis() as i32)?;
        if let Some(p) = parse(&buf[..n]) {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

fn parse(r: &[u8]) -> Option<u8> {
    let raw = u16::from_le_bytes([*r.get(3)?, *r.get(4)?]);
    (1..=1000).contains(&raw).then_some((raw / 10) as u8)
}

/// A 64-byte report without an id: hidapi wants a leading 0.
fn send(dev: &HidDevice, data: &[u8]) -> hidapi::HidResult<()> {
    let mut r = [0u8; 65];
    r[1..1 + data.len()].copy_from_slice(data);
    dev.write(&r).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_battery() {
        assert_eq!(parse(&[0x00, 0x09, 0x02, 0xE8, 0x03]), Some(100)); // 1000
        assert_eq!(parse(&[0x00, 0x09, 0x02, 0xDF, 0x02]), Some(73)); // 735
        assert_eq!(parse(&[0x00, 0x09, 0x02, 0x00, 0x00]), None);
        assert_eq!(parse(&[0x00, 0x09]), None);
    }

    #[test]
    fn names_and_kinds() {
        assert_eq!(identify("CORSAIR HARPOON RGB WIRELESS Dongle"), ("CORSAIR HARPOON RGB WIRELESS".into(), Kind::Mouse));
        assert_eq!(identify("DARK CORE RGB PRO Gaming Dongle"), ("DARK CORE RGB PRO".into(), Kind::Mouse));
        assert_eq!(identify("SLIPSTREAM WIRELESS USB Receiver"), ("Corsair wireless device".into(), Kind::Other));
        assert_eq!(identify("CORSAIR K57 RGB WIRELESS Gaming Dongle").1, Kind::Keyboard);
    }
}
