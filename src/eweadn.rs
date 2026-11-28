//! EWEADN S7 / S9 mice and the Royal Kludge M3 on EWEADN's vendor id 372E.
//!
//! 372E:1026 / 1033 / 103D / 1055 are the shared "Gaming 2.4G" S9 receiver; 1029 / 1027 / 105E /
//! 108D are S9 models on their USB cables. All speak the BeiYing protocol from EWEADN's web
//! driver, as does the RK M3 (2.4 GHz dongle 1019, cable 102A) from RK's web driver:
//!
//! Requests are 64-byte feature reports with report id 3 (usage page FF00, interface 2), read
//! back about 100 ms later:
//!   `03 <sum> 50 <len> 00 <cmd> <sub> 00…`, sum = sum of the 63 bytes after the id.
//!   `03 50 <ack> <len> 00 <sub> <data…>`, ack 0 = OK, data starts at byte 6.
//! `47 00` (link check) gives data[0] = 1 when the mouse is linked; `4F 81` (battery) gives
//! data[0] bit 7 = charging and bits 0-6 = percent.
//!
//! RK's variant sends sequence number 2 in byte 4, waits 200 ms and accepts only replies whose
//! byte 5 is 1. Only these read commands are sent.
//!
//! Protocol from Wireless-Device-Battery-Monitor (drivers/eweadn.cpp). Other 372E products
//! (AULA, VGN, keyboards) are not this protocol.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::Duration;

const VID: u16 = 0x372E;
const REPORT_ID: u8 = 0x03;
const LEN: usize = 64;
const LINK: (u8, u8) = (0x47, 0x00);
const BATTERY: (u8, u8) = (0x4F, 0x81);

struct Model {
    pid: u16,
    name: &'static str,
    /// Ask 47 00 first: these receivers report whether the mouse is linked.
    link_check: bool,
    rk: bool,
}

const MODELS: &[Model] = &[
    Model { pid: 0x1026, name: "EWEADN wireless mouse", link_check: true, rk: false },
    Model { pid: 0x1033, name: "EWEADN wireless mouse", link_check: true, rk: false },
    Model { pid: 0x103D, name: "EWEADN wireless mouse", link_check: true, rk: false },
    Model { pid: 0x1055, name: "EWEADN wireless mouse", link_check: true, rk: false },
    Model { pid: 0x1027, name: "EWEADN S9 Ultra", link_check: false, rk: false },
    Model { pid: 0x1029, name: "EWEADN S9 Pro", link_check: false, rk: false },
    Model { pid: 0x105E, name: "EWEADN S9 Max", link_check: false, rk: false },
    Model { pid: 0x108D, name: "EWEADN S9 Ultra+", link_check: false, rk: false },
    Model { pid: 0x1019, name: "RK M3", link_check: false, rk: true },
    Model { pid: 0x102A, name: "RK M3", link_check: false, rk: true },
];

#[derive(Default)]
pub struct Eweadn {
    seen: HashSet<String>,
}

impl Eweadn {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for model in MODELS {
            // RK's M3 keeps its own source prefix; EWEADN's ids are the USB product ids.
            let key = if model.rk { format!("rk:{:04x}", model.pid) } else { format!("eweadn:{:04x}", model.pid) };
            let before = out.len();
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == model.pid
                    && d.usage_page() == 0xFF00
                    && d.usage() == 0x0001
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::feature_len(&desc, REPORT_ID).unwrap_or(0) != LEN {
                    continue;
                }
                if model.link_check {
                    let Some(data) = query(&dev, model.rk, LINK.0, LINK.1) else { continue };
                    if data.first() != Some(&1) {
                        // Receiver present, mouse off or out of range.
                        break;
                    }
                }
                let Some(data) = query(&dev, model.rk, BATTERY.0, BATTERY.1) else { continue };
                let Some(&raw) = data.first() else { continue };
                let (level, charging) = level_raw(raw);
                if level > 100 {
                    continue;
                }
                diag::event(&key, format!("battery via 4F 81 (0x{raw:02X})"));
                out.push(observation(&key, model.name, State::Battery { percent: level, charging }));
                self.seen.insert(key.clone());
                break;
            }
            if out.len() == before && self.seen.contains(&key) {
                out.push(observation(&key, model.name, State::Unreachable));
            }
        }
        out
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

fn level_raw(raw: u8) -> (u8, bool) {
    (raw & 0x7F, raw & 0x80 != 0)
}

/// `03 <sum> 50 <len> 00 <cmd> <sub> 00…`, with the sequence number RK counts on.
fn request(rk: bool, cmd: u8, sub: u8) -> [u8; LEN] {
    let mut req = [0u8; LEN];
    req[0] = REPORT_ID;
    req[2] = 0x50; // CMD_TYPE
    req[4] = if rk { 0x02 } else { 0x00 };
    req[5] = cmd;
    req[6] = sub;
    let sum = req[1..].iter().fold(0u8, |s, b| s.wrapping_add(*b));
    req[1] = sum;
    req
}

fn query(dev: &HidDevice, rk: bool, cmd: u8, sub: u8) -> Option<Vec<u8>> {
    let req = request(rk, cmd, sub);
    let sn = req[4];
    if dev.send_feature_report(&req).is_err() {
        return None;
    }
    for attempt in 0..3 {
        // RK waits 200 ms; an earlier read can return the previous reply.
        std::thread::sleep(Duration::from_millis(if rk && attempt == 0 { 200 } else { 100 }));
        let mut reply = [0u8; LEN];
        reply[0] = REPORT_ID;
        let Ok(n) = dev.get_feature_report(&mut reply) else { continue };
        if let Some(data) = reply_data(&reply[..n], sn, rk) {
            return Some(data.to_vec());
        }
    }
    None
}

/// `03 50 <ack> <len> 00 <sub> <data…>`; the reply echoes the sequence number in byte 4.
fn reply_data(reply: &[u8], sn: u8, rk: bool) -> Option<&[u8]> {
    if reply.len() < 8 || reply[0] != REPORT_ID || reply[2] != 0 || reply[4] != sn {
        return None;
    }
    if rk && reply[5] != 1 {
        return None; // RK: the mouse did not answer
    }
    let len = usize::from(reply[3]);
    if len == 0 || 6 + len > reply.len() {
        return None;
    }
    Some(&reply[6..6 + len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_checksum_covers_the_payload() {
        let req = request(false, BATTERY.0, BATTERY.1);
        assert_eq!(req[1], 0x20); // 50 + 4F + 81 = 0x120

        let rk = request(true, BATTERY.0, BATTERY.1);
        assert_eq!(rk[4], 0x02); // RK's sequence number is covered too
        assert_eq!(rk[1], 0x22); // 50 + 02 + 4F + 81 = 0x122
    }

    #[test]
    fn parses_the_captured_s9_reply() {
        // S9 Pro 8K receiver: 4F 81 answered 5D (93 %).
        let reply = [0x03, 0x50, 0x00, 0x01, 0x00, 0x81, 0x5D, 0x00];
        let data = reply_data(&reply, 0x00, false).unwrap();
        assert_eq!(data[0], 0x5D);
        assert_eq!(level_raw(0x5D), (93, false));
        assert_eq!(level_raw(0xDD), (93, true));

        // The reply to another sequence number is not ours.
        assert_eq!(reply_data(&reply, 0x02, false), None);
        // A non-zero ack is an error.
        let mut bad = reply;
        bad[2] = 1;
        assert_eq!(reply_data(&bad, 0x00, false), None);
        // A length that runs past the reply.
        let mut bad = reply;
        bad[3] = 60;
        assert_eq!(reply_data(&bad, 0x00, false), None);
    }

    #[test]
    fn rk_needs_its_answer_flag() {
        let mut reply = [0x03, 0x50, 0x00, 0x01, 0x02, 0x01, 0x64, 0x00];
        assert_eq!(reply_data(&reply, 0x02, true), Some(&reply[6..7]));
        reply[5] = 0x00;
        assert_eq!(reply_data(&reply, 0x02, true), None);
    }

    #[test]
    fn link_byte_is_the_first_data_byte() {
        let reply = [0x03, 0x50, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00];
        assert_eq!(reply_data(&reply, 0x00, false).map(|d| d[0]), Some(1));
    }
}
