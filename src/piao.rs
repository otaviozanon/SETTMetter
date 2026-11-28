//! IPI PIAO wireless and wired mice (372E:1014 receiver, 1015 / 1028 / 1056 cables).
//!
//! 64-byte feature report 0x03: `[03][sum][50][00][00][cmd][sub]…`, sum = sum of bytes 1..63.
//! `47 00` reads the link state; `4F 81` the battery, bit 7 = charging and bits 0-6 = percent.
//! PIAO echoes command/sub differently from EWEADN, so both layouts are accepted.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/piao.cpp), captured on hardware and in
//! IPI's web driver.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::Duration;

const VID: u16 = 0x372E;
const REPORT_ID: u8 = 0x03;
const LEN: usize = 64;

#[derive(Default)]
pub struct Piao {
    seen: HashSet<u16>,
}

impl Piao {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for &(pid, wireless) in &[(0x1014u16, true), (0x1015, false), (0x1028, false), (0x1056, false)] {
            let before = out.len();
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0xFF00 && d.usage() == 0x0001
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::feature_len(&desc, REPORT_ID).unwrap_or(0) < LEN {
                    continue;
                }
                if wireless {
                    match query(&dev, 0x47, 0x00) {
                        Some(data) if data.first().copied().unwrap_or(0) == 0 => {
                            if self.seen.contains(&pid) {
                                out.push(observation(pid, State::Unreachable));
                            }
                            break;
                        }
                        Some(_) => {}
                        None => continue,
                    }
                }
                let Some(data) = query(&dev, 0x4F, 0x81) else { continue };
                let Some(&raw) = data.first() else { continue };
                let level = raw & 0x7F;
                if level > 100 {
                    continue;
                }
                self.seen.insert(pid);
                out.push(observation(pid, State::Battery { percent: level, charging: raw & 0x80 != 0 }));
                break;
            }
            if out.len() == before && self.seen.contains(&pid) {
                out.push(observation(pid, State::Unreachable));
            }
        }
        out
    }
}

fn observation(pid: u16, state: State) -> Observation {
    Observation {
        id: format!("piao:{pid:04x}"),
        name: Some("IPI PIAO wireless mouse".into()),
        kind: Some(Kind::Mouse),
        state,
    }
}

fn query(dev: &HidDevice, cmd: u8, sub: u8) -> Option<Vec<u8>> {
    let mut req = [0u8; LEN];
    req[0] = REPORT_ID;
    req[2] = 0x50;
    req[5] = cmd;
    req[6] = sub;
    let sum: u8 = req[1..].iter().fold(0u8, |s, b| s.wrapping_add(*b));
    req[1] = sum;
    for attempt in 0..6 {
        if attempt == 3 && cmd == 0x4F {
            let _ = query(dev, 0x47, 0x00); // wake the mouse through the link channel
        }
        if let Err(e) = dev.send_feature_report(&req) {
            diag::event("piao", format!("i/o failed: {e}"));
            return None;
        }
        std::thread::sleep(Duration::from_millis(120));
        let mut reply = [0u8; LEN];
        reply[0] = REPORT_ID;
        let Ok(n) = dev.get_feature_report(&mut reply) else { continue };
        if n < 8 || reply[0] != REPORT_ID || reply[2] != 0 || reply[4] != 0 {
            continue;
        }
        let offset = if reply[5] == cmd {
            7
        } else if reply[5] == sub || reply[5] == (sub & 0x0F) || reply[5] == (sub & 0x7F) {
            6
        } else {
            continue;
        };
        let len = usize::from(reply[3]);
        if len == 0 || offset + len > n {
            continue;
        }
        return Some(reply[offset..offset + len].to_vec());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_bits_match_the_vendor_decode() {
        assert_eq!(0xE4 & 0x7F, 100);
        assert_eq!(0xE4 & 0x80, 0x80);
        assert_eq!(0x5D & 0x7F, 93);
    }

    #[test]
    fn query_checksum_covers_payload() {
        let mut req = [0u8; LEN];
        req[0] = REPORT_ID;
        req[2] = 0x50;
        req[5] = 0x4F;
        req[6] = 0x81;
        req[1] = req[1..].iter().fold(0u8, |s, b| s.wrapping_add(*b));
        assert_eq!(req[1], 0x50u8.wrapping_add(0x4F).wrapping_add(0x81));
    }
}
