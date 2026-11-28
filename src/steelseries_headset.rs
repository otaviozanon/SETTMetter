//! SteelSeries Arctis headsets (the mouse/headset table in `steelseries.rs` covers the mice,
//! plus the Nova 5 push protocol; this module adds the query-based headset families).
//!
//! Protocols from Wireless-Device-Battery-Monitor (drivers/steelseries.cpp), which follows
//! HeadsetControl: Arctis 1 `06 12`, Arctis 7 `06 18`, Arctis 9 `00 20`, Arctis Pro `41/40 AA`,
//! Nova Pro `06 B0`, Nova family `00 B0`. Read-only.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;

const VID: u16 = 0x1038;

#[derive(Default)]
pub struct SteelSeriesHeadset {
    seen: HashSet<u16>,
}

impl SteelSeriesHeadset {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for &pid in PIDS {
            let mut state = None;
            for info in api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == pid) {
                if !accepts(pid, info) {
                    continue;
                }
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if let Some(s) = poll(&dev, &desc, pid) {
                    state = Some(s);
                    break;
                }
            }
            match state {
                Some(state) => {
                    self.seen.insert(pid);
                    out.push(observation(pid, name(pid), state));
                }
                None if self.seen.contains(&pid) => out.push(observation(pid, name(pid), State::Unreachable)),
                None => {}
            }
        }
        out
    }
}

const PIDS: &[u16] = &[
    0x12B3, 0x12B6, 0x12D7, // Arctis 1
    0x1260, 0x12AD, 0x1252, // Arctis 7 / Pro 2019
    0x12C2, // Arctis 9
    0x1290, // Arctis Pro Wireless
    0x12E0, 0x225D, // Nova Pro Wireless
    0x220E, 0x2212, 0x2216, 0x2236, 0x2269, 0x226D, 0x230A, 0x2202, 0x22A1, 0x227E,
    0x2206, 0x2258, 0x229E, 0x22AD, 0x223A, 0x22A9, 0x227A, 0x22A4, 0x22A5, 0x220A, 0x22A7,
];

fn accepts(pid: u16, d: &hidapi::DeviceInfo) -> bool {
    match pid {
        0x12B3 | 0x12B6 | 0x12D7 | 0x1260 | 0x12AD | 0x1252 => {
            d.usage_page() == 0xFF43 && d.usage() == 0x0202
        }
        0x12C2 | 0x1290 => d.interface_number() == 0,
        0x12E0 | 0x225D => d.interface_number() == 4,
        _ => d.usage_page() == 0xFFC0 && d.usage() == 0x0001,
    }
}

fn name(pid: u16) -> &'static str {
    match pid {
        0x12B3 | 0x12B6 | 0x12D7 => "SteelSeries Arctis 1 Wireless",
        0x1260 | 0x12AD => "SteelSeries Arctis 7",
        0x1252 => "SteelSeries Arctis Pro",
        0x12C2 => "SteelSeries Arctis 9",
        0x1290 => "SteelSeries Arctis Pro Wireless",
        0x12E0 | 0x225D => "SteelSeries Arctis Nova Pro Wireless",
        0x2269 | 0x226D => "SteelSeries Arctis Nova 3P Wireless",
        0x230A => "SteelSeries Arctis GameBuds",
        0x220E | 0x2212 | 0x2216 | 0x2236 => "SteelSeries Arctis 7+",
        _ => "SteelSeries Arctis Nova 7",
    }
}

fn observation(pid: u16, name: &str, state: State) -> Observation {
    Observation { id: format!("steelseries:{pid:04x}"), name: Some(name.into()), kind: Some(Kind::Headset), state }
}

fn poll(dev: &HidDevice, desc: &[u8], pid: u16) -> Option<State> {
    match pid {
        0x12B3 | 0x12B6 | 0x12D7 => {
            let r = query(dev, desc, &[0x06, 0x12])?;
            if r.len() < 4 {
                return None;
            }
            if r[2] == 0x01 {
                return Some(State::Unreachable);
            }
            Some(State::Battery { percent: r[3].min(100), charging: false })
        }
        0x1260 | 0x12AD | 0x1252 => {
            let r = query(dev, desc, &[0x06, 0x18])?;
            if r.len() < 3 {
                return None;
            }
            if r[2] == 0 {
                Some(State::Unreachable)
            } else {
                Some(State::Battery { percent: r[2].min(100), charging: false })
            }
        }
        0x12C2 => {
            let r = query(dev, desc, &[0x00, 0x20])?;
            if r.len() < 5 {
                return None;
            }
            Some(State::Battery { percent: map(r[3], 0x64, 0x9A), charging: r[4] == 0x01 })
        }
        0x1290 => {
            let r = query(dev, desc, &[0x00, 0x41, 0xAA])?;
            if r.is_empty() {
                return None;
            }
            if r[0] == 0x02 {
                return Some(State::Unreachable);
            }
            let level = query(dev, desc, &[0x00, 0x40, 0xAA]).and_then(|r| r.first().copied()).filter(|v| *v <= 4);
            Some(match level {
                Some(v) => State::Battery { percent: map(v, 0, 4), charging: false },
                None => State::Unreachable,
            })
        }
        0x12E0 | 0x225D => {
            let r = query(dev, desc, &[0x06, 0xB0])?;
            if r.len() < 16 {
                return None;
            }
            if r[15] == 0x01 {
                return Some(State::Unreachable);
            }
            Some(State::Battery { percent: map(r[6], 0, 8), charging: r[15] == 0x02 })
        }
        _ => nova_family(dev, desc, pid),
    }
}

fn nova_family(dev: &HidDevice, desc: &[u8], pid: u16) -> Option<State> {
    let r = query(dev, desc, &[0x00, 0xB0])?;
    if r.len() < 7 {
        return None;
    }
    if matches!(pid, 0x220E | 0x2212 | 0x2216 | 0x2236) {
        return Some(State::Battery { percent: map(r[2], 0, 4), charging: r[3] == 0x01 });
    }
    if matches!(pid, 0x2269 | 0x226D) {
        return Some(State::Battery { percent: r[3].min(100), charging: r[4] == 0x01 });
    }
    if pid == 0x230A {
        let mut level: Option<u8> = None;
        if r[3] == 0x03 {
            level = Some(r[5]);
        }
        if r[4] == 0x03 {
            level = Some(level.map_or(r[6], |v| v.min(r[6])));
        }
        return level.map(|v| State::Battery { percent: v.min(100), charging: false });
    }
    let discrete = matches!(pid, 0x2202 | 0x2206 | 0x220A | 0x223A | 0x227A | 0x22A4);
    if r[3] == 0 {
        return Some(State::Unreachable);
    }
    Some(State::Battery {
        percent: if discrete { map(r[2], 0, 4) } else { r[2].min(100) },
        charging: r[3] == 0x01 || r[3] == 0x02,
    })
}

fn query(dev: &HidDevice, desc: &[u8], command: &[u8]) -> Option<Vec<u8>> {
    let id = command[0];
    let out_len = if id == 0 {
        probe::output_len(desc, 0).map(|n| n + 1)
    } else {
        probe::output_len(desc, id)
    }?;
    if out_len < command.len() {
        return None;
    }
    let mut req = vec![0u8; out_len];
    req[..command.len()].copy_from_slice(command);
    while let Ok(n) = dev.read_timeout(&mut [0u8; 64], 0) {
        if n == 0 {
            break;
        }
    }
    if let Err(e) = dev.write(&req) {
        diag::event("steelseries-headset", format!("i/o failed: {e}"));
        return None;
    }
    let mut buf = [0u8; 64];
    match dev.read_timeout(&mut buf, 1000) {
        Ok(0) => None,
        Ok(n) => Some(buf[..n].to_vec()),
        Err(_) => None,
    }
}

fn map(value: u8, min: u8, max: u8) -> u8 {
    if max <= min {
        return 0;
    }
    ((u32::from(value.saturating_sub(min)) * 100) / u32::from(max - min)).min(100) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_matches_known_ranges() {
        assert_eq!(map(0x64, 0x64, 0x9A), 0);
        assert_eq!(map(0x9A, 0x64, 0x9A), 100);
        assert_eq!(map(4, 0, 4), 100);
        assert_eq!(map(8, 0, 8), 100);
    }
}
