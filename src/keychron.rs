//! Keychron wireless mice and keyboards.
//!
//! Several generations share the vendor id 3434 and different receiver protocols:
//! - Classic / 8K mice: 64-byte unnumbered reports on a vendor page. Request `01 00 81 01 …`
//!   with checksum byte 63 = 0xA1 - sum(0..62); the reply carries the mouse's own vendor id at
//!   bytes 6-7, byte 10 charge state and byte 11 level. The 8K receiver uses `41 00 81 01`
//!   (bit 6 set) and answers byte 10 state / byte 11 level.
//! - 4K mice on the 4K Link receiver (FF0A:0001): same request, reply byte 5 state / byte 6
//!   level; `40 00 81 00` returns the paired mouse's product id.
//! - 1K receivers D030/D031 (page 0x8C, feature 0x51): command 06 answers the mouse's vid/pid,
//!   byte 11 level and byte 12 charging.
//! - Ultra-Link 8K receiver D028 (and Link-KM D029 for a mouse): FFC1:0001, B5 03 lists the
//!   slots and B3 06 answers base info; byte 19 of the payload is `battery | 0x80 charging`.
//! - Nape Pro trackball (Link-KM D026 / cable 0440): raw HID A7 31 answers level and charging.
//! - Keychron Link / Ultra keyboards: raw HID B2 lists slots; A4 answers
//!   `A4 <percent> <mV lo> <mV hi> <0 on battery / 1 charging / 2 full>`. Stock firmware never
//!   answers A4 (it echoes VIA's "unhandled" FF); only firmware with the KC_GET_BATTERY patch
//!   (ldkv/qmk_firmware#1, needs WIRLESS_RAW_ENABLE) does, so a timeout means "not supported" and
//!   nothing is shown. Ultra 8K ZMK keyboards need the equivalent A7 31 patch
//!   (docs/keychron_keyboard_firmware/q6_ultra_8k.md upstream).
//!
//! Protocol and ids from Wireless-Device-Battery-Monitor (drivers/keychron.cpp), itself from
//! launcher.keychron.com, nape_pro, KeychronNapeBatteryWidget and NapeBar. Read-only.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::checksum_a1 as checksum;
use crate::hidutil::drain_len as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x3434;
const RAW_PAGE: u16 = 0xFF60;
const RAW_USAGE: u16 = 0x61;
const TAG: &str = "keychron";

const LINK_KM: u16 = 0xD026;
const NAPE_CABLE: u16 = 0x0440;
const ULTRA_LINK: u16 = 0xD028;
const LINK_KM_MOUSE: u16 = 0xD029;
const LEGACY_A: u16 = 0xD030;
const LEGACY_C: u16 = 0xD031;
const ULTRA_KEYBOARD_CABLE: u16 = 0x1260;

#[derive(Default)]
pub struct Keychron;

impl Keychron {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        nape(api, &mut out);
        legacy(api, &mut out);
        ultra_link(api, &mut out);
        ultra_keyboard(api, &mut out);
        for pid in products(api) {
            mice(api, pid, &mut out);
            link_keyboard(api, pid, &mut out);
        }
        out
    }
}

fn products(api: &HidApi) -> Vec<u16> {
    let mut pids: Vec<u16> = api.device_list().filter(|d| d.vendor_id() == VID).map(|d| d.product_id()).collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn push(out: &mut Vec<Observation>, key: String, name: &str, kind: Kind, reading: Option<(u8, bool)>) {
    let state = match reading {
        Some((percent, charging)) => State::Battery { percent, charging },
        None => State::Unreachable,
    };
    out.push(Observation { id: key, name: Some(name.into()), kind: Some(kind), state });
}

// --- Nape Pro trackball ---------------------------------------------------------------------

fn nape(api: &HidApi, out: &mut Vec<Observation>) {
    for pid in [LINK_KM, NAPE_CABLE] {
        for info in api
            .device_list()
            .filter(|d| d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == RAW_PAGE && d.usage() == RAW_USAGE)
        {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, 0) != Some(32) {
                continue;
            }
            if pid == LINK_KM {
                // Only ask when a Nape Pro sits in a slot and is connected.
                let Some(r) = raw_query(&dev, &[0xB2], 600).ok().flatten() else { continue };
                if r.len() < 17 {
                    continue;
                }
                let mut connected = false;
                for off in [2usize, 7, 12] {
                    if ((u16::from(r[off + 2]) << 8) | u16::from(r[off + 3])) == 0x4004 {
                        connected = r[off + 4] == 1;
                    }
                }
                if !connected {
                    continue;
                }
            }
            match raw_query(&dev, &[0xA7, 0x31], 600) {
                Ok(Some(r)) if r.len() >= 4 && r[2] <= 100 => {
                    push(out, "keychron:3434:nape".into(), "Keychron Nape Pro", Kind::Mouse, Some((r[2], r[3] != 0)));
                    return;
                }
                Ok(_) => {}
                Err(e) => diag::event(TAG, format!("i/o failed: {e}")),
            }
        }
    }
}

// --- 1K receiver mice (D030 / D031) ---------------------------------------------------------

fn legacy(api: &HidApi, out: &mut Vec<Observation>) {
    for rpid in [LEGACY_A, LEGACY_C] {
        for info in api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == rpid && d.usage_page() == 0x8C) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            let Some(len) = probe::feature_len(&desc, 0x51) else { continue };
            if len < 16 {
                continue;
            }
            let mut req = vec![0u8; len];
            req[0] = 0x51;
            req[1] = 0x06;
            if let Err(e) = dev.send_feature_report(&req) {
                diag::event(TAG, format!("i/o failed: {e}"));
                continue;
            }
            let mut buf = vec![0u8; len];
            let mut got = false;
            for _ in 0..5 {
                std::thread::sleep(Duration::from_millis(40));
                buf[0] = 0x51;
                if let Ok(n) = dev.get_feature_report(&mut buf) {
                    if n >= 13 && buf[0] == 0x51 && buf[1] == 0x06 {
                        got = true;
                        break;
                    }
                }
            }
            if !got || buf[2] != 0x01 || (u16::from(buf[4]) | (u16::from(buf[5]) << 8)) != VID || buf[11] > 100 {
                continue;
            }
            let mpid = u16::from(buf[6]) | (u16::from(buf[7]) << 8);
            push(out, format!("keychron:3434:{mpid:04x}"), legacy_name(mpid), Kind::Mouse, Some((buf[11], buf[12] == 0x01)));
            break;
        }
    }
}

fn legacy_name(pid: u16) -> &'static str {
    match pid {
        0xD032 | 0xD033 => "Keychron M3",
        0xD035 => "Keychron M1",
        0xD036 | 0xD03E => "Keychron M3 mini",
        0xD03B => "Keychron M2",
        0xD03D => "Keychron M2 mini",
        0xD03F => "Keychron M6",
        0xD044 => "Keychron M7",
        _ => "Keychron mouse",
    }
}

// --- Ultra-Link 8K mice (D028, D029) --------------------------------------------------------

fn ultra_link(api: &HidApi, out: &mut Vec<Observation>) {
    for rpid in [ULTRA_LINK, LINK_KM_MOUSE] {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == rpid && d.usage_page() == 0xFFC1 && d.usage() == 0x01
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::input_len(&desc, 0xB5).unwrap_or(0) < 21 || probe::input_len(&desc, 0xB3).unwrap_or(0) < 21 {
                continue;
            }
            let len5 = report_out_len(&desc, 0xB5);
            let len3 = report_out_len(&desc, 0xB3);
            let Some(r) = ultra_query(&dev, 0xB5, 0x03, len5).ok().flatten() else { continue };

            let mut connected = false;
            let mut mouse_pid = 0u16;
            for off in [3usize, 8, 13] {
                if off + 4 >= r.len() {
                    continue;
                }
                let slot_pid = u16::from(r[off + 2]) | (u16::from(r[off + 3]) << 8);
                // A keyboard in the slot never answers B3 06; don't wait for it.
                let keyboard = ultra_keyboard_name(slot_pid).is_some();
                let here = !keyboard
                    && (u16::from(r[off]) | (u16::from(r[off + 1]) << 8)) != 0
                    && r[off + 4] != 0;
                if here && mouse_pid == 0 {
                    mouse_pid = slot_pid;
                }
                connected |= here;
            }
            if !connected {
                continue;
            }
            let Some(r) = ultra_query(&dev, 0xB3, 0x06, len3).ok().flatten() else { continue };
            if r.len() < 21 {
                continue;
            }
            let Some(reading) = power_byte(r[20]) else { continue };
            let name = if rpid == LINK_KM_MOUSE {
                if mouse_pid == 0xD064 { "Keychron M6".into() } else { "Keychron mouse".into() }
            } else {
                clean_name(info.product_string().unwrap_or(""))
            };
            push(out, format!("keychron:3434:{rpid:04x}"), &name, Kind::Mouse, Some(reading));
            break;
        }
    }
}

fn report_out_len(desc: &[u8], id: u8) -> usize {
    probe::output_len(desc, id).or_else(|| probe::input_len(desc, id)).unwrap_or(21)
}

/// Payload byte 19: bits 0-6 battery, bit 7 charging.
fn power_byte(power: u8) -> Option<(u8, bool)> {
    let percent = power & 0x7F;
    (1..=100).contains(&percent).then_some((percent, power & 0x80 != 0))
}

fn ultra_query(dev: &HidDevice, id: u8, cmd: u8, len: usize) -> hidapi::HidResult<Option<Vec<u8>>> {
    flush(dev, 64);
    let mut req = vec![0u8; len.max(2)];
    req[0] = id;
    req[1] = cmd;
    dev.write(&req)?;

    let deadline = Instant::now() + Duration::from_millis(600);
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let ms = (left.as_millis() as i32).clamp(1, 100);
        match dev.read_timeout(&mut buf, ms) {
            Ok(0) => {}
            Ok(n) if n >= 21 && buf[1] == cmd => return Ok(Some(buf[..n].to_vec())),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

// --- Ultra 8K (ZMK) keyboards ---------------------------------------------------------------

fn ultra_keyboard(api: &HidApi, out: &mut Vec<Observation>) {
    // Behind the Ultra-Link 8K receiver.
    for info in api
        .device_list()
        .filter(|d| d.vendor_id() == VID && d.product_id() == ULTRA_LINK && d.usage_page() == RAW_PAGE && d.usage() == RAW_USAGE)
    {
        let Ok(dev) = api.open_path(info.path()) else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if probe::output_len(&desc, 0) != Some(32) {
            continue;
        }
        let Some(r) = raw_query(&dev, &[0xB2], 600).ok().flatten() else { continue };
        if r.len() < 17 {
            continue;
        }
        for off in [2usize, 7, 12] {
            let kb_pid = u16::from(r[off + 2]) | (u16::from(r[off + 3]) << 8);
            let Some(name) = ultra_keyboard_name(kb_pid) else { continue };
            if (u16::from(r[off]) | (u16::from(r[off + 1]) << 8)) != VID || r[off + 4] != 1 {
                continue;
            }
            if let Some(reading) = ultra_battery(&dev) {
                push(out, format!("keychron:3434:{kb_pid:04x}"), name, Kind::Keyboard, Some(reading));
                return;
            }
            break;
        }
    }
    // On its own USB cable.
    for pid in products(api) {
        let Some(name) = ultra_keyboard_name(pid) else { continue };
        for info in api
            .device_list()
            .filter(|d| d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == RAW_PAGE && d.usage() == RAW_USAGE)
        {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, 0) != Some(32) {
                continue;
            }
            if let Some(reading) = ultra_battery(&dev) {
                push(out, format!("keychron:3434:{pid:04x}"), name, Kind::Keyboard, Some(reading));
                break;
            }
        }
    }
}

fn ultra_keyboard_name(pid: u16) -> Option<&'static str> {
    match pid {
        0x1260 => Some("Keychron Q6 Ultra 8K"),
        _ => None,
    }
}

/// Raw HID A7 31 -> `[A7][31][percent][charging]`. Stock firmware echoes zeros: rejected.
fn ultra_battery(dev: &HidDevice) -> Option<(u8, bool)> {
    let r = raw_query(dev, &[0xA7, 0x31], 600).ok().flatten()?;
    if r.len() < 4 || r[2] < 1 || r[2] > 100 || r[3] > 1 {
        return None;
    }
    Some((r[2], r[3] != 0))
}

// --- classic mice ---------------------------------------------------------------------------

fn mice(api: &HidApi, pid: u16, out: &mut Vec<Observation>) {
    for info in api.device_list().filter(|d| {
        d.vendor_id() == VID
            && d.product_id() == pid
            && d.usage_page() >= 0xFF00
    }) {
        let Ok(dev) = api.open_path(info.path()) else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if probe::output_len(&desc, 0) != Some(64) || probe::input_len(&desc, 0) != Some(64) {
            continue;
        }
        let page = info.usage_page();
        let usage = info.usage();
        let product = info.product_string().unwrap_or("");

        let r1 = keychron_query(&dev, 0x01, 0x01).ok().flatten();
        let tag = r1.as_ref().map_or(0, |r| u16::from(r[6]) | (u16::from(r[7]) << 8));

        if r1.is_some() && page == 0xFF0A && usage == 0x01 && tag != VID && tag != 0x362D {
            // 4K mouse on the 4K Link receiver.
            let Some(r) = keychron_query(&dev, 0x41, 0x01).ok().flatten() else { continue };
            let Some((percent, charging)) = mouse_4k(&r) else { continue };
            let mut name = clean_name(product);
            if let Some(st) = keychron_query(&dev, 0x40, 0x00).ok().flatten() {
                if st.len() >= 6 {
                    if let Some(found) = name_4k(u16::from(st[4]) | (u16::from(st[5]) << 8)) {
                        name = found.into();
                    }
                }
            }
            push(out, format!("keychron:3434:{pid:04x}"), &name, Kind::Mouse, Some((percent, charging)));
            return;
        }

        let mut reading = match &r1 {
            Some(r1) if tag == VID => mouse_1k(r1),
            _ => None,
        };
        if reading.is_none() {
            if let Some(r) = keychron_query(&dev, 0x41, 0x01).ok().flatten() {
                reading = mouse_8k(&r);
            }
        }
        if reading.is_none() {
            if let Some(r1) = &r1 {
                reading = mouse_8k_cable(r1);
            }
        }
        if let Some(reading) = reading {
            push(out, format!("keychron:3434:{pid:04x}"), &clean_name(product), Kind::Mouse, Some(reading));
            return;
        }
    }
}

/// Classic reply: byte 10 charging, byte 11 level, vendor id at bytes 6-7.
fn mouse_1k(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 12 || r[0] != 0x01 || r[3] != 0x01 {
        return None;
    }
    if (u16::from(r[6]) | (u16::from(r[7]) << 8)) != VID || r[11] > 100 {
        return None;
    }
    Some((r[11], r[10] != 0))
}

/// 4K reply: byte 5 charging, byte 6 level.
fn mouse_4k(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 7 || r[0] != 0x41 || r[3] != 0x01 || !(1..=100).contains(&r[6]) {
        return None;
    }
    Some((r[6], r[5] != 0))
}

/// 8K reply on the receiver: byte 10 state, byte 11 level.
fn mouse_8k(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 12 || r[0] != 0x41 || r[3] != 0x01 || r[10] > 3 || r[11] > 100 {
        return None;
    }
    Some((r[11], r[10] == 1))
}

/// 8K mouse on its cable: no bit 6, byte 5 is the highest polling rate (1-8).
fn mouse_8k_cable(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 12 || r[0] != 0x01 || r[3] != 0x01 || r[10] > 3 || !(1..=8).contains(&r[5]) {
        return None;
    }
    if !(1..=100).contains(&r[11]) {
        return None;
    }
    Some((r[11], r[10] == 1))
}

fn name_4k(pid: u16) -> Option<&'static str> {
    match pid {
        0x0620 | 0x0622 => Some("Keychron M3 mini 4K"),
        0x0621 => Some("Keychron M4 4K"),
        0x0624 => Some("Keychron M6 4K"),
        0x07A0 => Some("Keychron M3 4K"),
        _ => None,
    }
}

fn clean_name(product: &str) -> String {
    let name = product.trim();
    if name.is_empty() || name.contains("Link") || name.contains("Receiver") {
        "Keychron mouse".into()
    } else {
        name.into()
    }
}

// --- Keychron Link / Ultra keyboards --------------------------------------------------------

fn link_keyboard(api: &HidApi, pid: u16, out: &mut Vec<Observation>) {
    if matches!(pid, LINK_KM | NAPE_CABLE | ULTRA_KEYBOARD_CABLE) {
        return;
    }
    for info in api
        .device_list()
        .filter(|d| d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == RAW_PAGE && d.usage() == RAW_USAGE)
    {
        let Ok(dev) = api.open_path(info.path()) else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if probe::output_len(&desc, 0) != Some(32) {
            continue;
        }

        let mut kb_pid = 0u16;
        let reply = match link_query(&dev, 0xB2, 300) {
            Ok(Some(r)) => {
                let slots = if r[1] < 5 { r[1] as usize } else { 5 };
                for i in 0..slots {
                    let base = 2 + 5 * i;
                    if base + 4 >= r.len() {
                        break;
                    }
                    if (u16::from(r[base]) | (u16::from(r[base + 1]) << 8)) == VID && r[base + 4] != 0 {
                        kb_pid = u16::from(r[base + 2]) | (u16::from(r[base + 3]) << 8);
                        break;
                    }
                }
                if kb_pid == 0 || ultra_keyboard_name(kb_pid).is_some() {
                    // Keyboard off/asleep, on its cable, or an Ultra 8K (answers A4 with FF).
                    return;
                }
                // After an idle link the first request can be lost; a second is answered at once.
                match link_query(&dev, 0xA4, 400).ok().flatten() {
                    Some(r) => Some(r),
                    None => link_query(&dev, 0xA4, 400).ok().flatten(),
                }
            }
            // Not a receiver (a keyboard answers B2 with VIA's unhandled FF): ask it directly.
            _ => {
                kb_pid = pid;
                link_query(&dev, 0xA4, 400).ok().flatten()
            }
        };
        let Some(r) = reply else { continue };
        if r[1] > 100 {
            continue;
        }
        let name = if (0x0A50..=0x0A58).contains(&kb_pid) { "Keychron K5 Max" } else { "Keychron keyboard" };
        push(
            out,
            format!("keychron:3434:{kb_pid:04x}"),
            name,
            Kind::Keyboard,
            Some((r[1], r[4] == 1)),
        );
        return;
    }
}

// --- transports -----------------------------------------------------------------------------

/// 64-byte payload request with the 0xA1 checksum, unnumbered (leading report id 0).
fn keychron_query(dev: &HidDevice, cmd: u8, sub: u8) -> hidapi::HidResult<Option<Vec<u8>>> {
    let payload = query_frame(cmd, sub);
    let mut req = vec![0u8; 65];
    req[1..].copy_from_slice(&payload);
    flush(dev, 64);
    dev.write(&req)?;

    let deadline = Instant::now() + Duration::from_millis(600);
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let ms = (left.as_millis() as i32).clamp(1, 100);
        match dev.read_timeout(&mut buf, ms) {
            Ok(0) => {}
            Ok(n) if n >= 13 && buf[0] == cmd && buf[3] == sub => return Ok(Some(buf[..n].to_vec())),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

fn query_frame(cmd: u8, sub: u8) -> [u8; 64] {
    let mut f = [0u8; 64];
    f[0] = cmd;
    f[2] = 0x81;
    f[3] = sub;
    f[63] = checksum(&f[..63]);
    f
}



/// Raw HID channel: 32 payload bytes, no checksum. Waits for a reply starting with `cmd`.
fn raw_query(dev: &HidDevice, cmd: &[u8], timeout_ms: u64) -> hidapi::HidResult<Option<Vec<u8>>> {
    let mut req = vec![0u8; 33];
    req[1..1 + cmd.len()].copy_from_slice(cmd);
    flush(dev, 32);
    dev.write(&req)?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let ms = (left.as_millis() as i32).clamp(1, 100);
        match dev.read_timeout(&mut buf, ms) {
            Ok(0) => {}
            Ok(n) if n >= cmd.len() + 2 && &buf[..cmd.len()] == cmd => return Ok(Some(buf[..n].to_vec())),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

fn link_query(dev: &HidDevice, cmd: u8, timeout_ms: u64) -> hidapi::HidResult<Option<Vec<u8>>> {
    Ok(raw_query(dev, &[cmd], timeout_ms)?.filter(|r| r.len() >= 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_checksum_is_a1_minus_the_sum() {
        let f = query_frame(0x01, 0x01);
        assert_eq!(f[3], 0x01);
        assert_eq!(f[63], 0x1E); // 0xA1 - (0x01 + 0x81 + 0x01)
        let f = query_frame(0x40, 0x00);
        assert_eq!(f[63], 0xE0); // 0xA1 - (0x40 + 0x81)
    }

    #[test]
    fn classic_and_8k_mouse_replies() {
        let mut classic = [0u8; 64];
        classic[0] = 0x01;
        classic[3] = 0x01;
        classic[6] = 0x34;
        classic[7] = 0x34;
        classic[10] = 1;
        classic[11] = 87;
        assert_eq!(mouse_1k(&classic), Some((87, true)));

        let mut receiver = [0u8; 64];
        receiver[0] = 0x41;
        receiver[3] = 0x01;
        receiver[10] = 0;
        receiver[11] = 64;
        assert_eq!(mouse_8k(&receiver), Some((64, false)));

        let mut cable = receiver;
        cable[0] = 0x01;
        cable[5] = 8;
        cable[10] = 1;
        cable[11] = 55;
        assert_eq!(mouse_8k_cable(&cable), Some((55, true)));

        let mut four_k = [0u8; 64];
        four_k[0] = 0x41;
        four_k[3] = 0x01;
        four_k[5] = 0;
        four_k[6] = 91;
        assert_eq!(mouse_4k(&four_k), Some((91, false)));
    }

    #[test]
    fn rejects_noise_and_out_of_range() {
        let mut r = [0u8; 64];
        r[0] = 0x41;
        r[3] = 0x01;
        r[10] = 4;
        r[11] = 50;
        assert_eq!(mouse_8k(&r), None);
        r[3] = 0x00;
        assert_eq!(mouse_8k(&r), None);
    }

    #[test]
    fn ultra_power_byte() {
        assert_eq!(power_byte(95), Some((95, false)));
        assert_eq!(power_byte(0x80 | 95), Some((95, true)));
        assert_eq!(power_byte(0), None);
        assert_eq!(power_byte(0x80 | 101), None);
        assert_eq!(power_byte(0x7F), None); // 127 is not a level
    }

    #[test]
    fn names() {
        assert_eq!(legacy_name(0xD033), "Keychron M3");
        assert_eq!(legacy_name(0x1234), "Keychron mouse");
        assert_eq!(name_4k(0x0621), Some("Keychron M4 4K"));
        assert_eq!(clean_name("Keychron Link Receiver"), "Keychron mouse");
        assert_eq!(clean_name("Keychron M4 4K"), "Keychron M4 4K");
        assert_eq!(ultra_keyboard_name(0x1260), Some("Keychron Q6 Ultra 8K"));
    }
}
