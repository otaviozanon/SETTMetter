//! SteelSeries wireless mice, over their 2.4 GHz dongle or a cable.
//!
//! Protocol from rivalcfg (https://github.com/flozz/rivalcfg). Commands are output reports
//! (report id 0) on a vendor collection; the reply is the next input report.
//!
//! - Aerox 3/5/9 Wireless and Prime (Mini) Wireless: command 0x92 on a cable, 0xD2 (with the
//!   wireless flag 0x40) through the dongle. Reply byte 1: bit 7 charging, the rest a level
//!   of 1-21 in 5% steps.
//! - Rival 3 Wireless (Gen 2) and Rival 650 Wireless: command `AA 01`. Reply byte 0 is the
//!   percentage, byte 2 the charging flag.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::ffi::CString;
use std::time::{Duration, Instant};

const VID: u16 = 0x1038;
const WAIT: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Protocol {
    /// Aerox / Prime, cable or dongle.
    Aerox { wireless: bool },
    Rival,
}

/// (USB product id, name, interface, protocol). The dongle and the cable share a name, so
/// they show as one device.
const MODELS: &[(u16, &str, i32, Protocol)] = &[
    (0x1838, "SteelSeries Aerox 3 Wireless", 3, Protocol::Aerox { wireless: true }),
    (0x1878, "SteelSeries Aerox 3 Wireless", 3, Protocol::Aerox { wireless: true }), // CS2 Dragon Lore
    (0x183A, "SteelSeries Aerox 3 Wireless", 3, Protocol::Aerox { wireless: false }),
    (0x187A, "SteelSeries Aerox 3 Wireless", 3, Protocol::Aerox { wireless: false }),
    (0x1852, "SteelSeries Aerox 5 Wireless", 3, Protocol::Aerox { wireless: true }),
    (0x185C, "SteelSeries Aerox 5 Wireless", 3, Protocol::Aerox { wireless: true }), // Destiny 2
    (0x1860, "SteelSeries Aerox 5 Wireless", 3, Protocol::Aerox { wireless: true }), // Diablo IV
    (0x1854, "SteelSeries Aerox 5 Wireless", 3, Protocol::Aerox { wireless: false }),
    (0x185E, "SteelSeries Aerox 5 Wireless", 3, Protocol::Aerox { wireless: false }),
    (0x1862, "SteelSeries Aerox 5 Wireless", 3, Protocol::Aerox { wireless: false }),
    (0x1858, "SteelSeries Aerox 9 Wireless", 3, Protocol::Aerox { wireless: true }),
    (0x1874, "SteelSeries Aerox 9 Wireless", 3, Protocol::Aerox { wireless: true }), // WOW
    (0x185A, "SteelSeries Aerox 9 Wireless", 3, Protocol::Aerox { wireless: false }),
    (0x1876, "SteelSeries Aerox 9 Wireless", 3, Protocol::Aerox { wireless: false }),
    (0x1840, "SteelSeries Prime Wireless", 3, Protocol::Aerox { wireless: true }),
    (0x1842, "SteelSeries Prime Wireless", 3, Protocol::Aerox { wireless: false }),
    (0x1848, "SteelSeries Prime Mini Wireless", 3, Protocol::Aerox { wireless: true }),
    (0x184A, "SteelSeries Prime Mini Wireless", 3, Protocol::Aerox { wireless: false }),
    (0x1830, "SteelSeries Rival 3 Wireless", 3, Protocol::Rival),
    (0x1872, "SteelSeries Rival 3 Wireless Gen 2", 3, Protocol::Rival),
    (0x1726, "SteelSeries Rival 650 Wireless", 0, Protocol::Rival),
    (0x172B, "SteelSeries Rival 650 Wireless", 0, Protocol::Rival),
];

struct Mouse {
    dev: HidDevice,
    pid: u16,
    name: &'static str,
    protocol: Protocol,
    seen: bool,
}

#[derive(Default)]
pub struct SteelSeries {
    open: HashMap<CString, Mouse>,
    /// Collections already checked and found without output reports.
    skip: Vec<CString>,
}

impl SteelSeries {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let present: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == VID)
            .filter_map(|d| {
                let m = MODELS.iter().find(|m| m.0 == d.product_id() && m.2 == d.interface_number())?;
                Some((d.path().to_owned(), m))
            })
            .collect();
        self.open.retain(|path, _| present.iter().any(|(p, _)| p == path));
        self.skip.retain(|path| present.iter().any(|(p, _)| p == path));

        // The command collection is the one on that interface with an output report.
        for (path, info) in &present {
            let (pid, name, _, protocol) = **info;
            if self.open.contains_key(path) || self.skip.contains(path) || self.open.values().any(|m| m.pid == pid) {
                continue;
            }
            let Ok(dev) = api.open_path(path) else { continue };
            if !probe::descriptor(&dev).is_some_and(|d| probe::has_output(&d)) {
                self.skip.push(path.clone());
                continue;
            }
            self.open.insert(path.clone(), Mouse { dev, pid, name, protocol, seen: false });
        }

        let mut by_name: HashMap<&'static str, State> = HashMap::new();
        let mut broken = Vec::new();
        for (path, m) in self.open.iter_mut() {
            let key = format!("steelseries:{:04x}", m.pid);
            let state = match query(&m.dev, m.protocol) {
                Ok(Some((percent, charging))) => {
                    m.seen = true;
                    diag::event(&key, "battery reply");
                    State::Battery { percent, charging }
                }
                Ok(None) => {
                    diag::event(&key, "no battery reply (mouse off or asleep)");
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
            let slot = by_name.entry(m.name).or_insert(state);
            if !matches!(slot, State::Battery { .. }) {
                *slot = state;
            }
        }
        for path in broken {
            self.open.remove(&path);
        }
        by_name
            .into_iter()
            .map(|(name, state)| Observation { id: format!("steelseries:{}", slug(name)), name: Some(name.into()), kind: Some(Kind::Mouse), state })
            .collect()
    }
}

fn slug(name: &str) -> String {
    name.to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join("-")
}

fn query(dev: &HidDevice, protocol: Protocol) -> hidapi::HidResult<Option<(u8, bool)>> {
    let mut buf = [0u8; 64];
    while dev.read_timeout(&mut buf, 0)? > 0 {} // stale reports
    match protocol {
        Protocol::Aerox { wireless } => dev.write(&[0x00, if wireless { 0xD2 } else { 0x92 }])?,
        Protocol::Rival => dev.write(&[0x00, 0xAA, 0x01])?,
    };
    let deadline = Instant::now() + WAIT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let n = dev.read_timeout(&mut buf, left.as_millis() as i32)?;
        if n == 0 {
            break;
        }
        if let Some(r) = parse(&buf[..n], protocol) {
            return Ok(Some(r));
        }
    }
    Ok(None)
}

fn parse(r: &[u8], protocol: Protocol) -> Option<(u8, bool)> {
    match protocol {
        Protocol::Aerox { .. } => {
            let b = *r.get(1)?;
            let level = b & 0x7F;
            if !(1..=21).contains(&level) {
                return None;
            }
            Some(((level - 1) * 5, b & 0x80 != 0))
        }
        Protocol::Rival => {
            let (percent, charging) = (*r.first()?, *r.get(2)?);
            (percent <= 100).then_some((percent, charging != 0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aerox() {
        let p = Protocol::Aerox { wireless: true };
        assert_eq!(parse(&[0xD2, 21], p), Some((100, false)));
        assert_eq!(parse(&[0xD2, 0x80 | 11], p), Some((50, true)));
        assert_eq!(parse(&[0xD2, 0], p), None); // mouse off
        assert_eq!(parse(&[0xD2], p), None);
    }

    #[test]
    fn parses_rival() {
        assert_eq!(parse(&[73, 0, 1], Protocol::Rival), Some((73, true)));
        assert_eq!(parse(&[73, 0, 0], Protocol::Rival), Some((73, false)));
        assert_eq!(parse(&[0xFF, 0, 0], Protocol::Rival), None);
    }

    #[test]
    fn table_has_no_duplicates() {
        for (i, m) in MODELS.iter().enumerate() {
            assert!(MODELS[i + 1..].iter().all(|n| n.0 != m.0), "pid {:04x} twice", m.0);
        }
    }
}
