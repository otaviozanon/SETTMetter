//! Keyboards on the shared "Wireless 2.4G Dongle" 36B0:3002, and PMO Hub keyboards on their
//! USB cable.
//!
//! The dongle passes the keyboard's raw HID channel through on interface 1 (FF60:0061, 32-byte
//! reports, report id 0). Two firmware families answer there:
//! - PMO Hub (WAVE 75, Aurora 80, PMO P75/80): `AA 10 00 00 <len>` opens a session, `AA 30 00
//!   00 18` asks the battery (reply byte 8, after the 0x55 success byte at 7), `AA 11 …` closes
//!   it. The START reply carries the paired keyboard's pid (36B0:3009 for the WAVE 75).
//! - VIA (URX Core75, EVO80): read-only custom value 7 on channel 0, request `08 00 07`,
//!   reply echoes the header with the level in byte 3. A firmware without it answers FF.
//!
//! The protocol behind the dongle is invisible in the descriptors, so the likely one is tried
//! first (VIA when the product names a URX board, PMO otherwise), then the other; once one
//! answers only it is asked. A dongle that answers neither backs off instead of being asked on
//! every scan. Both protocols are read-only. From Wireless-Device-Battery-Monitor
//! (drivers/via_dongle.cpp), via pmohub.cn's web driver and HaloBattery's URX notes.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::drain_len as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const VID: u16 = 0x36B0;
const DONGLE: u16 = 0x3002;
const PAGE: u16 = 0xFF60;
const USAGE: u16 = 0x0061;
const KEY: &str = "via";

struct Board {
    pid: u16,
    name: &'static str,
}

/// pmohub.cn's device list: the keyboard's own cable pid.
const BOARDS: &[Board] = &[
    Board { pid: 0x3009, name: "WAVE 75" },
    Board { pid: 0x3004, name: "Aurora 80" },
    Board { pid: 0x302B, name: "PMO P75" },
    Board { pid: 0x3012, name: "PMO 80" },
];

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Proto {
    #[default]
    Unknown,
    Pmo,
    Via,
}

struct Tracked {
    proto: Proto,
    misses: u8,
    next_try: Option<Instant>,
    /// Keyboard pid seen behind the PMO dongle.
    paired: u16,
}

impl Default for Tracked {
    fn default() -> Self {
        Tracked { proto: Proto::Unknown, misses: 0, next_try: None, paired: 0 }
    }
}

#[derive(Default)]
pub struct ViaDongle {
    state: HashMap<u16, Tracked>,
}

impl ViaDongle {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        let mut cable = false;
        for board in BOARDS {
            if let Some(o) = self.poll_cable(api, board.pid) {
                cable = true;
                out.push(o);
            }
        }
        if let Some(o) = self.poll_dongle(api, cable) {
            out.push(o);
        }
        let present: HashSet<u16> = api.device_list().filter(|d| d.vendor_id() == VID).map(|d| d.product_id()).collect();
        self.state.retain(|pid, _| present.contains(pid));
        out
    }

    /// PMO Hub keyboard plugged in by cable: the dongle times out while the cable is used.
    fn poll_cable(&mut self, api: &HidApi, pid: u16) -> Option<Observation> {
        let infos: Vec<_> = api
            .device_list()
            .filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == pid
                    && d.interface_number() == 1
                    && d.usage_page() == PAGE
                    && d.usage() == USAGE
            })
            .collect();
        if infos.is_empty() {
            self.state.remove(&pid);
            return None;
        }
        let st = self.state.entry(pid).or_default();
        if st.next_try.is_some_and(|t| Instant::now() < t) {
            return None;
        }
        for info in infos {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            let payload = probe::output_len(&desc, 0).unwrap_or(0);
            if payload != 32 && payload != 64 {
                continue;
            }
            match pmo_query(&dev, payload) {
                Ok((Some(level), _)) => {
                    answered(st, Proto::Pmo);
                    let name = BOARDS.iter().find(|b| b.pid == pid).map_or("Wireless keyboard", |b| b.name);
                    return Some(observation(pid, name, level));
                }
                Ok((None, _)) => {}
                Err(e) => diag::event(KEY, format!("cable {pid:04x} i/o failed: {e}")),
            }
        }
        missed(st);
        None
    }

    fn poll_dongle(&mut self, api: &HidApi, cable_answered: bool) -> Option<Observation> {
        let infos: Vec<_> = api
            .device_list()
            .filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == DONGLE
                    && d.interface_number() == 1
                    && d.usage_page() == PAGE
                    && d.usage() == USAGE
            })
            .collect();
        if infos.is_empty() {
            self.state.remove(&DONGLE);
            return None;
        }
        let mut found = None;
        for info in infos {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, 0) == Some(32) {
                found = Some((dev, info.product_string().unwrap_or("").to_string()));
                break;
            }
        }
        let Some((dev, product)) = found else { return None };

        let st = self.state.entry(DONGLE).or_default();
        // A PMO keyboard on its cable leaves the dongle unanswered.
        if cable_answered && st.proto != Proto::Via {
            return None;
        }
        if st.next_try.is_some_and(|t| Instant::now() < t) {
            return None;
        }

        let mut order = [Proto::Pmo, Proto::Via];
        if st.proto == Proto::Via || (st.proto == Proto::Unknown && product.contains("URX")) {
            order.swap(0, 1);
        }
        let tries = if st.proto == Proto::Unknown { 2 } else { 1 };
        let mut result = None;
        for proto in order.iter().take(tries) {
            match proto {
                Proto::Pmo => match pmo_query(&dev, 32) {
                    Ok((Some(level), paired)) => {
                        if paired != 0 {
                            st.paired = paired;
                        }
                        answered(st, Proto::Pmo);
                        let key_pid = if st.paired != 0 { st.paired } else { DONGLE };
                        let name = BOARDS.iter().find(|b| b.pid == key_pid).map_or("Wireless keyboard", |b| b.name);
                        result = Some(observation(key_pid, name, level));
                        break;
                    }
                    Ok((None, _)) => {}
                    Err(e) => diag::event(KEY, format!("PMO query failed: {e}")),
                },
                Proto::Via => match via_query(&dev, 32) {
                    Ok(Some(level)) => {
                        answered(st, Proto::Via);
                        result = Some(observation(DONGLE, &via_name(&product), level));
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => diag::event(KEY, format!("VIA query failed: {e}")),
                },
                Proto::Unknown => {}
            }
        }
        if result.is_none() {
            missed(st);
        }
        result
    }
}

fn observation(pid: u16, name: &str, level: u8) -> Observation {
    Observation {
        id: format!("via:{:04x}:{pid:04x}", VID),
        name: Some(name.into()),
        kind: Some(Kind::Keyboard),
        state: State::Battery { percent: level, charging: false },
    }
}

// --- PMO Hub --------------------------------------------------------------------------------

fn pmo_frame(payload: usize, cmd: u8, len: u8) -> Vec<u8> {
    let mut f = vec![0u8; payload + 1];
    f[1] = 0xAA;
    f[2] = cmd;
    f[5] = len;
    f
}

fn pmo_ok(r: &[u8]) -> bool {
    r.len() > 8 && r[7] == 0x55
}

fn paired_pid(r: &[u8]) -> u16 {
    if r.len() > 12 && (u16::from(r[9]) | (u16::from(r[10]) << 8)) == VID {
        u16::from(r[11]) | (u16::from(r[12]) << 8)
    } else {
        0
    }
}

/// Returns the level (if any) and the paired keyboard pid from the START reply.
fn pmo_query(dev: &HidDevice, payload: usize) -> hidapi::HidResult<(Option<u8>, u16)> {
    let len = (payload - 8) as u8; // pmohub.cn's payload - 8
    let mut start = ask(dev, &pmo_frame(payload, 0x10, len), &[0xAA, 0x10, 0x00, 0x00], 600)?;
    if start.is_none() {
        // The link may still be waking up.
        start = ask(dev, &pmo_frame(payload, 0x10, len), &[0xAA, 0x10, 0x00, 0x00], 600)?;
    }
    let Some(start) = start else {
        // Close the session anyway in case START was taken; don't wait when nothing answers.
        let _ = dev.write(&pmo_frame(payload, 0x11, len));
        return Ok((None, 0));
    };
    if !pmo_ok(&start) {
        let _ = ask(dev, &pmo_frame(payload, 0x11, len), &[0xAA, 0x11, 0x00, 0x00], 300)?;
        return Ok((None, 0));
    }
    let paired = paired_pid(&start);

    // GET_BATTERY always asks with length 0x18, even on the 64-byte cable collection.
    let bat = ask(dev, &pmo_frame(payload, 0x30, 0x18), &[0xAA, 0x30, 0x00, 0x00], 600)?;
    let level = bat.filter(|r| pmo_ok(r) && r[8] <= 100).map(|r| r[8]);

    let _ = ask(dev, &pmo_frame(payload, 0x11, len), &[0xAA, 0x11, 0x00, 0x00], 600)?;
    Ok((level, paired))
}

// --- VIA ------------------------------------------------------------------------------------

/// Returns the level (1..100), or None when the firmware has no such value.
fn via_query(dev: &HidDevice, payload: usize) -> hidapi::HidResult<Option<u8>> {
    let mut req = vec![0u8; payload + 1];
    req[1] = 0x08; // VIA id_custom_get_value
    req[2] = 0x00; // channel 0
    req[3] = 0x07; // value 7: battery level
    for _ in 0..2 {
        // The first request can be missed while the link wakes up.
        if let Some(r) = ask(dev, &req, &[0x08, 0x00, 0x07], 600)? {
            return Ok((1..=100).contains(&r[3]).then_some(r[3]));
        }
    }
    Ok(None)
}

/// "EVO80 2.4G" -> "EVO80", "URX Core75 Wireless" -> "URX Core75"; the generic dongle name
/// becomes "Wireless keyboard".
fn via_name(product: &str) -> String {
    let name = product.trim_end();
    if name.contains("Dongle") {
        return "Wireless keyboard".into();
    }
    let mut name = name;
    for suffix in [" 2.4G", " Wireless"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped;
            break;
        }
    }
    if name.is_empty() { "Wireless keyboard".into() } else { name.into() }
}

// --- shared ---------------------------------------------------------------------------------

fn ask(dev: &HidDevice, req: &[u8], echo: &[u8], timeout_ms: u64) -> hidapi::HidResult<Option<Vec<u8>>> {
    flush(dev, req.len());
    dev.write(req)?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut buf = [0u8; 128];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let ms = (left.as_millis() as i32).clamp(1, 100);
        match dev.read_timeout(&mut buf, ms) {
            Ok(0) => {}
            Ok(n) if n >= echo.len() + 6 && &buf[..echo.len()] == echo => return Ok(Some(buf[..n].to_vec())),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

fn answered(st: &mut Tracked, proto: Proto) {
    st.proto = proto;
    st.misses = 0;
    st.next_try = None;
}

fn missed(st: &mut Tracked) {
    st.misses = st.misses.saturating_add(1);
    // A known protocol that stops answering most likely means a sleeping keyboard: retry soon.
    // After several misses forget it, in case another keyboard was paired.
    if st.misses >= 6 {
        st.proto = Proto::Unknown;
    }
    let secs = if st.proto != Proto::Unknown {
        20
    } else {
        (30u64 << st.misses.saturating_sub(1).min(4)).min(300)
    };
    st.next_try = Some(Instant::now() + Duration::from_secs(secs));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pmo_frames() {
        assert_eq!(&pmo_frame(32, 0x10, 0x18)[..6], &[0x00, 0xAA, 0x10, 0x00, 0x00, 0x18]);
        assert_eq!(&pmo_frame(32, 0x30, 0x18)[1..3], &[0xAA, 0x30]);
    }

    #[test]
    fn pmo_success_and_paired_pid() {
        let mut start = vec![0u8; 33];
        start[7] = 0x55;
        start[9] = 0xB0;
        start[10] = 0x36;
        start[11] = 0x09;
        start[12] = 0x30;
        assert!(pmo_ok(&start));
        assert_eq!(paired_pid(&start), 0x3009);
        start[10] = 0x35; // not this vendor
        assert_eq!(paired_pid(&start), 0);
        start[10] = 0x36;
        start[7] = 0x0F;
        assert!(!pmo_ok(&start));
    }

    #[test]
    fn via_names() {
        assert_eq!(via_name("EVO80 2.4G"), "EVO80");
        assert_eq!(via_name("URX Core75 Wireless"), "URX Core75");
        assert_eq!(via_name("Wireless 2.4G Dongle"), "Wireless keyboard");
        assert_eq!(via_name(""), "Wireless keyboard");
    }

    #[test]
    fn backoff_grows_and_forgets_after_six_misses() {
        let mut st = Tracked::default();
        missed(&mut st);
        assert_eq!(st.proto, Proto::Unknown);
        assert_eq!(st.misses, 1);
        for _ in 0..5 {
            missed(&mut st);
        }
        assert_eq!(st.misses, 6);
        assert_eq!(st.proto, Proto::Unknown);
    }
}
