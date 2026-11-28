//! Kysona, Pulsar, ATK/VXE, G-Wolves, Hitscan, HECATE and Redragon mice that share the same
//! 17-byte vendor protocol.
//!
//! Request: output report 0x08, command in byte 1, checksum in the last byte
//! (`0x55` minus the sum of the first 16). Reply: input report 0x08 with the same checksum.
//! Command 0x03 answers the link state in byte 6 (1 = the mouse is linked), command 0x04 the
//! battery: byte 6 the level, byte 7 the charging flag, bytes 8-9 the cell voltage in mV.
//!
//! Protocol and device ids from Wireless-Device-Battery-Monitor (docs and drivers/kysona.cpp),
//! which tested several of the receivers on hardware. Only these two read-only queries are sent.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::checksum55 as checksum;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const VIDS: &[u16] = &[0x3554, 0x373B, 0x3770, 0x33E4, 0x3710, 0x35BB];
const REPORT_ID: u8 = 0x08;
const ONLINE: u8 = 0x03;
const BATTERY: u8 = 0x04;
const LEN: usize = 17;
const REPLY_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct Kysona {
    /// Zero-byte reads do not prove a device is gone; remember which keys answered once so a
    /// sleeping mouse keeps its last value instead of disappearing.
    seen: HashSet<String>,
}

impl Kysona {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out: Vec<Observation> = Vec::new();
        let mut done: HashSet<(u16, u16)> = HashSet::new();
        let mut answered: HashSet<String> = HashSet::new();

        for info in api.device_list() {
            let (vid, pid) = (info.vendor_id(), info.product_id());
            if !VIDS.contains(&vid) || excluded(vid, pid) || !done.insert((vid, pid)) {
                continue;
            }
            let mut candidates: Vec<_> =
                api.device_list().filter(|d| d.vendor_id() == vid && d.product_id() == pid).collect();
            // The battery channel is FF02:0002 where it exists; try it before the other pages.
            candidates.sort_by_key(|d| !(d.usage_page() == 0xFF02 && d.usage() == 0x0002));

            for c in candidates {
                let Ok(dev) = api.open_path(c.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                let Some(out_len) = probe::output_len(&desc, REPORT_ID) else { continue };
                if out_len < LEN || probe::input_len(&desc, REPORT_ID).unwrap_or(0) < LEN {
                    continue;
                }
                let online = match query(&dev, out_len, ONLINE) {
                    Ok(Some(r)) => Some(r[6] != 0),
                    Ok(None) => None, // not this protocol, or the receiver is silent
                    Err(e) => {
                        diag::event("kysona", format!("{vid:04x}:{pid:04x} i/o failed: {e}"));
                        break;
                    }
                };
                if online == Some(false) {
                    // The dongle says the mouse is off/asleep. Report nothing so a known mouse
                    // keeps its last reading marked asleep.
                    break;
                }
                let state = match query(&dev, out_len, BATTERY) {
                    Ok(Some(r)) => match battery(&r) {
                        Some((percent, charging)) => State::Battery { percent, charging },
                        None => State::Unreachable,
                    },
                    // Some receivers answer the battery without the link-state query.
                    Ok(None) if online == Some(true) => State::Unreachable,
                    Ok(None) => continue,
                    Err(e) => {
                        diag::event("kysona", format!("{vid:04x}:{pid:04x} i/o failed: {e}"));
                        break;
                    }
                };
                let name = device_name(vid, pid, c.product_string().unwrap_or(""));
                let key = format!("kysona:{:04x}:{:04x}", key_vid(vid, pid).0, key_vid(vid, pid).1);
                let kind = match Kind::from_name(&name) {
                    Kind::Other => Kind::Mouse,
                    k => k,
                };
                answered.insert(key.clone());
                self.seen.insert(key.clone());
                merge(&mut out, Observation { id: key, name: Some(name), kind: Some(kind), state });
                break;
            }
        }

        // After the first answer, a receiver that stops answering is asleep, not gone.
        let mut present: Vec<(u16, u16)> = api.device_list().map(|d| (d.vendor_id(), d.product_id())).collect();
        present.sort_unstable();
        present.dedup();
        for key in self.seen.iter().filter(|k| !answered.contains(*k)) {
            let name = key_name(key, &present, api);
            if let Some(name) = name {
                out.push(Observation { id: key.clone(), name: Some(name), kind: Some(Kind::Mouse), state: State::Unreachable });
            }
        }
        out
    }
}

fn key_name(key: &str, present: &[(u16, u16)], api: &HidApi) -> Option<String> {
    let rest = key.strip_prefix("kysona:")?;
    let (v, p) = rest.split_once(':')?;
    let (vid, pid) = (u16::from_str_radix(v, 16).ok()?, u16::from_str_radix(p, 16).ok()?);
    if !present.contains(&(vid, pid)) {
        return None;
    }
    let product = api
        .device_list()
        .find(|d| d.vendor_id() == vid && d.product_id() == pid)
        .and_then(|d| d.product_string())
        .unwrap_or("");
    Some(device_name(vid, pid, product))
}

fn merge(out: &mut Vec<Observation>, o: Observation) {
    if let Some(old) = out.iter_mut().find(|x| x.id == o.id) {
        let better = match (&old.state, &o.state) {
            (_, State::Battery { charging: true, .. }) => true,
            (State::Unreachable, State::Battery { .. }) => true,
            _ => false,
        };
        if better {
            old.name = o.name;
            old.kind = o.kind;
            old.state = o.state;
        }
        return;
    }
    out.push(o);
}

fn query(dev: &HidDevice, out_len: usize, cmd: u8) -> hidapi::HidResult<Option<[u8; LEN]>> {
    let mut req = vec![0u8; out_len.max(LEN)];
    req[0] = REPORT_ID;
    req[1] = cmd;
    req[LEN - 1] = checksum(&req[..LEN - 1]);

    let mut buf = [0u8; 64];
    // A frame queued before the request isn't its answer.
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    dev.write(&req)?;

    let deadline = Instant::now() + REPLY_TIMEOUT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(0) => break,
            Ok(n) => {
                if n >= LEN && buf[0] == REPORT_ID && buf[1] == cmd && checksum(&buf[..LEN - 1]) == buf[LEN - 1] {
                    let mut r = [0u8; LEN];
                    r.copy_from_slice(&buf[..LEN]);
                    return Ok(Some(r));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// Byte 6 is the level, byte 7 the charging flag. A receiver answers 0 before it can hear the
/// mouse; a real 0% mouse is switched off, so that is not a reading.
fn battery(r: &[u8; LEN]) -> Option<(u8, bool)> {
    let charging = r[7] != 0;
    let level = r[6];
    if level > 100 || (level == 0 && !charging) {
        return None;
    }
    Some((level, charging))
}

/// ATK headsets and the 8K Nano dongle speak other protocols; BY keyboards share 3554:FA09 and
/// FB40; HECATE headsets share 35BB with its mice.
fn excluded(vid: u16, pid: u16) -> bool {
    match vid {
        0x373B => matches!(
            pid,
            0x11CA | 0x11CB | 0x1203 | 0x1204 | 0x1178 | 0x1216 | 0x124C | 0x124F | 0x1155 | 0x1154 | 0x1012
                | 0x107C | 0x111F | 0x1053 | 0x1151 | 0x1193 | 0x11FF
        ),
        0x3554 => matches!(pid, 0xFA09 | 0xFB40 | 0xF512 | 0xFC0A),
        0x3710 => matches!(pid, 0x3402 | 0x3406 | 0x5402 | 0x5403 | 0x5404 | 0x5405 | 0x7401),
        0x3770 => matches!(pid, 0x0136 | 0x0194),
        0x35BB => !matches!(pid, 0xD3FA | 0xD3FB | 0xD3FC | 0xD3FD),
        _ => false,
    }
}

/// A cable reports under its receiver's key, so charging updates the same row.
fn key_vid(vid: u16, pid: u16) -> (u16, u16) {
    match (vid, pid) {
        (0x3554, 0xFB43) => (0x3554, 0xFB44),
        (0x3554, 0xF58A) | (0x3554, 0xF58C) => (0x3554, 0xF58A),
        (0x373B, 0x1272) => (0x3554, 0xF58A),
        (0x373B, 0x1085) | (0x3554, 0xF58F) => (0x373B, 0x1085),
        (0x3554, 0xF505) | (0x3554, 0xF506) => (0x3554, 0xF505),
        _ => (vid, pid),
    }
}

fn device_name(vid: u16, pid: u16, product: &str) -> String {
    if vid == 0x33E4 {
        return gwolves_name(pid).unwrap_or_else(|| clean(product, "G-Wolves wireless mouse"));
    }
    if vid == 0x3554 && matches!(pid, 0xF55D | 0xF5D5 | 0xF55E) {
        return "Redragon wireless mouse".into();
    }
    if (vid == 0x3554 && matches!(pid, 0xF58A | 0xF58C | 0xF58E | 0xF58F | 0xF505 | 0xF506))
        || (vid == 0x373B && matches!(pid, 0x1085 | 0x1272))
    {
        return "VXE Dragonfly R1".into();
    }
    if vid == 0x3554 && matches!(pid, 0xFB43 | 0xFB44) {
        return "Attack Shark R11 Ultra".into();
    }
    if vid == 0x3770 {
        return "Hitscan Hyperlight".into();
    }
    if vid == 0x35BB {
        return "HECATE mouse".into();
    }
    clean(product, if vid == 0x373B { "ATK wireless mouse" } else { "Wireless mouse" })
}

fn clean(product: &str, fallback: &'static str) -> String {
    let mut name = product.trim();
    for suffix in [" Dongle", " Receiver", " 2.4G", " Wireless Receiver"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped;
            break;
        }
    }
    if name.is_empty() { fallback.into() } else { name.into() }
}

/// G-Wolves model names from its own device list (mouse.xyz, Config/env-models.json).
fn gwolves_name(pid: u16) -> Option<String> {
    let name = match pid {
        0x3808 | 0x3817 => "G-Wolves HTM Plus",
        0x6808 | 0x6817 => "G-Wolves HSK Pro 2.0",
        0x5608 | 0x5617 | 0x5618 | 0x5619 => "G-Wolves HTXU",
        0x3608 | 0x3617 | 0x3619 => "G-Wolves Fenrir Pro",
        0x3908 | 0x3917 => "G-Wolves VUK",
        0x7904 | 0x7913 => "G-Wolves HT-S2",
        0x3708 | 0x3717 => "G-Wolves Fenrir Max",
        0x5308 | 0x5317 => "G-Wolves HTS Ultra",
        0x7704 | 0x7713 => "G-Wolves HTR",
        0x3508 | 0x3517 => "G-Wolves Fenrir",
        0x7908 | 0x7917 => "G-Wolves HT-S2 Pro",
        0x5808 | 0x5817 => "G-Wolves HSK Pro",
        0x2708 | 0x2717 | 0x2719 => "G-Wolves HTX Mini",
        0x5408 | 0x5417 | 0x5418 | 0x5419 => "G-Wolves HTS Plus",
        0x5708 | 0x5717 => "G-Wolves HTX",
        0x5908 | 0x5917 => "G-Wolves HSK Plus",
        0x7204 | 0x7203 => "G-Wolves HSK Lite",
        0x7708 | 0x7717 => "G-Wolves HTR Pro",
        0x5804 | 0x5803 => "G-Wolves HSK Pro ACE",
        0x5404 | 0x5403 => "G-Wolves HTS Plus ACE",
        0x5704 | 0x5703 => "G-Wolves HTX ACE",
        0x5904 | 0x5903 => "G-Wolves HSK Plus ACE",
        0x4718 | 0x4719 => "G-Wolves Lycan",
        0x4219 => "G-Wolves WARG",
        0x5219 => "G-Wolves HTS Plus Pro",
        0x3519 => "G-Wolves Fenrir Asym",
        0x2619 => "G-Wolves HTX Mini Asym",
        0x1619 => "G-Wolves Fenrir Max Asym",
        0x2119 => "G-Wolves VUK Mini",
        _ => return None,
    };
    Some(name.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(cmd: u8, level: u8, charging: u8) -> [u8; LEN] {
        let mut r = [0u8; LEN];
        r[0] = REPORT_ID;
        r[1] = cmd;
        r[6] = level;
        r[7] = charging;
        r[LEN - 1] = checksum(&r[..LEN - 1]);
        r
    }

    #[test]
    fn checksum_matches_get_power() {
        let mut req = [0u8; LEN];
        req[0] = REPORT_ID;
        req[1] = BATTERY;
        req[LEN - 1] = checksum(&req[..LEN - 1]);
        assert_eq!(req[LEN - 1], 0x49);
        assert_eq!(checksum(&req[..LEN - 1]), req[LEN - 1]);
    }

    #[test]
    fn parses_battery_and_rejects_sleeping_zero() {
        assert_eq!(battery(&reply(BATTERY, 87, 0)), Some((87, false)));
        assert_eq!(battery(&reply(BATTERY, 0, 1)), Some((0, true)));
        assert_eq!(battery(&reply(BATTERY, 0, 0)), None);
        assert_eq!(battery(&reply(BATTERY, 101, 0)), None);
    }

    #[test]
    fn cables_share_the_receiver_key() {
        assert_eq!(key_vid(0x3554, 0xFB43), (0x3554, 0xFB44));
        assert_eq!(key_vid(0x373B, 0x1272), (0x3554, 0xF58A));
        assert_eq!(key_vid(0x373B, 0x1085), (0x373B, 0x1085));
        assert_eq!(key_vid(0x3554, 0xF505), (0x3554, 0xF505));
    }

    #[test]
    fn names_the_known_receivers() {
        assert_eq!(device_name(0x3770, 0x0200, "Hitscan USB Receiver"), "Hitscan Hyperlight");
        assert_eq!(device_name(0x35BB, 0xD3FB, "HECATE"), "HECATE mouse");
        assert_eq!(device_name(0x373B, 0x1085, "VXE NordicMouse 1K Dongle"), "VXE Dragonfly R1");
        assert_eq!(device_name(0x3554, 0xF55D, "2.4G Wireless Receiver"), "Redragon wireless mouse");
        assert_eq!(device_name(0x33E4, 0x3808, ""), "G-Wolves HTM Plus");
        assert_eq!(device_name(0x3554, 0x1234, " Kysona M600 2.4G "), "Kysona M600");
    }
}
