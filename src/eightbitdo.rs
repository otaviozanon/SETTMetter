//! 8BitDo wireless controllers (2DC8), over their D-input gamepad reports or the vendor
//! config channel.
//!
//! 1. D-input gamepad reports (usage page 0001, usage 0004/0005, >= 15 bytes, report 01 or 04,
//!    byte 14: bits 0-6 = percent, bit 7 = charging). Cover the Ultimate 3-mode, Ultimate 2C,
//!    Pro 3/2 and SN30/SF30 Pro in Bluetooth or 2.4 GHz mode.
//! 2. Vendor config channel (page FF7A or FFA0, usage 0001, 64-byte reports, output id 81,
//!    input id 02), used in X-input mode where the 2.4 GHz dongle emulates a wired Xbox pad:
//!    `81 04 00 01` (info) -> internal PID at [22..23] (6013/3109 = dongle, else USB cable)
//!    `81 04 20 01` (link) -> [18] != 0 when the pad is linked
//!    `81 04 21 01 00 00 06` (battery) -> [18] bits 0-6 level, bit 7 charging
//!    The Ultimate 2 dongle turns its radio off while the pad sits on the charging dock and
//!    moves to 6013/3109: the last level is kept visible as asleep.
//! Protocols from Wireless-Device-Battery-Monitor (drivers/eightbitdo.cpp), which follows SDL's
//! SDL_hidapi_8bitdo.c and MiguelSanzBr/8bitdo-ultimate2-linux.

use crate::device::{Kind, Observation, State};
use crate::{diag, probe};
use hidapi::{DeviceInfo, HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x2DC8;
const PID_DONGLE: u16 = 0x6013;
const PID_DONGLE_IDLE: u16 = 0x3109;
/// Ultimate 2 firmware 1.02 sends 12-byte reports without a battery byte.
const ULTIMATE2_REPORT_LEN: usize = 34;

struct Model {
    pid: u16,
    name: &'static str,
    /// Older pads only report a battery level after feature report 06 is read.
    feature06: bool,
}

const MODELS: &[Model] = &[
    Model { pid: 0x310B, name: "8BitDo Ultimate 2 Wireless", feature06: false },
    Model { pid: 0x6012, name: "8BitDo Ultimate 2 Wireless", feature06: false },
    Model { pid: 0x6013, name: "8BitDo Ultimate 2 Wireless", feature06: false },
    Model { pid: 0x202F, name: "8BitDo Ultimate 3-mode", feature06: false },
    Model { pid: 0x202E, name: "8BitDo Ultimate 3-mode", feature06: false },
    Model { pid: 0x301B, name: "8BitDo Ultimate 2C Wireless", feature06: false },
    Model { pid: 0x301A, name: "8BitDo Ultimate 2C Wireless", feature06: false },
    Model { pid: 0x310A, name: "8BitDo Ultimate 2C Wireless", feature06: false },
    Model { pid: 0x3012, name: "8BitDo Ultimate 2.4G", feature06: false },
    Model { pid: 0x3013, name: "8BitDo Ultimate 2.4G", feature06: false },
    Model { pid: 0x3109, name: "8BitDo Ultimate 2.4G", feature06: false },
    Model { pid: 0x6007, name: "8BitDo Ultimate Bluetooth", feature06: false },
    Model { pid: 0x6009, name: "8BitDo Pro 3", feature06: true },
    Model { pid: 0x6003, name: "8BitDo Pro 2", feature06: true },
    Model { pid: 0x6006, name: "8BitDo Pro 2", feature06: true },
    Model { pid: 0x6001, name: "8BitDo SN30 Pro", feature06: true },
    Model { pid: 0x6101, name: "8BitDo SN30 Pro", feature06: true },
    Model { pid: 0x6000, name: "8BitDo SF30 Pro", feature06: true },
    Model { pid: 0x6100, name: "8BitDo SF30 Pro", feature06: true },
];

fn model(pid: u16) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.pid == pid)
}

fn is_ultimate2(pid: u16) -> bool {
    matches!(pid, 0x310B | 0x6012 | 0x6013)
}

fn is_idle_dongle(pid: u16) -> bool {
    matches!(pid, PID_DONGLE | PID_DONGLE_IDLE)
}

#[derive(Default)]
pub struct EightBitDo {
    seen: Vec<(String, String)>,
    /// Last level an Ultimate 2 reported, kept while its dongle sits idle.
    last_ultimate2: Option<u8>,
}

/// A reading from either channel; `percent` is None when the pad answered but gave no level.
struct Reading {
    percent: Option<u8>,
    charging: bool,
}

impl EightBitDo {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut pids: Vec<u16> = api.device_list().filter(|d| d.vendor_id() == VID).map(|d| d.product_id()).collect();
        pids.sort_unstable();
        pids.dedup();

        let mut out = Vec::new();
        let mut live_ultimate2 = false;
        // Live, wired and Bluetooth collections first; idle 2.4 GHz dongles last, so an idle
        // dongle never overwrites an active cable or Bluetooth reading for the same pad.
        for pass in 0..2 {
            for &pid in &pids {
                if (pass == 0) == is_idle_dongle(pid) {
                    continue;
                }
                self.product(api, pid, &mut live_ultimate2, &mut out);
            }
        }
        for (key, name) in &self.seen {
            if !out.iter().any(|o| o.id == *key) {
                out.push(Observation { id: key.clone(), name: Some(name.clone()), kind: Some(Kind::Gamepad), state: State::Unreachable });
            }
        }
        out
    }

    fn product(&mut self, api: &HidApi, pid: u16, live_ultimate2: &mut bool, out: &mut Vec<Observation>) {
        let mut answered = false;
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == pid && matches!(d.usage_page(), 0xFF7A | 0xFFA0) && d.usage() == 0x0001
        }) {
            let Some(dev) = api.open_path(info.path()).ok() else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            let (Some(in_len), Some(out_len)) = (probe::input_len(&desc, 0x02), probe::output_len(&desc, 0x81)) else { continue };
            if in_len < 64 || out_len < 64 {
                continue;
            }
            let Some((reading, via)) = self.vendor(&dev, out_len, pid) else { continue };
            answered = true;
            if is_ultimate2(pid) && reading.percent.is_some() {
                *live_ultimate2 = true;
            }
            // The idle dongle of an already live Ultimate 2 would only add an empty duplicate.
            if is_idle_dongle(pid) && is_ultimate2(pid) && *live_ultimate2 && reading.percent.is_none() {
                break;
            }
            self.emit(out, pid, info, reading, via);
            break;
        }
        if answered {
            return;
        }
        let Some(model) = model(pid) else { return };
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0x01 && matches!(d.usage(), 0x04 | 0x05)
        }) {
            let Some(dev) = api.open_path(info.path()).ok() else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::input_len(&desc, 0x01).or_else(|| probe::input_len(&desc, 0x04)).unwrap_or(0) < 15 {
                continue;
            }
            let feature06 = model.feature06.then(|| probe::feature_len(&desc, 0x06)).flatten();
            let Some(reading) = dinput(&dev, pid, feature06) else { continue };
            if is_ultimate2(pid) && reading.percent.is_some() {
                *live_ultimate2 = true;
            }
            let path = info.path().to_string_lossy();
            let via = if path.contains("00001124") || path.contains("00001812") { "Bluetooth" } else { "2.4 GHz dongle or USB" };
            self.emit(out, pid, info, reading, via);
            return;
        }
    }

    fn emit(&mut self, out: &mut Vec<Observation>, pid: u16, info: &DeviceInfo, reading: Reading, via: &'static str) {
        let key = if is_ultimate2(pid) { "8bitdo:ultimate2".to_string() } else { format!("8bitdo:{pid:04x}") };
        let name = name(pid, info);
        let state = match reading.percent {
            Some(percent) => State::Battery { percent, charging: reading.charging },
            None => State::Unreachable,
        };
        let level = match reading.percent {
            Some(p) => format!("{p}%{}", if reading.charging { " charging" } else { "" }),
            None => "no level".to_string(),
        };
        diag::event(&key, format!("{via}: {level}"));
        self.seen.push((key.clone(), name.clone()));
        out.push(Observation { id: key, name: Some(name), kind: Some(Kind::Gamepad), state });
    }

    /// The vendor config channel: tells cable from dongle, then reads the level.
    fn vendor(&mut self, dev: &HidDevice, out_len: usize, pid: u16) -> Option<(Reading, &'static str)> {
        let mut wired = false;
        let mut is_dongle = false;
        if let Some(r) = query(dev, out_len, &[0x81, 0x04, 0x00, 0x01], 0x00, 0x01) {
            if let Some(internal) = internal_pid(&r) {
                is_dongle = matches!(internal, PID_DONGLE | PID_DONGLE_IDLE);
                wired = !is_dongle;
            }
        }

        if wired {
            if let Some(r) = query(dev, out_len, &[0x81, 0x04, 0x21, 0x01, 0x00, 0x00, 0x06], 0x21, 0x01) {
                let percent = r[18] & 0x7F;
                if (1..=100).contains(&percent) {
                    if is_ultimate2(pid) {
                        self.last_ultimate2 = Some(percent);
                    }
                    return Some((Reading { percent: Some(percent), charging: true }, "USB cable"));
                }
            }
            let percent = self.last_ultimate2.filter(|_| is_ultimate2(pid));
            return Some((Reading { percent, charging: true }, "USB cable"));
        }

        let mut linked_known = false;
        let mut linked = true;
        if let Some(r) = query(dev, out_len, &[0x81, 0x04, 0x20, 0x01], 0x20, 0x01) {
            linked_known = true;
            linked = r[18] != 0;
        }
        if (!linked_known || linked) && let Some(r) = query(dev, out_len, &[0x81, 0x04, 0x21, 0x01, 0x00, 0x00, 0x06], 0x21, 0x01) {
            let percent = r[18] & 0x7F;
            if (1..=100).contains(&percent) {
                if is_ultimate2(pid) {
                    self.last_ultimate2 = Some(percent);
                }
                let charging = r[18] & 0x80 != 0 && percent < 100;
                return Some((Reading { percent: Some(percent), charging }, "2.4 GHz dongle"));
            }
        }

        if is_dongle || is_idle_dongle(pid) {
            let percent = self.last_ultimate2.filter(|_| is_ultimate2(pid));
            return Some((Reading { percent, charging: false }, "2.4 GHz dongle"));
        }
        None
    }
}

/// Internal PID from the `81 04 00 01` (CMD_INFO) reply.
fn internal_pid(reply: &[u8]) -> Option<u16> {
    Some(u16::from_le_bytes([*reply.get(22)?, *reply.get(23)?]))
}

fn name(pid: u16, info: &DeviceInfo) -> String {
    if let Some(model) = model(pid) {
        return model.name.to_string();
    }
    match info.product_string().filter(|p| !p.trim().is_empty()) {
        Some(p) if p.starts_with("8BitDo") || p.starts_with("8Bitdo") => p.to_string(),
        Some(p) => format!("8BitDo {p}"),
        None => "8BitDo Wireless Controller".to_string(),
    }
}

/// One vendor request/reply round trip. The reply must carry the same command bytes.
fn query(dev: &HidDevice, out_len: usize, req: &[u8], cmd_lo: u8, cmd_hi: u8) -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    let mut out = vec![0u8; out_len];
    let n = req.len().min(out_len);
    out[..n].copy_from_slice(&req[..n]);
    dev.write(&out).ok()?;
    let deadline = Instant::now() + Duration::from_millis(300);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis().max(1) as i32) {
            Ok(n) if n >= 19 && buf[0] == 0x02 && buf[1] == 0x04 && buf[4] == cmd_lo && buf[5] == cmd_hi => {
                return Some(buf[..n].to_vec());
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

/// D-input gamepad report: byte 14 holds the level and the charging bit.
fn dinput(dev: &HidDevice, pid: u16, feature06: Option<usize>) -> Option<Reading> {
    if let Some(len) = feature06 {
        let mut feat = vec![0u8; len];
        feat[0] = 0x06;
        let _ = dev.get_feature_report(&mut feat);
    }
    let min_len = if pid == 0x6012 { ULTIMATE2_REPORT_LEN } else { 15 };
    let mut buf = [0u8; 64];
    let deadline = Instant::now() + Duration::from_millis(600);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis().max(1) as i32) {
            Ok(n) if n >= min_len && matches!(buf[0], 0x01 | 0x04) && buf[14] & 0x7F <= 100 => {
                let percent = buf[14] & 0x7F;
                let charging = buf[14] & 0x80 != 0 && percent < 100;
                return Some(Reading { percent: Some(percent), charging });
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dinput_battery_byte() {
        // Off the wire: 0xE0 = charging, 0x60 = 96%.
        assert_eq!(0xE0u8 & 0x7F, 96);
        assert_ne!(0xE0u8 & 0x80, 0);
        assert_eq!(0x60u8 & 0x7F, 96);
        assert_eq!(0x60u8 & 0x80, 0);
    }

    #[test]
    fn info_reply_internal_pid() {
        let mut reply = vec![0u8; 64];
        reply[0] = 0x02;
        reply[1] = 0x04;
        reply[4] = 0x00;
        reply[5] = 0x01;
        reply[22] = 0x13; // 0x6013, the 2.4G dongle
        reply[23] = 0x60;
        assert_eq!(internal_pid(&reply), Some(0x6013));
        assert!(is_idle_dongle(0x6013) && is_idle_dongle(0x3109));
        assert!(!is_idle_dongle(0x6012)); // the pad itself over its cable
        assert_eq!(internal_pid(&[0x02; 4]), None);
    }

    #[test]
    fn ultimate2_ids_share_one_row() {
        assert!(is_ultimate2(0x310B) && is_ultimate2(0x6012) && is_ultimate2(0x6013));
        assert!(!is_ultimate2(0x301B));
    }
}
