//! Apple AirPods / Beats (AirPods 1-5, Pro 1-3, Max, Beats) battery levels.
//!
//! Upstream reads two transports:
//! 1. Apple Continuity "proximity pairing" BLE advertisements (company 0x004C, type 0x07,
//!    10% steps) through the shared advertisement watcher.
//! 2. Apple Accessory Protocol over an installed L2CAP bridge driver (NTPodsAAP / PodPilotAap /
//!    MagicAAP), which gives true 1% levels.
//!
//! Transport 1 is wired to the shared watcher in `ble_adverts`. Transport 2 needs a third-party
//! kernel L2CAP bridge driver plus a write handshake, so it is not used: the AAP frame decoders
//! below are ported anyway, so 1% levels are one transport away.
//!
//! Layouts from OpenPods / AirPodsDesktop / librepods, as in upstream airpods_protocol.h.
#![allow(dead_code)] // parsers and tables land before the transport that needs them

use crate::ble_adverts;
use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::wide;
use std::collections::{HashMap, HashSet};
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::time::Instant;
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::Properties::DEVPROPKEY;

/// Apple's Bluetooth company id / USB vendor id.
const VID_APPLE: u16 = 0x004C;
/// Apple audio model ids are 0x20xx; phones, tablets and Macs are not.
const PID_APPLE_AUDIO: u16 = 0x2000;

/// DEVPKEY_Device_ContainerId: groups a device's profile nodes into one device.
const CONTAINER_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x8C7ED206_3F8A_4827_B3AB_AE9E1FAEFC6C),
    pid: 2,
};
const CONNECTED_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x83DA6326_97A6_4088_9453_A1923F573B29),
    pid: 15,
};
const FRIENDLY_NAME_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0xA45C254E_DF1C_4EFD_8020_67D146A850E0),
    pid: 14,
};

#[derive(Default)]
pub struct AirPods;

impl AirPods {
    pub fn scan(&mut self) -> Vec<Observation> {
        let devices = paired_apple_audio();
        if devices.is_empty() {
            return Vec::new();
        }
        if ble_adverts::ensure() {
            // Just started: the headphones broadcast about every 1-2 s.
            for _ in 0..15 {
                if !ble_adverts::apple().is_empty() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }

        let now = Instant::now();
        let adverts: Vec<Advert> = ble_adverts::apple()
            .iter()
            .filter_map(|a| {
                let mut parsed = Advert::parse(&a.data)?;
                parsed.rssi = a.rssi;
                parsed.age_ms = now.saturating_duration_since(a.at).as_millis() as u64;
                Some(parsed)
            })
            .collect();

        let mut out = Vec::new();
        let mut used_pid = HashSet::new();
        for d in devices {
            // Several pairs of the same model: a disconnected one reuses the connected one's
            // advertisement otherwise (random BLE addresses can't be told apart).
            if d.pid != 0 && !d.connected && used_pid.contains(&d.pid) {
                continue;
            }
            let name = if d.name.is_empty() { "AirPods".to_string() } else { d.name };
            let Some((percent, charging)) = pick_best(&adverts, d.pid, d.connected).and_then(|a| battery(&a)) else {
                diag::event(
                    &format!("airpods:{}", diag::bt_tag(d.addr)),
                    "paired, no Apple 0x07 advertisement heard yet",
                );
                continue;
            };
            if d.pid != 0 {
                used_pid.insert(d.pid);
            }
            out.push(Observation {
                id: format!("airpods:{:012x}", d.addr),
                // Beats names carry no earbud/headphone keyword; treat any other Apple audio
                // device as a headset, like upstream.
                kind: Some(match Kind::from_name(&name) {
                    Kind::Other => Kind::Headset,
                    k => k,
                }),
                name: Some(name),
                state: State::Battery { percent: percent.min(100), charging },
            });
        }
        out
    }
}

/// A paired Apple audio device, one per Windows device container.
#[derive(Default)]
struct AppleDevice {
    name: String,
    /// Apple model id from the "_VID&0001004C_PID&xxxx" service nodes.
    pid: u16,
    /// 48-bit Classic Bluetooth address from "BTHENUM\DEV_<12 hex>\...".
    addr: u64,
    connected: bool,
    apple: bool,
}

fn paired_apple_audio() -> Vec<AppleDevice> {
    let mut by_container: HashMap<[u8; 16], AppleDevice> = HashMap::new();
    let enumerator = wide("BTHENUM");
    // SAFETY: handles come from SetupAPI and every buffer is sized before the call that fills it.
    unsafe {
        let set = SetupDiGetClassDevsW(null(), enumerator.as_ptr(), null_mut(), DIGCF_ALLCLASSES | DIGCF_PRESENT);
        if set as isize == -1 {
            return Vec::new();
        }
        let mut i = 0;
        loop {
            let mut info: SP_DEVINFO_DATA = zeroed();
            info.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiEnumDeviceInfo(set, i, &mut info) == 0 {
                break;
            }
            i += 1;
            let mut id = [0u16; 512];
            if SetupDiGetDeviceInstanceIdW(set, &info, id.as_mut_ptr(), id.len() as u32, null_mut()) == 0 {
                continue;
            }
            let len = id.iter().position(|c| *c == 0).unwrap_or(id.len());
            let instance = String::from_utf16_lossy(&id[..len]);
            let Some(container) = property(set, &info, &CONTAINER_KEY).and_then(|b| b.get(..16).and_then(|s| <[u8; 16]>::try_from(s).ok())) else {
                continue;
            };
            let d = by_container.entry(container).or_default();
            let upper = instance.to_ascii_uppercase();
            if let Some(vid) = upper.find("_VID&0001004C") {
                d.apple = true;
                if let Some(pid) = upper[vid..].find("_PID&") {
                    d.pid = upper[vid + pid + 5..].get(..4).and_then(|h| u16::from_str_radix(h, 16).ok()).unwrap_or(0);
                }
            }
            if upper.starts_with("BTHENUM\\DEV_") {
                if let Some(addr) = parse_address12(&instance[12..]) {
                    d.addr = addr;
                }
                if let Some(v) = property(set, &info, &FRIENDLY_NAME_KEY) {
                    d.name = string_prop(&v);
                }
                if let Some(v) = property(set, &info, &CONNECTED_KEY) {
                    d.connected = v.first().is_some_and(|b| *b != 0);
                }
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }

    let mut out: Vec<AppleDevice> = by_container
        .into_values()
        .filter(|d| {
            let n = d.name.to_ascii_uppercase();
            let looks_apple = n.contains("AIRPODS") || n.contains("BEATS");
            // Keep paired 0x20xx devices even when Classic Bluetooth is down: earbuds charging on
            // the desk still broadcast. Devices other drivers report need a connected node.
            (d.apple && (d.pid & 0xFF00) == PID_APPLE_AUDIO) || (d.connected && looks_apple && d.pid == 0)
        })
        .collect();
    out.sort_by_key(|d| (!d.connected, d.addr));
    out
}

/// "BTHENUM\DEV_F5CFD77BCB0B\..." -> 0xF5CFD77BCB0B
fn parse_address12(s: &str) -> Option<u64> {
    let mac = s.get(..12)?;
    mac.bytes().all(|b| b.is_ascii_hexdigit()).then(|| u64::from_str_radix(mac, 16).ok())?
}

fn string_prop(b: &[u8]) -> String {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|u| *u != 0).collect();
    String::from_utf16_lossy(&units)
}

fn property(set: HDEVINFO, info: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<Vec<u8>> {
    let mut buf = [0u8; 512];
    let (mut ptype, mut size) = (0u32, 0u32);
    // SAFETY: `buf` is valid for `buf.len()` bytes and `size` receives the actual length.
    let ok = unsafe { SetupDiGetDevicePropertyW(set, info, key, &mut ptype, buf.as_mut_ptr(), buf.len() as u32, &mut size, 0) };
    (ok != 0).then(|| buf[..(size as usize).min(buf.len())].to_vec())
}

// ---- Apple Accessory Protocol (AAP) battery notifications ----

pub const AAP_PSM: u16 = 0x1001;
pub const AAP_HEADER: [u8; 4] = [0x04, 0x00, 0x04, 0x00];
/// Sent first over a bridge: handshake, host capabilities, notification request.
pub const AAP_HANDSHAKE: [u8; 16] = [
    0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
pub const AAP_HOST_CAPABILITIES: [u8; 14] = [0x04, 0x00, 0x04, 0x00, 0x4D, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
pub const AAP_REQUEST_NOTIFICATIONS: [u8; 10] = [0x04, 0x00, 0x04, 0x00, 0x0F, 0x00, 0xFF, 0xFF, 0xFF, 0xFF];

/// Battery levels from AAP: 1% steps, unlike the 10% advertisement nibbles.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct AapBattery {
    pub single_model: bool,
    pub single: Option<u8>,
    pub left: Option<u8>,
    pub right: Option<u8>,
    pub case: Option<u8>,
    pub single_charging: bool,
    pub left_charging: bool,
    pub right_charging: bool,
    pub case_charging: bool,
}

impl AapBattery {
    pub fn has_any(&self) -> bool {
        self.single.is_some() || self.left.is_some() || self.right.is_some() || self.case.is_some()
    }

    /// Keeps components a partial frame omitted or reported as disconnected.
    pub fn merge(&mut self, src: &AapBattery) {
        if src.single_model {
            self.single_model = true;
        }
        for (dst, level, charging, src_charging) in [
            (&mut self.single, src.single, &mut self.single_charging, src.single_charging),
            (&mut self.left, src.left, &mut self.left_charging, src.left_charging),
            (&mut self.right, src.right, &mut self.right_charging, src.right_charging),
            (&mut self.case, src.case, &mut self.case_charging, src.case_charging),
        ] {
            if let Some(p) = level {
                *dst = Some(p);
                *charging = src_charging;
            }
        }
    }
}

/// `[04 00 04 00] [04 00] [count] { [type] [01] [level 0..100] [status] [01] } * count`
/// Type 0x01 single, 0x02 right, 0x04 left, 0x08 case.
fn is_aap_header(s: &[u8]) -> bool {
    s.len() >= 4 && s[0] == 0x04 && s[1] == 0x00 && s[2] == 0x04 && s[3] == 0x00
}

/// Parses one complete AAP battery notification frame.
pub fn parse_aap_battery(data: &[u8]) -> Option<AapBattery> {
    if data.len() < 7 || !is_aap_header(data) || data[4] != 0x04 || data[5] != 0x00 {
        return None;
    }
    let count = data[6] as usize;
    if !(1..=4).contains(&count) || data.len() < 7 + count * 5 {
        return None;
    }
    let mut parsed = AapBattery::default();
    for i in 0..count {
        let off = 7 + i * 5;
        let (kind, level, status) = (data[off], data[off + 2], data[off + 3]);
        // 0x04 disconnected, 0xFF absent, (0x00, 0) the uninitialized sentinel.
        if status == 0x04 || level == 0xFF || level > 100 || (status == 0x00 && level == 0) {
            continue;
        }
        // 0x01 charging, 0x05 charging in the case.
        let charging = status == 0x01 || status == 0x05;
        match kind {
            0x01 => {
                parsed.single_model = true;
                parsed.single = Some(level);
                parsed.single_charging = charging;
            }
            0x02 => {
                parsed.right = Some(level);
                parsed.right_charging = charging;
            }
            0x04 => {
                parsed.left = Some(level);
                parsed.left_charging = charging;
            }
            0x08 => {
                parsed.case = Some(level);
                parsed.case_charging = charging;
            }
            _ => {}
        }
    }
    parsed.has_any().then_some(parsed)
}

/// Pulls every complete battery frame out of a byte stream (the MagicAAP bridge is a stream, not
/// packets), merging them, and keeps the incomplete tail for the next read.
pub fn extract_aap_packets(buf: &mut Vec<u8>, merged: &mut AapBattery) -> bool {
    const MAX_BUFFER: usize = 4096;
    let mut updated = false;
    let mut cursor = 0;
    let mut wait_more = false;
    while cursor + 4 <= buf.len() {
        let Some(pos) = buf[cursor..].windows(4).position(|w| is_aap_header(w)).map(|p| p + cursor) else {
            cursor = buf.len();
            break;
        };
        if pos + 7 > buf.len() {
            buf.drain(..pos);
            wait_more = true;
            break;
        }
        if buf[pos + 4] == 0x04 && buf[pos + 5] == 0x00 {
            let count = buf[pos + 6] as usize;
            if (1..=4).contains(&count) {
                let pkt_len = 7 + count * 5;
                if pos + pkt_len > buf.len() {
                    buf.drain(..pos);
                    wait_more = true;
                    break;
                }
                if let Some(parsed) = parse_aap_battery(&buf[pos..pos + pkt_len]) {
                    merged.merge(&parsed);
                    updated = true;
                }
                cursor = pos + pkt_len;
                continue;
            }
        }
        cursor = pos + 4;
    }
    if !wait_more {
        // Keep a trailing prefix of the header for the next read.
        let avail = buf.len() - cursor;
        let mut keep = 0;
        for k in (1..=avail.min(3)).rev() {
            if buf.ends_with(&AAP_HEADER[..k]) {
                keep = k;
                break;
            }
        }
        if keep > 0 {
            let n = buf.len() - keep;
            buf.drain(..n);
        } else {
            buf.clear();
        }
    }
    if buf.len() > MAX_BUFFER {
        let n = buf.len() - 3;
        buf.drain(..n);
    }
    updated
}

// ---- Apple BLE proximity advertisements ----

/// One decoded Apple 0x07 proximity message. Levels are 10% steps (`None` = not reported).
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Advert {
    pub model: u16,
    pub single: bool,
    pub left_broadcasts: bool,
    pub left: Option<u8>,
    pub right: Option<u8>,
    pub case: Option<u8>,
    pub left_chg: bool,
    pub right_chg: bool,
    pub box_chg: bool,
    pub rssi: i8,
    /// Milliseconds since this advertisement was heard (set by the caller).
    pub age_ms: u64,
}

/// Single-battery headphones and neckbands put their level in the low nibble of byte 6 and leave
/// the other nibbles 0, so decoding them as dual-bud would report the unused side as 0%.
fn is_single_battery_model(model: u16) -> bool {
    matches!(
        model,
        0x200A // AirPods Max (Lightning)
            | 0x201F // AirPods Max (USB-C)
            | 0x202D // AirPods Max 2
            | 0x2003 // Powerbeats 3
            | 0x2005 // BeatsX
            | 0x2006 // Beats Solo 3
            | 0x2009 // Beats Studio 3
            | 0x200C // Beats Solo Pro
            | 0x200D // Powerbeats 4
            | 0x2010 // Beats Flex
            | 0x2017 // Beats Studio Pro
            | 0x2025 // Beats Solo 4
    )
}

fn level(nibble: u8) -> Option<u8> {
    (nibble <= 10).then_some(nibble * 10)
}

fn parse_proximity(d: &[u8]) -> Option<Advert> {
    // d[1] == 0x19 (25 bytes follow), d[2] == 0x01 paired status (0x00 is pairing mode).
    if d.len() < 27 || d[0] != 0x07 || d[1] != 0x19 || d[2] != 0x01 {
        return None;
    }
    let model = u16::from_le_bytes([d[3], d[4]]);
    if model & 0xFF00 != PID_APPLE_AUDIO {
        return None;
    }
    let single = is_single_battery_model(model);
    let left_broadcasts = d[5] & 0x20 != 0;
    let cur = d[6] & 0x0F;
    let other = d[6] >> 4;
    let chg = d[7] >> 4;
    let mut a = Advert { model, single, left_broadcasts, ..Default::default() };
    if single {
        let mut percent = level(cur);
        let mut charging = chg & 0x01 != 0;
        // Which nibble holds the level depends on which bud is in the case.
        if cur == 0 && (1..=10).contains(&other) {
            percent = level(other);
            charging = chg & 0x02 != 0;
        }
        a.left = percent;
        a.left_chg = charging;
        return Some(a);
    }
    a.left = level(if left_broadcasts { cur } else { other });
    a.right = level(if left_broadcasts { other } else { cur });
    a.left_chg = a.left.is_some() && chg & (if left_broadcasts { 0x01 } else { 0x02 }) != 0;
    a.right_chg = a.right.is_some() && chg & (if left_broadcasts { 0x02 } else { 0x01 }) != 0;
    a.case = level(d[7] & 0x0F);
    a.box_chg = a.case.is_some() && chg & 0x04 != 0;
    (a.left.is_some() || a.right.is_some() || a.case.is_some()).then_some(a)
}

impl Advert {
    /// Walks the Apple Continuity TLV sequence in case another message precedes 0x07.
    pub fn parse(data: &[u8]) -> Option<Advert> {
        let mut i = 0;
        while i + 1 < data.len() {
            let len = data[i + 1] as usize;
            if i + 2 + len > data.len() {
                break;
            }
            if data[i] == 0x07 {
                if let Some(a) = parse_proximity(&data[i..i + 2 + len]) {
                    return Some(a);
                }
            }
            i += 2 + len;
        }
        None
    }
}

/// Best advertisement for a paired device: fresh ones compete on RSSI for 10 s, stale high-RSSI
/// addresses yield, otherwise the newest wins. Missing sides are filled from nearby advertisements
/// of the same model (AirPods rotate their BLE address, so one message rarely has all three).
fn pick_best(adverts: &[Advert], pid: u16, connected: bool) -> Option<Advert> {
    const ACTIVE_MS: u64 = 10_000;
    let max_age = if connected { 5 * 60 * 1000 } else { 90 * 1000 };
    let min_rssi: i16 = if connected { -90 } else { -80 };

    let mut best: Option<Advert> = None;
    for a in adverts {
        if pid != 0 && a.model != pid {
            continue;
        }
        if a.age_ms > max_age || (a.rssi as i16) < min_rssi {
            continue;
        }
        let active = a.age_ms < ACTIVE_MS;
        let take = match best {
            None => true,
            Some(b) => {
                let best_active = b.age_ms < ACTIVE_MS;
                if active && !best_active {
                    true
                } else if active && best_active {
                    a.rssi > b.rssi || (a.rssi == b.rssi && a.age_ms < b.age_ms)
                } else if !active && !best_active {
                    a.age_ms < b.age_ms
                } else {
                    false
                }
            }
        };
        if take {
            best = Some(*a);
        }
    }

    let mut best = best?;
    if best.single {
        return Some(best);
    }
    let (need_left, need_right, need_box) = (best.left.is_none(), best.right.is_none(), best.case.is_none());
    let (mut fill_left, mut fill_right, mut fill_box) = (None, None, None);
    for a in adverts {
        if a.model != best.model || a.single || (a.rssi as i16) < min_rssi {
            continue;
        }
        if (a.rssi as i16 - best.rssi as i16).abs() > 15 || best.age_ms.abs_diff(a.age_ms) > 15_000 {
            continue;
        }
        if need_left && a.left.is_some() && fill_left.is_none_or(|t| a.age_ms <= t) {
            best.left = a.left;
            best.left_chg = a.left_chg;
            fill_left = Some(a.age_ms);
        }
        if need_right && a.right.is_some() && fill_right.is_none_or(|t| a.age_ms <= t) {
            best.right = a.right;
            best.right_chg = a.right_chg;
            fill_right = Some(a.age_ms);
        }
        if need_box && a.case.is_some() && fill_box.is_none_or(|t| a.age_ms <= t) {
            best.case = a.case;
            best.box_chg = a.box_chg;
            fill_box = Some(a.age_ms);
        }
    }
    Some(best)
}

/// The level the taskbar shows: the lower discharging bud, else the lower charging one, else the
/// case. `charging` is false whenever a discharging bud exists, like upstream.
fn battery(a: &Advert) -> Option<(u8, bool)> {
    if a.single {
        return a.left.map(|p| (p, a.left_chg));
    }
    let (mut discharging, mut charging) = (None::<u8>, None::<u8>);
    for (percent, chg) in [(a.left, a.left_chg), (a.right, a.right_chg)] {
        if let Some(p) = percent {
            let slot = if chg { &mut charging } else { &mut discharging };
            *slot = Some(match *slot {
                Some(v) => v.min(p),
                None => p,
            });
        }
    }
    if let Some(p) = discharging {
        Some((p, false))
    } else if let Some(p) = charging {
        Some((p, true))
    } else {
        a.case.map(|p| (p, a.box_chg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 27-byte proximity message: header, model, flags, levels, then padding.
    fn proximity(model: u16, flags: u8, byte6: u8, byte7: u8) -> Vec<u8> {
        let mut v = vec![0x07, 0x19, 0x01, (model & 0xFF) as u8, (model >> 8) as u8, flags, byte6, byte7];
        v.resize(27, 0);
        v
    }

    #[test]
    fn parses_dual_bud_proximity_message() {
        // Model 0x2014 (AirPods Pro 2), left bud broadcasting: left 80%, right 70%, case 50%.
        // Charging bits (byte 7 high nibble): 0x02 right, 0x04 case.
        let a = Advert::parse(&proximity(0x2014, 0x20, 0x78, 0x65)).unwrap();
        assert_eq!(a.model, 0x2014);
        assert!(!a.single);
        assert_eq!((a.left, a.right, a.case), (Some(80), Some(70), Some(50)));
        assert!(!a.left_chg && a.right_chg && a.box_chg);
        assert_eq!(battery(&a), Some((80, false)));
    }

    #[test]
    fn single_battery_model_uses_low_nibble() {
        // AirPods Max (0x200A): low nibble 9 = 90%, high nibble 0, charging bit 0.
        let a = Advert::parse(&proximity(0x200A, 0, 0x09, 0x10)).unwrap();
        assert!(a.single);
        assert_eq!((a.left, a.right, a.case), (Some(90), None, None));
        assert_eq!(battery(&a), Some((90, true)));
    }

    #[test]
    fn rejects_pairing_mode_and_non_audio_models() {
        // Too short for a 25-byte proximity body.
        assert!(Advert::parse(&[0x07, 0x19, 0x01, 0x14, 0x20]).is_none());
        assert!(Advert::parse(&proximity(0x1234, 0x20, 0x78, 0x65)).is_none());
        // Pairing mode (d[2] == 0x00) is not a battery broadcast.
        let mut pairing = proximity(0x2014, 0x20, 0x78, 0x65);
        pairing[2] = 0x00;
        assert!(Advert::parse(&pairing).is_none());
    }

    #[test]
    fn walks_a_preceding_tlv() {
        let mut data = vec![0x01, 0x03, 0xAA, 0xBB, 0xCC];
        data.extend(proximity(0x2014, 0x20, 0x78, 0x65));
        assert_eq!(Advert::parse(&data).unwrap().left, Some(80));
    }

    #[test]
    fn charging_buds_report_the_lower_charging_level() {
        let a = Advert::parse(&proximity(0x2014, 0x20, 0x78, 0x65)).unwrap();
        let both_charging = Advert { left_chg: true, right_chg: true, ..a };
        assert_eq!(battery(&both_charging), Some((70, true)));
        let no_buds = Advert { left: None, right: None, left_chg: false, right_chg: false, ..a };
        assert_eq!(battery(&no_buds), Some((50, true)));
    }

    #[test]
    fn picks_the_newest_stale_advertisement() {
        let old = Advert { model: 0x2014, age_ms: 100_000, rssi: -40, left: Some(10), ..Default::default() };
        let newer = Advert { model: 0x2014, age_ms: 20_000, rssi: -60, left: Some(20), ..Default::default() };
        assert_eq!(pick_best(&[old, newer], 0x2014, true).unwrap().left, Some(20));
        // A fresh advertisement wins over a stale stronger one.
        let fresh = Advert { age_ms: 1_000, rssi: -80, right: Some(30), ..older() };
        assert_eq!(pick_best(&[old, fresh], 0x2014, true).unwrap().right, Some(30));
        // Too weak / too old / other model: nothing.
        assert!(pick_best(&[old], 0x2014, false).is_none());
        assert!(pick_best(&[Advert { age_ms: 0, rssi: -95, ..older() }], 0x2014, true).is_none());
        assert!(pick_best(&[Advert { age_ms: 0, rssi: -40, model: 0x2015, ..older() }], 0x2014, true).is_none());
    }

    fn older() -> Advert {
        Advert { model: 0x2014, left: Some(50), ..Default::default() }
    }

    #[test]
    fn fills_missing_sides_from_nearby_advertisements() {
        let best = Advert { model: 0x2014, age_ms: 1_000, rssi: -60, left: Some(80), ..Default::default() };
        let other = Advert { model: 0x2014, age_ms: 2_000, rssi: -62, right: Some(70), case: Some(40), box_chg: true, ..Default::default() };
        let a = pick_best(&[best, other], 0x2014, true).unwrap();
        assert_eq!((a.left, a.right, a.case, a.box_chg), (Some(80), Some(70), Some(40), true));
        // Too far apart in time, RSSI, or model: no fill.
        let far = Advert { age_ms: 40_000, rssi: -60, right: Some(10), ..Default::default() };
        assert_eq!(pick_best(&[best, far], 0x2014, true).unwrap().right, None);
    }

    #[test]
    fn parses_aap_battery_notification() {
        let frame = [
            0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x03, // header, count 3
            0x02, 0x01, 0x37, 0x01, 0x01, // right 55, charging
            0x04, 0x01, 0x36, 0x00, 0x01, // left 54
            0x08, 0x01, 0x64, 0x05, 0x01, // case 100, charging in case
        ];
        let b = parse_aap_battery(&frame).unwrap();
        assert_eq!((b.left, b.right, b.case), (Some(54), Some(55), Some(100)));
        assert!(!b.left_charging && b.right_charging && b.case_charging);
        assert!(!b.single_model);
    }

    #[test]
    fn aap_skips_absent_and_disconnected_components() {
        let frame = [
            0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x02, // count 2
            0x01, 0x01, 0xFF, 0x01, 0x01, // single: absent
            0x02, 0x01, 0x2A, 0x04, 0x01, // right: disconnected
        ];
        assert!(parse_aap_battery(&frame).is_none());
        // A sentinel (status 0, level 0) also counts as unknown, but the left bud still parses.
        let frame = [
            0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x02,
            0x04, 0x01, 0x00, 0x00, 0x01, // left: sentinel
            0x02, 0x01, 0x32, 0x00, 0x01, // right 50
        ];
        let b = parse_aap_battery(&frame).unwrap();
        assert_eq!((b.left, b.right), (None, Some(50)));
    }

    #[test]
    fn aap_merges_partial_frames() {
        let mut merged = AapBattery::default();
        let left = [0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x01, 0x04, 0x01, 0x36, 0x00, 0x01];
        let case = [0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x01, 0x08, 0x01, 0x64, 0x05, 0x01];
        merged.merge(&parse_aap_battery(&left).unwrap());
        merged.merge(&parse_aap_battery(&case).unwrap());
        assert_eq!((merged.left, merged.case), (Some(54), Some(100)));
        assert!(merged.case_charging);
    }

    #[test]
    fn aap_stream_keeps_the_incomplete_tail() {
        let mut buf = vec![0xDE, 0xAD, 0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x01, 0x04, 0x01, 0x36];
        let mut merged = AapBattery::default();
        assert!(!extract_aap_packets(&mut buf, &mut merged));
        // Garbage before the header is dropped, the half frame kept.
        assert_eq!(buf, [0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x01, 0x04, 0x01, 0x36]);
        // Rest of the first frame (level, status, trailer) plus a complete second one.
        buf.extend([0x00, 0x01]);
        buf.extend([0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x01, 0x02, 0x01, 0x32, 0x00, 0x01]);
        assert!(extract_aap_packets(&mut buf, &mut merged));
        assert_eq!((merged.left, merged.right), (Some(54), Some(50)));
        assert!(buf.is_empty());
    }
}
