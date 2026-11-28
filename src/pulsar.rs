//! Pulsar mice on vendor id 3710: the Sonix feature-report protocol (X2 V3 / X2 V3 eS and the
//! Feinmann FO1) and the B4 status exchange of the PRO Dongle and the wired ZYWOO mouse.
//!
//! Sonix: feature report 0, 64-byte packet `[00][category][register][sub][00 …][sum lo][sum hi]`,
//! sum = 16-bit sum of packet bytes 0..61, little endian in 62..63. The battery read is
//! `08 81 01`: SET_FEATURE, then GET_FEATURE every ~50 ms until the reply echoes the command:
//! `[status][08][81][01][..][..][percent]` (packet byte 6, 0xFF = no reading). Status 01 =
//! answered, 02 = pending, 05 = the mouse is not linked. On the cable the mouse is charging; 0 %
//! from a dongle means the mouse is off or out of range.
//!
//! The 1K dongle (5402) has its own exchange, checked on an Xlite v4: 05 is final there (asleep),
//! a request sent while another is pending on 02 is dropped, and resending can leave every read
//! stuck on 02. So nothing is sent while the dongle shows 02, a pending request is never re-sent,
//! and one 1 s deadline covers all reads. On that dongle `08 83 01` reads the charging flag and
//! `01 84 04` the paired mouse's VID/PID, used to name it.
//!
//! B4: on the 65-byte Generic Desktop collection, output report 0 with byte 1 = B4 asks for
//! status; the reply has byte 1 = percent, bytes 4..5 = `84 40` (`85 40` while charging toggles,
//! `86 40` on the wired ZYWOO) and byte 6 = USB power.
//!
//! Protocol and ids from Wireless-Device-Battery-Monitor (drivers/pulsar.cpp), which credits
//! darthsoup/PulsarBattery, bluesceada/pulsar-mouse-linux, packerlschupfer's Feinmann 8K driver
//! and the issue #911 / PR #164 captures. The output reports only request status.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::drain as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const VID: u16 = 0x3710;
const ONE_K: u16 = 0x5402;
const PRO_DONGLE: u16 = 0x5405;
const ZYWOO: u16 = 0x7401;
const ONE_K_WAIT: Duration = Duration::from_millis(1000);
const POLL: Duration = Duration::from_millis(10);
/// Polls showing an older answer before the request counts as lost.
const STALE_POLLS: u32 = 5;
/// Resends allowed per request.
const STALE_RESENDS: u32 = 2;
/// No resend this close to the deadline: it would be left pending.
const RESEND_ROOM: Duration = Duration::from_millis(350);

struct Product {
    pid: u16,
    cable: bool,
    name: Option<&'static str>,
}

const PRODUCTS: &[Product] = &[
    Product { pid: 0x3402, cable: true, name: Some("Pulsar X2 V3") },
    Product { pid: 0x3406, cable: true, name: Some("Pulsar X2 V3 eS") },
    Product { pid: ONE_K, cable: false, name: None }, // 1K dongle
    Product { pid: 0x5403, cable: false, name: None }, // 8K dongle
    Product { pid: 0x5404, cable: false, name: Some("Pulsar Feinmann FO1") }, // Feinmann 8K dongle
    Product { pid: ZYWOO, cable: true, name: None }, // ZYWOO The Chosen X over USB
    Product { pid: PRO_DONGLE, cable: false, name: None }, // PRO Dongle
];

#[derive(Default)]
pub struct Pulsar {
    seen: HashSet<String>,
}

impl Pulsar {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for prod in PRODUCTS {
            if !api.device_list().any(|d| d.vendor_id() == VID && d.product_id() == prod.pid) {
                continue;
            }
            let key = format!("pulsar:{:04x}", prod.pid);
            let found = match prod.pid {
                ONE_K => one_k(api),
                PRO_DONGLE | ZYWOO => b4(api, prod),
                _ => sonix(api, prod),
            };
            match found {
                Some((name, state)) => {
                    self.seen.insert(key.clone());
                    out.push(observation(&key, &name, state));
                }
                None => {
                    if self.seen.contains(&key) {
                        out.push(observation(&key, fallback_name(prod), State::Unreachable));
                    }
                }
            }
        }
        out
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

fn fallback_name(prod: &Product) -> &'static str {
    prod.name.unwrap_or("Pulsar wireless mouse")
}

/// Windows' report length: hidapi hides the report-id byte of an unnumbered report.
fn caps(len: Option<usize>) -> usize {
    len.map_or(0, |l| l + 1)
}

enum Reply {
    /// The 64-byte reply packet (without the report id).
    Battery(Vec<u8>),
    /// The dongle answered that the mouse is not linked.
    Offline,
    None,
}

/// The Sonix battery read: `08 81 01`.
fn sonix(api: &HidApi, prod: &Product) -> Option<(String, State)> {
    let fo1 = prod.pid == 0x5404;
    let mut infos: Vec<_> = api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == prod.pid).collect();
    // Interface 2 rejects SET_FEATURE, so higher interfaces are tried first.
    infos.sort_by_key(|d| std::cmp::Reverse(d.interface_number()));

    for info in infos {
        let Some(dev) = api.open_path(info.path()).ok() else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if caps(probe::feature_len(&desc, 0)) < 65 {
            continue;
        }
        if fo1
            && (info.interface_number() != 3
                || info.usage_page() != 0xFFFF
                || info.usage() != 1
                || caps(probe::input_len(&desc, 0)) != 65
                || caps(probe::output_len(&desc, 0)) != 65)
        {
            continue;
        }
        let Reply::Battery(packet) = query(&dev, 0x08, 0x81, 0x01) else { continue };
        let Some(percent) = packet_percent(&packet) else { continue };

        let name = if let Some(name) = prod.name {
            name.to_string()
        } else {
            let product = info.product_string().unwrap_or("").trim();
            if product.is_empty() || product.contains("Dongle") || product.contains("Receiver") {
                "Pulsar wireless mouse".into()
            } else {
                product.into()
            }
        };
        diag::event(&format!("pulsar:{:04x}", prod.pid), format!("battery via report 08 81 01 ({percent}%)"));
        let state = if percent == 0 && !prod.cable {
            // 0 from a dongle: the mouse is off or out of range.
            State::Unreachable
        } else {
            State::Battery { percent, charging: prod.cable }
        };
        return Some((name, state));
    }
    None
}

/// The level in the reply packet, or None when the dongle has no reading (0xFF).
fn packet_percent(packet: &[u8]) -> Option<u8> {
    match packet.get(6) {
        Some(&p) if p <= 100 => Some(p),
        _ => None,
    }
}

fn query(dev: &HidDevice, cat: u8, reg: u8, sub: u8) -> Reply {
    let req = packet(cat, reg, sub);
    let mut saw_offline = false;
    for _ in 0..3 {
        if dev.send_feature_report(&req).is_err() {
            return Reply::None;
        }
        for _ in 0..6 {
            std::thread::sleep(Duration::from_millis(50));
            let mut r = [0u8; 65];
            match dev.get_feature_report(&mut r) {
                Ok(n) if n >= 65 => {}
                _ => return Reply::None,
            }
            if r[2] != cat || r[3] != reg || r[4] != sub {
                continue; // not our reply (yet)
            }
            if r[1] == 0x01 {
                return Reply::Battery(r[1..].to_vec());
            }
            saw_offline = r[1] == 0x05; // 02 (or 05) = pending: keep polling
        }
    }
    if saw_offline { Reply::Offline } else { Reply::None }
}

/// The 65-byte packet: report id 0, then category/register/sub at packet bytes 1..3.
fn packet(cat: u8, reg: u8, sub: u8) -> [u8; 65] {
    let mut req = [0u8; 65];
    req[2] = cat;
    req[3] = reg;
    req[4] = sub;
    let sum: u32 = req[1..63].iter().map(|b| u32::from(*b)).sum();
    req[63] = sum as u8;
    req[64] = (sum >> 8) as u8;
    req
}

// --- 1K dongle 5402 ---

enum Link {
    Ok(Vec<u8>),
    NoLink,
    NoReply,
    IoError,
}

fn one_k(api: &HidApi) -> Option<(String, State)> {
    for info in api.device_list().filter(|d| {
        d.vendor_id() == VID
            && d.product_id() == ONE_K
            && d.interface_number() == 3
            && d.usage_page() == 0xFFFF
            && d.usage() == 1
    }) {
        let Some(dev) = api.open_path(info.path()).ok() else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if caps(probe::feature_len(&desc, 0)) != 65 {
            continue;
        }

        let mut dongle = OneK::new(&dev, Instant::now() + ONE_K_WAIT);
        let bat = dongle.read(0x08, 0x81, 0x01);
        if matches!(bat, Link::IoError) {
            continue;
        }
        let key = format!("pulsar:{:04x}", ONE_K);
        let state = match &bat {
            Link::Ok(packet) => match packet_percent(packet) {
                Some(percent) => {
                    let charging = matches!(dongle.read(0x08, 0x83, 0x01), Link::Ok(w) if w.get(6) == Some(&1));
                    State::Battery { percent, charging }
                }
                // 0 or FF: no mouse paired or in range.
                None => State::Unreachable,
            },
            // 05 (asleep) or no answer in time: keep the last value.
            Link::NoLink | Link::NoReply | Link::IoError => State::Unreachable,
        };

        let mut name = "Pulsar wireless mouse".to_string();
        // The dongle itself answers the paired-mouse read, so it names the mouse while it sleeps.
        if !matches!(bat, Link::NoReply) {
            if let Link::Ok(w) = dongle.read(0x01, 0x84, 0x04) {
                if let (Some(&vl), Some(&vh), Some(&pl), Some(&ph)) = (w.get(6), w.get(7), w.get(8), w.get(9)) {
                    let mouse = u16::from(vl) | u16::from(vh) << 8;
                    let mouse_pid = u16::from(pl) | u16::from(ph) << 8;
                    if let Some(paired) = paired_name(mouse, mouse_pid) {
                        name = paired.into();
                    }
                }
            }
        }
        diag::event(&key, format!("1K dongle: {name}"));
        return Some((name, state));
    }
    None
}

/// The dongle's reply buffer is shared, so a request from an earlier poll may still be pending.
/// This state machine drains it, never sends while it shows 02, and never re-sends a pending
/// request except when the buffer visibly went back to an older answer.
struct OneK<'a> {
    dev: &'a HidDevice,
    deadline: Instant,
    prev: [u8; 65],
    /// The last read ended on a final status, so nothing of ours is pending.
    idle: bool,
}

impl<'a> OneK<'a> {
    fn new(dev: &'a HidDevice, deadline: Instant) -> Self {
        Self { dev, deadline, prev: [0; 65], idle: false }
    }

    /// On `Link::Ok` the packet is the 64-byte reply without the report id.
    fn read(&mut self, cat: u8, reg: u8, sub: u8) -> Link {
        let req = packet(cat, reg, sub);
        let prev = self.prev;
        self.prev = req;
        let mut r = [0u8; 65];

        if !self.idle {
            loop {
                if !self.get(&mut r) {
                    return Link::IoError;
                }
                if r[1] != 0x02 {
                    break;
                }
                if Instant::now() >= self.deadline {
                    return Link::NoReply;
                }
                std::thread::sleep(POLL);
            }
        }
        self.idle = false;
        if self.dev.send_feature_report(&req).is_err() {
            return Link::IoError;
        }

        let mut sends = 1;
        let mut stale = 0;
        let mut shown = false;
        loop {
            if !self.get(&mut r) {
                return Link::IoError;
            }
            if echoes(&r, &req) {
                stale = 0;
                if r[1] == 0x01 {
                    self.idle = true;
                    return Link::Ok(r[1..].to_vec());
                }
                if r[1] == 0x05 {
                    self.idle = true;
                    return Link::NoLink;
                }
                shown = true; // pending: wait, never re-send
            } else if r[1] == 0x01 && (shown || echoes(&r, &prev)) {
                // The buffer went back to an older answer: the dongle lost the request.
                stale += 1;
                if stale >= STALE_POLLS && sends <= STALE_RESENDS && Instant::now() + RESEND_ROOM < self.deadline {
                    if self.dev.send_feature_report(&req).is_err() {
                        return Link::IoError;
                    }
                    sends += 1;
                    stale = 0;
                }
            } else {
                stale = 0;
            }
            if Instant::now() >= self.deadline {
                return Link::NoReply;
            }
            std::thread::sleep(POLL);
        }
    }

    fn get(&self, r: &mut [u8; 65]) -> bool {
        r[0] = 0;
        matches!(self.dev.get_feature_report(r), Ok(n) if n >= 65)
    }
}

fn echoes(r: &[u8; 65], req: &[u8; 65]) -> bool {
    r[2] == req[2] && r[3] == req[3] && r[4] == req[4]
}

/// Cabled product ids the 1K dongle has been seen reporting for the mouse it carries.
fn paired_name(vid: u16, pid: u16) -> Option<&'static str> {
    if vid != VID {
        return None;
    }
    match pid {
        0x3401 => Some("Pulsar Xlite v4"),
        0x3409 => Some("Pulsar X3"),
        _ => None,
    }
}

// --- PRO Dongle 5405 and wired ZYWOO 7401 ---

fn b4(api: &HidApi, prod: &Product) -> Option<(String, State)> {
    let direct = prod.pid == ZYWOO;
    for info in api.device_list().filter(|d| {
        d.vendor_id() == VID && d.product_id() == prod.pid && d.usage_page() == 0x0001 && d.usage() == 0x0000
    }) {
        let Some(dev) = api.open_path(info.path()).ok() else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if caps(probe::input_len(&desc, 0)) != 65 || caps(probe::output_len(&desc, 0)) != 65 {
            continue;
        }
        let Some((percent, charging)) = read_b4(&dev, direct) else { continue };
        let product = info.product_string().unwrap_or("");
        return Some((name_for(prod, product), State::Battery { percent, charging }));
    }
    None
}

fn name_for(prod: &Product, product: &str) -> String {
    if let Some(name) = prod.name {
        return name.into();
    }
    let product = product.trim();
    if product.is_empty() || product.contains("Dongle") || product.contains("Receiver") {
        "Pulsar wireless mouse".into()
    } else {
        product.into()
    }
}

fn read_b4(dev: &HidDevice, direct: bool) -> Option<(u8, bool)> {
    flush(dev);
    let mut req = [0u8; 65];
    req[1] = 0xB4;
    dev.write(&req).ok()?;

    let deadline = Instant::now() + Duration::from_millis(1000);
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis().min(50) as i32) {
            Ok(0) => {}
            Ok(n) => {
                if let Some(r) = b4_frame(&buf[..n], direct) {
                    return Some(r);
                }
            }
            Err(_) => return None,
        }
    }
    None
}

/// `B4 <percent> … 84 40 <charging>`; the wired mouse may use marker 86 instead of 84.
fn b4_frame(r: &[u8], direct: bool) -> Option<(u8, bool)> {
    let marker = r.len() >= 7 && (r[4] == 0x84 || r[4] == 0x85 || (direct && r[4] == 0x86));
    if !marker || r[0] != 0xB4 || r[1] > 100 || r[5] != 0x40 || r[6] > 1 {
        return None;
    }
    Some((r[1], r[6] != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_summed_little_endian() {
        let req = packet(0x08, 0x81, 0x01);
        assert_eq!(req[2], 0x08);
        assert_eq!(req[63], 0x8A); // 08 + 81 + 01
        assert_eq!(req[64], 0x00);
        let sum: u32 = req[1..63].iter().map(|b| u32::from(*b)).sum();
        assert_eq!(u32::from(req[63]) | u32::from(req[64]) << 8, sum);
    }

    #[test]
    fn battery_is_packet_byte_six() {
        let mut packet = vec![0u8; 64];
        packet[6] = 87;
        assert_eq!(packet_percent(&packet), Some(87));
        packet[6] = 0;
        assert_eq!(packet_percent(&packet), Some(0));
        packet[6] = 0xFF; // FF = no reading
        assert_eq!(packet_percent(&packet), None);
        assert_eq!(packet_percent(&packet[..6]), None);
    }

    #[test]
    fn b4_reply_needs_its_markers() {
        let mut r = [0u8; 16];
        r[0] = 0xB4;
        r[1] = 86;
        r[4] = 0x84;
        r[5] = 0x40;
        r[6] = 1;
        assert_eq!(b4_frame(&r, false), Some((86, true)));

        r[4] = 0x85; // charging toggled
        assert_eq!(b4_frame(&r, false), Some((86, true)));

        r[4] = 0x86; // only the wired mouse
        assert_eq!(b4_frame(&r, false), None);
        assert_eq!(b4_frame(&r, true), Some((86, true)));

        r[6] = 2;
        assert_eq!(b4_frame(&r, true), None);
        r[6] = 0;
        r[1] = 101;
        assert_eq!(b4_frame(&r, true), None);
        r[1] = 50;
        r[5] = 0x41;
        assert_eq!(b4_frame(&r, true), None);
    }

    #[test]
    fn only_known_paired_mice_get_a_name() {
        assert_eq!(paired_name(VID, 0x3401), Some("Pulsar Xlite v4"));
        assert_eq!(paired_name(VID, 0x3409), Some("Pulsar X3"));
        assert_eq!(paired_name(VID, 0x1234), None);
        assert_eq!(paired_name(0x1234, 0x3401), None);
    }

    #[test]
    fn one_k_echo_matches_category_register_and_sub() {
        let req = packet(0x08, 0x81, 0x01);
        let mut r = [0u8; 65];
        r[2] = 0x08;
        r[3] = 0x81;
        r[4] = 0x01;
        assert!(echoes(&r, &req));
        r[4] = 0x02;
        assert!(!echoes(&r, &req));
    }
}
