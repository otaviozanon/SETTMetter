//! DAREU mice and keyboards: the EM901X / Sharkoon Skiller SGM50W, the A84 keyboard, the
//! A950 Wing and the Togran receivers (A980 Pro, EK75 Pro).
//!
//! - EM901X/SGM50W: feature report 0x08 (17 bytes) on FF02:0002, command 04, reply as input
//!   report 0x09 on the sibling FF01:0000 collection: `[09][04][00]…[6]=percent [7]=charging`.
//! - A84: feature report 0 (65 bytes) on the keyboard collection, `04 F6`, then byte 5 of a
//!   later read is the level. This is the vendor driver's only HID command.
//! - A950 Wing: the shared Compx 17-byte report 0x08 on FF02:0002, command 04, reply report
//!   0x08 with the 0x55 checksum; byte 6 percent, 7 charging, 8-9 millivolts.
//! - Togran: feature report 0x0A (65 bytes) on consumer control page 0x000C, `[slot][len]
//!   [class][cmd]`; class 0x07/command 0x80 answers `[7]=state [8]=level [9]=max`. Slot 0,
//!   class 0x00/command 0xA0 lists the paired devices.
//!
//! All from Wireless-Device-Battery-Monitor (drivers/dareu.cpp), which took the EM901X query
//! from DAREU's own AIM-WL driver and the Togran protocol from DAREU's web driver. Read-only.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::checksum55 as checksum;
use crate::hidutil::drain as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const VID: u16 = 0x260D;
const LEN: usize = 17;
const REPORT_ID: u8 = 0x08;
const GET_POWER: u8 = 0x04;

#[derive(Default)]
pub struct Dareu;

impl Dareu {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        let mut claimed = HashSet::new();
        togran(api, &mut out, &mut claimed);
        a84(api, &mut out);
        a950_wing(api, &mut out, &mut claimed);
        em901x(api, &mut out, &mut claimed);
        out
    }
}

// --- Togran ---------------------------------------------------------------------------------

fn togran_query(
    dev: &HidDevice,
    slot: u8,
    len: u8,
    class: u8,
    cmd: u8,
) -> hidapi::HidResult<Option<Vec<u8>>> {
    let mut req = vec![0u8; 65];
    req[0] = 0x0A;
    req[1] = slot << 4;
    req[2] = len;
    req[3] = class;
    req[4] = cmd;
    dev.send_feature_report(&req)?;

    let mut buf = vec![0u8; 65];
    for _ in 0..15 {
        std::thread::sleep(Duration::from_millis(30));
        buf[0] = 0x0A;
        let Ok(n) = dev.get_feature_report(&mut buf) else { return Ok(None) };
        if n >= 11 && buf[1] & 0x0F == 2 && buf[3] == class && buf[4] == cmd {
            buf.truncate(n);
            return Ok(Some(buf));
        }
    }
    Ok(None)
}

/// `[7]` charge state, `[8]` level, `[9]` maximum level.
fn togran_battery(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 10 {
        return None;
    }
    let max = if r[9] == 0 { 100 } else { r[9] as u16 };
    if r[8] as u16 > max {
        return None;
    }
    let percent = (r[8] as u16 * 100 / max) as u8;
    Some((percent, r[7] == 1))
}

fn togran(api: &HidApi, out: &mut Vec<Observation>, claimed: &mut HashSet<(u16, u16)>) {
    let mut done = HashSet::new();
    for pid in [0x1131u16, 0x0046, 0x0042, 0x1148] {
        if claimed.contains(&(VID, pid)) || !done.insert(pid) {
            continue;
        }
        let colls: Vec<_> = api
            .device_list()
            .filter(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0x000C && d.usage() == 1
            })
            .collect();
        if colls.len() != 1 {
            continue;
        }
        let info = colls[0];
        let Ok(dev) = api.open_path(info.path()) else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if probe::feature_len(&desc, 0x0A) != Some(65) {
            continue;
        }
        if pid == 0x1148 || pid == 0x0046 {
            add_togran(out, &dev, 0, pid, pid);
            // The EK75 Pro cable claims its receiver.
            if pid == 0x0046 {
                claimed.insert((VID, 0x0042));
            }
            continue;
        }
        // Receiver: slot 0 class 00 / A0 lists the paired devices, three bytes each.
        let Some(r) = togran_query(&dev, 0, 6, 0x00, 0xA0).ok().flatten() else { continue };
        for slot in 1..=r[7].min(5) {
            let base = 8 + (slot as usize - 1) * 3;
            if base + 2 >= r.len() || r[base] == 0 {
                continue;
            }
            let device_pid = (u16::from(r[base + 1]) << 8) | u16::from(r[base + 2]);
            add_togran(out, &dev, slot, pid, device_pid);
        }
    }
}

fn add_togran(out: &mut Vec<Observation>, dev: &HidDevice, slot: u8, pid: u16, device_pid: u16) {
    let Some(r) = togran_query(dev, slot, 4, 0x07, 0x80).ok().flatten() else { return };
    let Some((percent, charging)) = togran_battery(&r) else { return };
    let stable = if device_pid == 0x0046 && pid == 0x0046 { 0x0042 } else { pid };
    let id = if stable == device_pid {
        format!("dareu:260d:{stable:04x}")
    } else {
        format!("dareu:260d:{stable:04x}:{device_pid:04x}")
    };
    let name = match device_pid {
        0x1148 => "DAREU A980 Pro".into(),
        0x0046 => "DAREU EK75 Pro".into(),
        other => format!("DAREU {other:04X}"),
    };
    diag::event(&id, format!("battery via feature 0A (slot {slot}, {percent}%)"));
    out.push(Observation {
        id,
        name: Some(name),
        kind: Some(if device_pid == 0x0046 { Kind::Keyboard } else { Kind::Mouse }),
        state: State::Battery { percent, charging },
    });
}

// --- A84 ------------------------------------------------------------------------------------

fn a84(api: &HidApi, out: &mut Vec<Observation>) {
    const KEY: &str = "dareu:260d:0022";
    let colls: Vec<_> = api
        .device_list()
        .filter(|d| {
            d.vendor_id() == VID
                && d.product_id() == 0x0022
                && d.interface_number() == 0
                && d.usage_page() == 0x01
                && d.usage() == 0x06
        })
        .collect();
    if colls.len() != 1 {
        return;
    }
    let Ok(dev) = api.open_path(colls[0].path()) else { return };
    let Some(desc) = probe::descriptor(&dev) else { return };
    let Some(payload) = probe::feature_len(&desc, 0) else { return };
    if payload < 64 {
        return;
    }
    let len = payload + 1; // report id 0

    let mut req = vec![0u8; len];
    req[1] = 0x04;
    req[2] = 0xF6;
    if let Err(e) = dev.send_feature_report(&req) {
        diag::event(KEY, format!("i/o failed: {e}"));
        return;
    }
    std::thread::sleep(Duration::from_millis(10));

    let mut buf = vec![0u8; len];
    for _ in 0..3 {
        // The vendor driver discards these reads.
        buf[0] = 0;
        if dev.get_feature_report(&mut buf).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut state = State::Unreachable;
    for i in 0..3 {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
        buf[0] = 0;
        let Ok(n) = dev.get_feature_report(&mut buf) else { continue };
        if n < 16 || buf[15] == 0 {
            continue;
        }
        if buf[5] > 100 {
            return;
        }
        state = State::Battery { percent: buf[5], charging: false };
        break;
    }
    diag::event(KEY, format!("battery via feature 04 F6: {state:?}"));
    out.push(Observation { id: KEY.into(), name: Some("DAREU A84".into()), kind: Some(Kind::Keyboard), state });
}

// --- A950 Wing ------------------------------------------------------------------------------

fn a950_wing(api: &HidApi, out: &mut Vec<Observation>, claimed: &mut HashSet<(u16, u16)>) {
    const CABLE: u16 = 0x1196;
    const RECEIVER: u16 = 0x1154;
    const KEY: &str = "dareu:260d:1154";
    for pid in [CABLE, RECEIVER] {
        if claimed.contains(&(VID, pid)) {
            continue;
        }
        let colls: Vec<_> = api
            .device_list()
            .filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == pid
                    && d.interface_number() == 1
                    && d.usage_page() == 0xFF02
                    && d.usage() == 2
            })
            .collect();
        if colls.len() != 1 {
            continue;
        }
        let info = colls[0];
        let Ok(dev) = api.open_path(info.path()) else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if probe::input_len(&desc, REPORT_ID) != Some(LEN) || probe::output_len(&desc, REPORT_ID) != Some(LEN) {
            continue;
        }

        let mut req = [0u8; LEN];
        req[0] = REPORT_ID;
        req[1] = GET_POWER;
        req[LEN - 1] = checksum(&req[..LEN - 1]);
        flush(&dev);
        if let Err(e) = dev.write(&req) {
            diag::event(KEY, format!("i/o failed: {e}"));
            continue;
        }

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut buf = [0u8; 64];
        let mut reading = None;
        for _ in 0..32 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
            match dev.read_timeout(&mut buf, (left.as_millis() as i32).clamp(1, 250)) {
                Ok(0) => {}
                Ok(n) => {
                    if let Some((percent, charging, millivolts)) = parse_a950(&buf[..n]) {
                        diag::event(KEY, format!("battery via report 08 (level {percent}, {millivolts} mV)"));
                        reading = Some((percent, charging));
                        break;
                    }
                }
                Err(e) => {
                    diag::event(KEY, format!("i/o failed: {e}"));
                    break;
                }
            }
        }
        if let Some((percent, charging)) = reading {
            if pid == CABLE {
                claimed.insert((VID, RECEIVER));
            }
            out.push(Observation {
                id: KEY.into(),
                name: Some("DAREU A950 Wing".into()),
                kind: Some(Kind::Mouse),
                state: State::Battery { percent, charging },
            });
            return;
        }
        if pid == RECEIVER {
            diag::event(KEY, "no reply (mouse asleep or off)");
            out.push(Observation {
                id: KEY.into(),
                name: Some("DAREU A950 Wing".into()),
                kind: Some(Kind::Mouse),
                state: State::Unreachable,
            });
        }
    }
}

/// 17-byte report 0x08, command 04, the receiver's frames sum to 0x55.
fn parse_a950(r: &[u8]) -> Option<(u8, bool, u16)> {
    if r.len() != LEN || r[0] != REPORT_ID || r[1] != GET_POWER || r[6] > 100 {
        return None;
    }
    if r.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)) != 0x55 {
        return None;
    }
    Some((r[6], r[7] != 0, u16::from_be_bytes([r[8], r[9]])))
}

// --- EM901X / Sharkoon Skiller SGM50W -------------------------------------------------------

fn em901x(api: &HidApi, out: &mut Vec<Observation>, claimed: &mut HashSet<(u16, u16)>) {
    let mut cable_read = false;
    for pid in [0x1090u16, 0x1074, 0x1125] {
        if claimed.contains(&(VID, pid)) || (cable_read && pid == 0x1074) {
            continue;
        }
        let inputs: Vec<_> = api
            .device_list()
            .filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == pid
                    && d.interface_number() == 1
                    && d.usage_page() == 0xFF01
                    && d.usage() == 0
            })
            .collect();
        let features: Vec<_> = api
            .device_list()
            .filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == pid
                    && d.interface_number() == 1
                    && d.usage_page() == 0xFF02
                    && d.usage() == 2
            })
            .collect();
        if inputs.len() != 1 || features.len() != 1 {
            continue;
        }
        let (Ok(input), Ok(feature)) = (api.open_path(inputs[0].path()), api.open_path(features[0].path())) else {
            continue;
        };
        let (Some(input_desc), Some(feature_desc)) = (probe::descriptor(&input), probe::descriptor(&feature)) else {
            continue;
        };
        if probe::input_len(&input_desc, 0x09) != Some(LEN) || probe::feature_len(&feature_desc, REPORT_ID) != Some(LEN) {
            continue;
        }

        let key = format!("dareu:260d:{pid:04x}");
        let mut req = [0u8; LEN];
        req[0] = REPORT_ID;
        req[1] = GET_POWER;
        req[LEN - 1] = checksum(&req[..LEN - 1]);
        flush(&input);
        if let Err(e) = feature.send_feature_report(&req) {
            diag::event(&key, format!("i/o failed: {e}"));
            continue;
        }

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut buf = [0u8; 64];
        let mut answered = false;
        let mut state = State::Unreachable;
        for _ in 0..32 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
            match input.read_timeout(&mut buf, (left.as_millis() as i32).clamp(1, 250)) {
                Ok(0) => {}
                Ok(n) => {
                    if let Some((percent, charging)) = parse_em901x(&buf[..n]) {
                        diag::event(&key, format!("battery via feature 08 (level {percent})"));
                        state = State::Battery { percent, charging };
                        answered = true;
                        break;
                    }
                }
                Err(e) => {
                    diag::event(&key, format!("i/o failed: {e}"));
                    break;
                }
            }
        }
        if answered {
            cable_read = pid != 0x1125;
        } else {
            diag::event(&key, "no reply (mouse asleep or off)");
        }
        let name = if pid == 0x1125 { "Sharkoon Skiller SGM50W" } else { "DAREU EM901X" };
        out.push(Observation { id: key, name: Some(name.into()), kind: Some(Kind::Mouse), state });
    }
}

/// Reply `<09> <04> <00> … [6] level [7] charging`; no reply checksum rule is established.
fn parse_em901x(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() != LEN || r[0] != 0x09 || r[1] != GET_POWER || r[2] != 0 || r[6] > 100 {
        return None;
    }
    Some((r[6], r[7] != 0))
}

// --- shared ---------------------------------------------------------------------------------

/// The receiver's frames sum to 0x55 (mod 256).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_power_request_checksum() {
        let mut req = [0u8; LEN];
        req[0] = REPORT_ID;
        req[1] = GET_POWER;
        assert_eq!(checksum(&req[..LEN - 1]), 0x49);
    }

    #[test]
    fn a950_reply_checks_sum_and_parse() {
        // Same Compx layout as the captured Cherry XTRFY frame: 95%, on battery, 4084 mV.
        let frame = [0x08, 0x04, 0x00, 0x00, 0x00, 0x02, 0x5f, 0x00, 0x0f, 0xf4, 0, 0, 0, 0, 0, 0, 0xe5];
        assert_eq!(checksum(&frame[..16]), 0xe5);
        assert_eq!(parse_a950(&frame), Some((95, false, 4084)));
        let mut bad = frame;
        bad[16] ^= 0xFF;
        assert_eq!(parse_a950(&bad), None);
        bad[16] = 0xe5;
        bad[1] = 0x03;
        assert_eq!(parse_a950(&bad), None);
    }

    #[test]
    fn em901x_reply_has_no_checksum_check() {
        let mut frame = [0u8; LEN];
        frame[0] = 0x09;
        frame[1] = 0x04;
        frame[6] = 87;
        frame[7] = 1;
        assert_eq!(parse_em901x(&frame), Some((87, true)));
        frame[2] = 1;
        assert_eq!(parse_em901x(&frame), None);
        frame[2] = 0;
        frame[6] = 101;
        assert_eq!(parse_em901x(&frame), None);
    }

    #[test]
    fn togran_scales_to_the_reported_maximum() {
        let mut r = [0u8; 65];
        r[1] = 0x02;
        r[3] = 0x07;
        r[4] = 0x80;
        r[7] = 1;
        r[8] = 80;
        r[9] = 100;
        assert_eq!(togran_battery(&r), Some((80, true)));
        r[9] = 0; // no maximum reported: assume 100
        assert_eq!(togran_battery(&r), Some((80, true)));
        r[8] = 50;
        r[9] = 40;
        assert_eq!(togran_battery(&r), None);
        r[8] = 20;
        assert_eq!(togran_battery(&r), Some((50, true)));
    }
}
