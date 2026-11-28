//! Sony INZONE headsets over USB HID: H9 II (054C:0FA8), H5 (054C:0EBF) and the INZONE Buds
//! dongle (054C:0EC2).
//!
//! All speak the Sony HCI command layer on usage page FF04, report 02 (64 bytes). Sony's packet
//! layout is kept byte for byte:
//!
//! ```text
//! command  [01][00 FC][len = 8+n][96 C3][addr][event][type][tid lo][tid hi][payload n][sum]
//!          sum = byte sum from offset 4 to the last payload byte
//! event    [04][FF][len][00][96 C3][addr][event][type][tid lo][tid hi][payload n][sum]
//!          sum = byte sum from offset 3 to the last payload byte
//! ```
//!
//! `addr`: high nibble destination, low nibble source (1 PC, 2 dongle, 4 headset); events must be
//! addressed to the PC (high nibble 1). Battery is event 04, type 01 (GET) to the headset.
//! Headsets answer `[charging][percent]` (percent FF = off); buds answer `[status][percent]`
//! for the left bud, right bud and case, and nothing when both buds sit in the case.
//!
//! Protocol from HeadsetControl (sony_inzone_protocol.hpp) and INZONE Hub 1.0.20, ported through
//! Wireless-Device-Battery-Monitor (drivers/sony_inzone.cpp). Read-only.
//!
//! Skipped: the first-generation H9 / H7 (054C:0E53) use the dongle's USB serial port instead
//! (460800 8N1, same packets); that needs COM-port enumeration and is not ported.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x054C;
const REPLY_TIMEOUT: Duration = Duration::from_millis(1500);
const REPORT: u8 = 0x02;

const BATTERY_INFO: u8 = 0x04;
const GET: u8 = 0x01;
const RET: u8 = 0x10;
const NOTIFY_ACTIVE: u8 = 0xA0;

/// pid, name, buds, minimum reply payload.
const MODELS: &[(u16, &str, bool, usize)] = &[
    (0x0FA8, "Sony INZONE H9 II", false, 2),
    (0x0EBF, "Sony INZONE H5", false, 2),
    (0x0EC2, "Sony INZONE Buds", true, 6),
];

pub struct SonyInzone {
    /// Transaction id; 1 belongs to the dongle's own notifications.
    tid: u16,
}

impl Default for SonyInzone {
    fn default() -> Self {
        Self { tid: 1 }
    }
}

impl SonyInzone {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for &(pid, name, buds, min_payload) in MODELS {
            let key = format!("inzone:{pid:04x}");
            let mut no_reply = false;
            let mut added = false;
            for info in api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0xFF04) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, REPORT).is_none_or(|n| n < 64) {
                    continue;
                }
                let tid = self.next_tid();
                match battery(&dev, tid, min_payload) {
                    Some(p) => {
                        out.push(observation(&key, name, if buds { buds_state(&p) } else { headset_state(&p) }));
                        added = true;
                        break;
                    }
                    None => {
                        no_reply = true;
                        diag::event(&key, "no battery reply");
                    }
                }
            }
            // Buds in the case take the request but nothing answers: off, not a missing dongle.
            if buds && no_reply && !added {
                out.push(observation(&key, name, State::Off));
            }
        }
        out
    }

    fn next_tid(&mut self) -> u16 {
        self.tid = self.tid.wrapping_add(1);
        if self.tid <= 1 {
            self.tid = 2;
        }
        self.tid
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Headset), state }
}

fn battery_get(tid: u16) -> [u8; 12] {
    let mut p = [0x01, 0x00, 0xFC, 8, 0x96, 0xC3, 0x41, BATTERY_INFO, GET, tid as u8, (tid >> 8) as u8, 0];
    p[11] = p[4..11].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    p
}

struct Event {
    id: u8,
    kind: u8,
    tid: u16,
    payload: Vec<u8>,
}

/// Writes the 12-byte battery GET as report 02 and waits for its reply event.
fn battery(dev: &HidDevice, tid: u16, min_payload: usize) -> Option<Vec<u8>> {
    let packet = battery_get(tid);
    let mut req = [0u8; 64];
    req[0] = REPORT;
    req[1] = packet.len() as u8;
    req[2..2 + packet.len()].copy_from_slice(&packet);

    drain(dev);
    dev.write(&req).ok()?;

    let deadline = Instant::now() + REPLY_TIMEOUT;
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let n = match dev.read_timeout(&mut buf, left.as_millis().max(1) as i32) {
            Ok(n) if n >= 15 => n,
            // A short read (including a timeout) ends the wait, as in the capture driver.
            _ => return None,
        };
        let Some(e) = parse_event(&buf[..n]) else { continue };
        if e.id == BATTERY_INFO && e.tid == tid && matches!(e.kind, RET | NOTIFY_ACTIVE) && e.payload.len() >= min_payload {
            return Some(e.payload);
        }
    }
    None
}

/// `[02][packet length][packet]`; checksum and PC address verified by `parse_event`.
fn parse_event(report: &[u8]) -> Option<Event> {
    if report.len() < 15 || report[0] != REPORT {
        return None;
    }
    let len = usize::from(report[1]);
    if !(12..=62).contains(&len) || report.len() < len + 2 {
        return None;
    }
    parse_packet(&report[2..2 + len])
}

fn parse_packet(f: &[u8]) -> Option<Event> {
    let n = f.len();
    if n < 12 || f[0] != 0x04 || f[1] != 0xFF || f[3] != 0x00 || f[4] != 0x96 || f[5] != 0xC3 {
        return None;
    }
    if f[6] >> 4 != 0x1 {
        return None; // not addressed to the PC
    }
    let sum = f[3..n - 1].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    if sum != f[n - 1] {
        return None;
    }
    Some(Event {
        id: f[7],
        kind: f[8],
        tid: u16::from(f[9]) | u16::from(f[10]) << 8,
        payload: f[11..n - 1].to_vec(),
    })
}

fn drain(dev: &HidDevice) {
    let mut buf = [0u8; 64];
    for _ in 0..64 {
        if !dev.read_timeout(&mut buf, 0).is_ok_and(|n| n > 0) {
            break;
        }
    }
}

/// `[charging][percent]`; percent FF = headset off.
fn headset_state(p: &[u8]) -> State {
    match p.get(1) {
        Some(&0xFF) => State::Off,
        Some(&percent) if percent <= 100 => State::Battery { percent, charging: p[0] != 0 },
        _ => State::Unreachable,
    }
}

/// Three `[status][percent]` pairs: left, right, case. The lower bud is shown; FF = in the case.
fn buds_state(p: &[u8]) -> State {
    if p.len() < 6 {
        return State::Off;
    }
    let level = |b: u8| (b <= 100).then_some(b);
    let percent = match (level(p[1]), level(p[3])) {
        (Some(l), Some(r)) => Some(l.min(r)),
        (a, b) => a.max(b),
    };
    match percent {
        Some(percent) => State::Battery { percent, charging: false },
        None => State::Off,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One event packet addressed to the PC (0x14), charging 1, 90%.
    fn reply(tid: u16, payload: &[u8]) -> Vec<u8> {
        let len = 11 + payload.len();
        let mut f = vec![0x04, 0xFF, (len - 3) as u8, 0x00, 0x96, 0xC3, 0x14, BATTERY_INFO, RET, tid as u8, (tid >> 8) as u8];
        f.extend_from_slice(payload);
        let sum = f[3..].iter().fold(0u8, |a, b| a.wrapping_add(*b));
        f.push(sum);
        f
    }

    #[test]
    fn battery_get_checksum_and_layout() {
        let p = battery_get(0x0002);
        assert_eq!(&p[..9], &[0x01, 0x00, 0xFC, 8, 0x96, 0xC3, 0x41, 0x04, 0x01]);
        assert_eq!(&p[9..11], &[0x02, 0x00]);
        let sum = p[4..11].iter().fold(0u8, |a, b| a.wrapping_add(*b));
        assert_eq!(p[11], sum);
    }

    #[test]
    fn parses_a_captured_reply() {
        let packet = reply(2, &[0x01, 0x5A]);
        let mut report = vec![0u8; 64];
        report[0] = REPORT;
        report[1] = packet.len() as u8;
        report[2..2 + packet.len()].copy_from_slice(&packet);
        let e = parse_event(&report).expect("valid event");
        assert_eq!((e.id, e.kind, e.tid), (BATTERY_INFO, RET, 2));
        assert_eq!(e.payload, [0x01, 0x5A]);
        assert_eq!(headset_state(&e.payload), State::Battery { percent: 90, charging: true });
    }

    #[test]
    fn rejects_wrong_checksum_address_and_short_packets() {
        let mut packet = reply(2, &[0x00, 0x50]);
        *packet.last_mut().unwrap() ^= 0x01;
        assert!(parse_packet(&packet).is_none());

        let mut packet = reply(2, &[0x00, 0x50]);
        packet[6] = 0x41; // addressed to the headset, not the PC
        assert!(parse_packet(&packet).is_none());

        assert!(parse_packet(&reply(2, &[0x00])[..11]).is_none());
        assert!(parse_event(&[REPORT, 11, 0x04, 0xFF]).is_none());
    }

    #[test]
    fn headset_percent_ff_means_off() {
        assert_eq!(headset_state(&[0x01, 0xFF]), State::Off);
        assert_eq!(headset_state(&[0x00, 0x65]), State::Unreachable);
        assert_eq!(headset_state(&[0x00, 0x00]), State::Battery { percent: 0, charging: false });
    }

    #[test]
    fn buds_show_the_lower_of_the_two() {
        // Left 93, right 67, case 100 (capture from the protocol notes).
        assert_eq!(buds_state(&[0x00, 93, 0x00, 67, 0x00, 100]), State::Battery { percent: 67, charging: false });
        // One bud in the case (FF): the other is shown.
        assert_eq!(buds_state(&[0x00, 0xFF, 0x00, 80, 0x00, 100]), State::Battery { percent: 80, charging: false });
        // Both in the case: nothing to show.
        assert_eq!(buds_state(&[0x00, 0xFF, 0x00, 0xFF, 0x00, 100]), State::Off);
        assert_eq!(buds_state(&[0x00, 0xFF]), State::Off);
    }

    #[test]
    fn tid_never_reuses_one() {
        let mut s = SonyInzone::default();
        assert_eq!(s.next_tid(), 2);
        assert_eq!(s.next_tid(), 3);
        s.tid = 0xFFFF;
        assert_eq!(s.next_tid(), 2);
    }
}
