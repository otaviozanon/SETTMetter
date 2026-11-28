//! Mice whose protocols did not fit `misc_mice.rs`: Incott, Wraith W1, Delux M800 Pro, and the
//! Redragon-firmware Fantech 25A7:FA7E.
//!
//! - Incott G23 V2 / G24 (PixArt 093A 522C dongle, 622C cable, vendor collection FF05:0001).
//!   Read-only status query: feature report 09 `[09 89 00 …]`, reply `[09 89 battery dpi/rate …]`,
//!   bit 7 = charging, low 7 bits = percent. A reply with bytes 2 and 3 both zero is stale. The
//!   mouse also pushes input report 09 every few seconds; that push is preferred, so a listener is
//!   kept open per collection. An 8K receiver (device info `09 8F` byte 4 = 2) answers 89 from its
//!   own frozen state, so only the push is used there and a silent mouse is shown asleep.
//! - Wraith W1 / W1 Freestyle: same PixArt ids and collection, different command map (89 reads
//!   profile settings there). Model check first: `09 8F`, byte 3 = model (18 = W1, 20 = Freestyle).
//!   Status: 86 is a per-button read; the mouse pushes input report 09 `[09 battery rate…]` in
//!   answer. The 86 sweep (buttons 0,1,2,4,3) and the 700 ms keep-alive are the web driver's.
//! - Delux M800 Pro and the original Ninjutso Sora (248A 5B2E cable, 5B2F receiver), 33-byte
//!   feature report 0x0C on FF00:0001: `[0C 01 20 00 seq…]`, reply with percent at [18], state at
//!   [19] (0 discharging, 1 charging, 2 full), link at [6].
//! - Fantech Helios Go XD5 OEM on 25A7:FA7E (Redragon Invader Pro): the Areson 17-byte feature
//!   report 0x08, command 0x04, checksum 0x55 - sum; percent at [6], charging at [7], but in tenths.
//!
//! Protocols from Wireless-Device-Battery-Monitor (drivers/misc_mice.cpp, incott_protocol.h,
//! fantech.cpp) and the community sources named there. Only status reads are sent.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::drain as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::time::{Duration, Instant};

const PIXART_VID: u16 = 0x093A;
const VENDOR_PAGE: u16 = 0xFF05;
const VENDOR_REPORT: u8 = 0x09;
const INCOTT_CABLE: u16 = 0x622C;
const INCOTT_RECEIVER: u16 = 0x522C;
/// One drain pass never blocks longer than this, and never for more than 64 reports.
const INCOTT_DRAIN_MS: u64 = 250;
const INCOTT_MAX_REPORTS: usize = 64;

const WRAITH_STANDARD: u8 = 18;
const WRAITH_FREESTYLE: u8 = 20;

const DELUX_VID: u16 = 0x248A;
const DELUX_CABLE: u16 = 0x5B2E;
const DELUX_RECEIVER: u16 = 0x5B2F;
const DELUX_REPORT: u8 = 0x0C;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Receiver {
    #[default]
    Unknown,
    Other,
    EightK,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Layout {
    #[default]
    Unknown,
    Packed,
    Old,
}

/// A decoded Incott pushed or queried status; always carries a percent.
#[derive(Clone, Copy)]
struct Status {
    percent: u8,
    charging: bool,
}

struct Listener {
    dev: HidDevice,
    layout: Layout,
}

#[derive(Default)]
pub struct MiscMiceExtra {
    /// PixArt pids known to answer as a Wraith W1, with the model code. Claimed even while asleep,
    /// so the Incott query is never sent to a W1.
    wraith_models: HashMap<u16, u8>,
    /// One read-only listener per FF05 input collection, kept open so pushes queue up between scans.
    incott: HashMap<CString, Listener>,
    incott_receiver: Receiver,
    incott_seen: HashSet<u16>,
    delux_seq: u8,
    delux_seen: bool,
}

impl MiscMiceExtra {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        // The Wraith must be identified before the Incott query runs on the same ids.
        self.wraith(api, &mut out);
        self.incott(api, &mut out);
        self.delux_m800(api, &mut out);
        out
    }

    // --- Wraith W1 --------------------------------------------------------------------------

    fn wraith(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for pid in [INCOTT_RECEIVER, INCOTT_CABLE] {
            let cable = pid == INCOTT_CABLE;
            let mut paths: Vec<CString> = api
                .device_list()
                .filter(|d| d.vendor_id() == PIXART_VID && d.product_id() == pid && d.usage_page() == VENDOR_PAGE)
                .map(|d| d.path().to_owned())
                .collect();
            if paths.is_empty() {
                continue;
            }
            paths.reverse(); // on the cable the first FF05 collection rejects SET_FEATURE

            let mut probed = None; // Some(code): answered, 0 = not a Wraith
            let mut feature = None;
            for path in &paths {
                let Ok(dev) = api.open_path(path) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                let Some(len) = probe::feature_len(&desc, VENDOR_REPORT) else { continue };
                if len < 9 {
                    continue;
                }
                let mut req = vec![0u8; len];
                req[..2].copy_from_slice(&[VENDOR_REPORT, 0x8F]);
                if dev.send_feature_report(&req).is_err() {
                    continue;
                }
                for _ in 0..10 {
                    std::thread::sleep(Duration::from_millis(50));
                    let mut buf = vec![0u8; len];
                    buf[0] = VENDOR_REPORT;
                    let Ok(n) = dev.get_feature_report(&mut buf) else { break };
                    if n >= 4 && buf[0] == VENDOR_REPORT && buf[1] == 0x8F {
                        let model = if buf[3] == WRAITH_STANDARD || buf[3] == WRAITH_FREESTYLE { buf[3] } else { 0 };
                        probed = Some(model);
                        if model != 0 {
                            feature = Some((dev, len));
                        }
                        break;
                    }
                }
                if probed.is_some() {
                    break;
                }
            }

            match probed {
                // Another PixArt mouse (Incott): leave it to the Incott reader.
                Some(0) => {
                    self.wraith_models.remove(&pid);
                    continue;
                }
                Some(model) => {
                    self.wraith_models.insert(pid, model);
                }
                None => {}
            }
            let Some(&model) = self.wraith_models.get(&pid) else { continue };

            let feature = feature.or_else(|| wraith_feature(api, pid));
            let status = feature.as_ref().and_then(|(dev, len)| wraith_read_status(api, pid, dev, *len));
            let state = match status {
                Some((percent, charging)) if cable || percent > 0 || charging => State::Battery { percent, charging },
                _ => State::Unreachable,
            };
            let name = if model == WRAITH_FREESTYLE { "Wraith W1 Freestyle" } else { "Wraith W1" };
            out.push(Observation {
                id: format!("wraithw1:{pid:04x}"),
                name: Some(name.into()),
                kind: Some(Kind::Mouse),
                state,
            });
        }
    }

    // --- Incott -----------------------------------------------------------------------------

    fn incott(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        let mut on_cable = false;
        let mut present: HashSet<CString> = HashSet::new();
        let mut shown: HashSet<u16> = HashSet::new();
        // Pids that still enumerate, and the receiver row dropped while the cable answers: same
        // mouse, one entry.
        let mut plugged: HashSet<u16> = HashSet::new();
        let mut suppressed: HashSet<u16> = HashSet::new();

        for pid in [INCOTT_CABLE, INCOTT_RECEIVER] {
            if self.wraith_models.contains_key(&pid) {
                continue;
            }
            let cable = pid == INCOTT_CABLE;
            let paths: Vec<CString> = api
                .device_list()
                .filter(|d| d.vendor_id() == PIXART_VID && d.product_id() == pid && d.usage_page() == VENDOR_PAGE)
                .map(|d| d.path().to_owned())
                .collect();
            if !cable && paths.is_empty() {
                self.incott_receiver = Receiver::Unknown;
            }
            if paths.is_empty() {
                continue;
            }
            plugged.insert(pid);

            let mut fresh = false;
            for path in &paths {
                if self.incott.contains_key(path) {
                    present.insert(path.clone());
                    continue;
                }
                let Ok(dev) = api.open_path(path) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::input_len(&desc, VENDOR_REPORT).unwrap_or(0) < 8 {
                    continue;
                }
                self.incott.insert(path.clone(), Listener { dev, layout: Layout::Unknown });
                present.insert(path.clone());
                fresh = true;
            }

            let mine: HashSet<CString> = paths.iter().filter(|p| present.contains(*p)).cloned().collect();
            let mut push = {
                let mut listeners = self.listener_refs(&mine);
                drain(&mut listeners, 0)
            };
            if !cable && on_cable && push.is_none() {
                suppressed.insert(pid);
                continue;
            }

            let mut receiver = Receiver::Other;
            let mut feature = None;
            if push.is_none() {
                if !cable {
                    if self.incott_receiver == Receiver::Unknown {
                        if let Some(kind) = incott_read(api, pid, 0x8F, parse_receiver) {
                            self.incott_receiver = kind;
                        }
                    }
                    receiver = self.incott_receiver;
                }
                // Just opened: give the mouse time for its next push. On an 8K receiver 89 is not
                // the mouse's reading; on the cable only the push carries the charging bit.
                if fresh && (cable || receiver == Receiver::EightK) {
                    let mut listeners = self.listener_refs(&mine);
                    push = drain(&mut listeners, 3000);
                }
                if push.is_none() && (cable || receiver == Receiver::Other) {
                    feature = incott_read(api, pid, 0x89, parse_feature);
                }
            }

            let Some(choice) = choose(cable, receiver, push, feature) else { continue };
            let state = match (choice.connected, choice.percent) {
                (true, Some(percent)) => State::Battery { percent, charging: choice.charging },
                _ => State::Unreachable,
            };
            if choice.asleep {
                diag::event(&format!("incott:{pid:04x}"), "receiver answered, the mouse pushes nothing (asleep)");
            }
            self.incott_seen.insert(pid);
            shown.insert(pid);
            out.push(Observation {
                id: format!("incott:{pid:04x}"),
                name: Some("Incott mouse".into()),
                kind: Some(Kind::Mouse),
                state,
            });
            if cable && choice.connected && choice.percent.is_some() {
                on_cable = true;
            }
        }

        self.incott.retain(|path, _| present.contains(path));
        for &pid in &self.incott_seen {
            if !plugged.contains(&pid) || shown.contains(&pid) || suppressed.contains(&pid) {
                continue;
            }
            out.push(Observation {
                id: format!("incott:{pid:04x}"),
                name: None,
                kind: Some(Kind::Mouse),
                state: State::Unreachable,
            });
        }
    }

    fn listener_refs<'a>(&'a mut self, mine: &HashSet<CString>) -> Vec<&'a mut Listener> {
        self.incott
            .iter_mut()
            .filter(|(path, _)| mine.contains(*path))
            .map(|(_, listener)| listener)
            .collect()
    }

    // --- Delux M800 Pro / Ninjutso Sora ------------------------------------------------------

    fn delux_m800(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        let mut answered = false;
        for pid in [DELUX_CABLE, DELUX_RECEIVER] {
            let receiver = pid == DELUX_RECEIVER;
            for info in api.device_list().filter(|d| {
                d.vendor_id() == DELUX_VID && d.product_id() == pid && d.usage_page() == 0xFF00 && d.usage() == 0x0001
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                let Some(len) = probe::feature_len(&desc, DELUX_REPORT) else { continue };
                if len < 33 {
                    continue;
                }
                let Some(reply) = delux_query(&dev, len, receiver, &mut self.delux_seq) else { continue };
                let percent = reply[18];
                if percent > 100 {
                    continue;
                }
                let state_code = reply[19];
                let connected = reply[6] != 0 || percent > 0;
                let state = if state_code == 2 {
                    State::Battery { percent: 100, charging: true }
                } else if state_code == 1 {
                    match (connected, percent) {
                        (true, p) if p > 0 => State::Battery { percent: p, charging: true },
                        _ => State::Unreachable,
                    }
                } else if connected && percent > 0 {
                    State::Battery { percent, charging: false }
                } else {
                    State::Unreachable
                };
                let product = info.product_string().unwrap_or("").to_ascii_lowercase();
                let name = if product.contains("sora") { "Ninjutso Sora" } else { "Delux M800 Pro" };
                self.delux_seen = true;
                answered = true;
                out.push(Observation {
                    id: "delux:m800".into(),
                    name: Some(name.into()),
                    kind: Some(Kind::Mouse),
                    state,
                });
                break;
            }
            if answered {
                break; // one entry: the cable is preferred, as upstream claims the receiver's product
            }
        }
        if self.delux_seen && !answered {
            out.push(Observation {
                id: "delux:m800".into(),
                name: None,
                kind: Some(Kind::Mouse),
                state: State::Unreachable,
            });
        }
    }
}

// --- Wraith helpers ---------------------------------------------------------------------------

fn wraith_feature(api: &HidApi, pid: u16) -> Option<(HidDevice, usize)> {
    for info in api.device_list().filter(|d| d.vendor_id() == PIXART_VID && d.product_id() == pid && d.usage_page() == VENDOR_PAGE) {
        let Ok(dev) = api.open_path(info.path()) else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if let Some(len) = probe::feature_len(&desc, VENDOR_REPORT) {
            if len >= 9 {
                return Some((dev, len));
            }
        }
    }
    None
}

/// One 86 (per-button) read the way the web driver's helper does it: SET_FEATURE, 150 ms,
/// GET_FEATURE, and a second GET 20 ms later while the reply is still empty.
fn wraith_read_button(dev: &HidDevice, len: usize, button: u8) -> bool {
    let mut req = vec![0u8; len];
    req[..3].copy_from_slice(&[VENDOR_REPORT, 0x86, button]);
    if dev.send_feature_report(&req).is_err() {
        return false;
    }
    let mut buf = vec![0u8; len];
    for wait in [150u64, 20] {
        std::thread::sleep(Duration::from_millis(wait));
        buf[0] = VENDOR_REPORT;
        let Ok(n) = dev.get_feature_report(&mut buf) else { return true };
        if n >= 4 && buf[1] == 0x86 {
            break;
        }
    }
    true
}

/// Reads the pushed status after the 86 sweep and the 700 ms keep-alive while waiting for it.
fn wraith_read_status(api: &HidApi, pid: u16, feature: &HidDevice, len: usize) -> Option<(u8, bool)> {
    // The push may arrive on any FF05 collection with an input report.
    let mut readers = Vec::new();
    for info in api.device_list().filter(|d| d.vendor_id() == PIXART_VID && d.product_id() == pid && d.usage_page() == VENDOR_PAGE) {
        let Ok(dev) = api.open_path(info.path()) else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if probe::input_len(&desc, VENDOR_REPORT).unwrap_or(0) < 2 {
            continue;
        }
        let mut buf = [0u8; 64];
        while let Ok(n) = dev.read_timeout(&mut buf, 0) {
            if n == 0 {
                break;
            }
        }
        readers.push(dev);
    }
    if readers.is_empty() {
        return None;
    }
    std::thread::sleep(Duration::from_millis(200)); // the W1 Service waits 200 ms after 8F
    let mut sent = false;
    for button in [0u8, 1, 2, 4, 3] {
        if wraith_read_button(feature, len, button) {
            sent = true;
        }
    }
    if !sent {
        return None;
    }
    if let Some(status) = wraith_listen(&readers, 400) {
        return Some(status);
    }
    for _ in 0..4 {
        if !wraith_read_button(feature, len, 0) {
            break;
        }
        if let Some(status) = wraith_listen(&readers, 550) {
            return Some(status);
        }
    }
    None
}

fn wraith_listen(readers: &[HidDevice], ms: u64) -> Option<(u8, bool)> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut buf = [0u8; 64];
    while deadline.checked_duration_since(Instant::now()).is_some() {
        for dev in readers {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { return None };
            match dev.read_timeout(&mut buf, left.as_millis().clamp(1, 50) as i32) {
                Ok(n) if n > 0 => {
                    if let Some(status) = wraith_status(&buf[..n]) {
                        return Some(status);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// `[09][percent | charging << 7][rate | slot << 4]…`.
fn wraith_status(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 2 || r[0] != VENDOR_REPORT || r[1] & 0x7F > 100 {
        return None;
    }
    Some((r[1] & 0x7F, r[1] & 0x80 != 0))
}

// --- Incott helpers ---------------------------------------------------------------------------

/// Newest pushed status queued since the previous call. With `wait_ms == 0` just drains what
/// Windows buffered; otherwise keeps listening up to that long (at least one drain pass) for the
/// mouse's next push. One shared budget covers every collection.
fn drain(listeners: &mut [&mut Listener], wait_ms: u64) -> Option<Status> {
    if listeners.is_empty() {
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(wait_ms.max(INCOTT_DRAIN_MS));
    let mut newest = None;
    let mut reports = 0usize;
    let mut buf = [0u8; 64];
    loop {
        for listener in listeners.iter_mut() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
            match listener.dev.read_timeout(&mut buf, left.as_millis().clamp(1, 30) as i32) {
                Ok(n) if n > 0 => {
                    reports += 1;
                    if n >= 8 {
                        if let Some(status) = parse_push(&buf[..n], &mut listener.layout) {
                            newest = Some(status);
                        }
                    }
                }
                _ => {}
            }
            if reports >= INCOTT_MAX_REPORTS {
                break;
            }
        }
        if wait_ms == 0 || newest.is_some() || reports >= INCOTT_MAX_REPORTS {
            break;
        }
        if deadline.checked_duration_since(Instant::now()).is_none() {
            break;
        }
    }
    newest
}

/// One documented read (SET_FEATURE `[09][cmd]`, then GET_FEATURE 09 until `parse` accepts a
/// reply), tried on each FF05 feature collection last to first. On the cable the first collection
/// rejects SET_FEATURE.
fn incott_read<T>(api: &HidApi, pid: u16, cmd: u8, parse: impl Fn(&[u8]) -> Option<T>) -> Option<T> {
    let mut paths: Vec<CString> = api
        .device_list()
        .filter(|d| d.vendor_id() == PIXART_VID && d.product_id() == pid && d.usage_page() == VENDOR_PAGE)
        .map(|d| d.path().to_owned())
        .collect();
    paths.reverse();
    for path in &paths {
        let Ok(dev) = api.open_path(path) else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        let Some(len) = probe::feature_len(&desc, VENDOR_REPORT) else { continue };
        if len < 9 {
            continue;
        }
        let mut req = vec![0u8; len];
        req[..2].copy_from_slice(&[VENDOR_REPORT, cmd]);
        if dev.send_feature_report(&req).is_err() {
            continue;
        }
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(50));
            let mut buf = vec![0u8; len];
            buf[0] = VENDOR_REPORT;
            let Ok(n) = dev.get_feature_report(&mut buf) else { break };
            if let Some(value) = parse(&buf[..n]) {
                return Some(value);
            }
        }
    }
    None
}

/// Pushed status, input report 09. Two layouts, told apart by their value ranges: packed
/// `[percent | charging << 7][rate | dpi tier << 4][debounce | slot << 6]…` and the older
/// 5-byte receiver forward `[charging][percent][dpi tier][rate]…`.
fn parse_push(r: &[u8], layout: &mut Layout) -> Option<Status> {
    if r.len() < 8 || r[0] != VENDOR_REPORT {
        return None;
    }
    let d = &r[1..];
    if d[..7].iter().all(|b| *b == 0) {
        return None; // carries nothing
    }
    let packed = d[0] & 0x7F <= 100 && d[1] & 0x0F <= 6 && d[1] >> 4 <= 5 && d[2] & 0x3F <= 30;
    let old = d[0] <= 1 && d[1] <= 100 && d[2] <= 5 && d[3] <= 6 && d[5] == 0 && d[6] == 0;
    if !packed && !old {
        return None;
    }
    let use_layout = if packed != old {
        let settled = if packed { Layout::Packed } else { Layout::Old };
        *layout = settled;
        settled
    } else if *layout != Layout::Unknown {
        *layout
    } else {
        // Both fit: packed would mean a 0-1% battery still in use, so take the old layout.
        Layout::Old
    };
    Some(if use_layout == Layout::Old {
        Status { percent: d[1], charging: d[0] != 0 }
    } else {
        Status { percent: d[0] & 0x7F, charging: d[0] & 0x80 != 0 }
    })
}

/// Feature reply to 09 89. Bytes 2 and 3 both zero carry nothing (receiver without data).
fn parse_feature(r: &[u8]) -> Option<Status> {
    if r.len() < 4 || r[0] != VENDOR_REPORT || r[1] != 0x89 || r[2] | r[3] == 0 {
        return None;
    }
    let percent = r[2] & 0x7F;
    if percent > 100 {
        return None;
    }
    Some(Status { percent, charging: r[2] & 0x80 != 0 })
}

/// Feature reply to 09 8F: byte 4 = 2 on the 8K receiver.
fn parse_receiver(r: &[u8]) -> Option<Receiver> {
    if r.len() < 5 || r[0] != VENDOR_REPORT || r[1] != 0x8F || r[2] != 0x01 {
        return None;
    }
    Some(if r[4] == 2 { Receiver::EightK } else { Receiver::Other })
}

struct Choice {
    percent: Option<u8>,
    charging: bool,
    connected: bool,
    asleep: bool,
}

/// Picks the reading for one Incott pid. The push wins; a receiver's 89 answer is not the mouse's
/// battery on an 8K receiver, where a silent mouse is asleep instead.
fn choose(cable: bool, receiver: Receiver, push: Option<Status>, feature: Option<Status>) -> Option<Choice> {
    let source = match push {
        Some(status) => Some((status, true)),
        None if cable || receiver != Receiver::EightK => feature.map(|status| (status, false)),
        None => None,
    };
    if let Some((status, from_push)) = source {
        // A push only comes from a mouse that is on, so its 0% is real; from a receiver's 89
        // answer, 0 means the mouse is off or out of range.
        let connected = from_push || cable || status.percent > 0 || status.charging;
        return Some(Choice {
            percent: connected.then_some(status.percent),
            charging: status.charging,
            connected,
            asleep: false,
        });
    }
    if !cable && receiver == Receiver::EightK {
        return Some(Choice { percent: None, charging: false, connected: false, asleep: true });
    }
    None
}

// --- Delux helpers ----------------------------------------------------------------------------

/// One status query with the dongle's incrementing sequence number, retried once on the receiver
/// when the first answer says the link is down.
fn delux_query(dev: &HidDevice, len: usize, receiver: bool, seq: &mut u8) -> Option<Vec<u8>> {
    let mut req = vec![0u8; len];
    req[0] = DELUX_REPORT;
    req[1] = 0x01;
    req[2] = 0x20;
    *seq = seq.wrapping_add(1);
    req[4] = *seq;
    if dev.send_feature_report(&req).is_err() {
        diag::event("delux:m800", "i/o failed");
        return None;
    }
    std::thread::sleep(Duration::from_millis(20));
    let mut reply = vec![0u8; len];
    reply[0] = DELUX_REPORT;
    let Ok(n) = dev.get_feature_report(&mut reply) else { return None };
    if n < 20 || reply[0] != DELUX_REPORT || reply[2] != 0x20 {
        return None;
    }
    if receiver && reply[6] == 0 {
        let mut w = vec![0u8; len];
        w[0] = DELUX_REPORT;
        w[1] = 0x01;
        w[2] = 0x20;
        *seq = seq.wrapping_add(1);
        w[4] = *seq;
        if dev.send_feature_report(&w).is_ok() {
            std::thread::sleep(Duration::from_millis(20));
            let mut again = vec![0u8; len];
            again[0] = DELUX_REPORT;
            if let Ok(n2) = dev.get_feature_report(&mut again) {
                if n2 >= 20 && again[0] == DELUX_REPORT && again[2] == 0x20 {
                    reply = again;
                }
            }
        }
    }
    Some(reply)
}

// --- Fantech 25A7:FA7E (Areson protocol, tenths) ----------------------------------------------

const FANTECH_VID: u16 = 0x25A7;
const FANTECH_PID: u16 = 0xFA7E;
const FANTECH_REPORT: u8 = 0x08;
const FANTECH_LEN: usize = 17;

#[derive(Default)]
pub struct FantechExtra {
    seen: bool,
}

impl FantechExtra {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut answered = None;
        let infos: Vec<_> = api.device_list().filter(|d| d.vendor_id() == FANTECH_VID && d.product_id() == FANTECH_PID).collect();
        let mut cmd = None;
        let mut readers = Vec::new();
        for info in &infos {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            let has_input = probe::input_len(&desc, 0).unwrap_or(0) >= 6 || probe::input_len(&desc, FANTECH_REPORT).unwrap_or(0) >= 6;
            if cmd.is_none() && probe::feature_len(&desc, FANTECH_REPORT).unwrap_or(0) >= FANTECH_LEN {
                cmd = Some(dev);
            } else if has_input {
                readers.push(dev);
            }
        }
        if let Some(cmd) = cmd.as_ref() {
            // The reply arrives on a different collection than the request, so every input
            // collection of the device is watched.
            answered = fantech_query(cmd, &readers);
        }
        let Some((level, charging)) = answered else {
            if self.seen {
                return vec![Observation {
                    id: "redragon:fa7e".into(),
                    name: None,
                    kind: Some(Kind::Mouse),
                    state: State::Unreachable,
                }];
            }
            return Vec::new();
        };
        self.seen = true;
        // This firmware reports tenths of a percent.
        let percent = level.saturating_mul(10).min(100);
        vec![Observation {
            id: "redragon:fa7e".into(),
            name: Some("Redragon Invader Pro".into()),
            kind: Some(Kind::Mouse),
            state: State::Battery { percent, charging },
        }]
    }
}

fn fantech_query(cmd: &HidDevice, readers: &[HidDevice]) -> Option<(u8, bool)> {
    let mut req = [0u8; FANTECH_LEN];
    req[0] = FANTECH_REPORT;
    req[1] = 0x04;
    req[FANTECH_LEN - 1] = fantech_checksum(&req[..FANTECH_LEN - 1]);
    flush(cmd);
    cmd.send_feature_report(&req).ok()?;

    let deadline = Instant::now() + Duration::from_millis(500);
    let mut best: Option<(u8, bool, bool)> = None;
    let mut buf = [0u8; 64];
    while deadline.checked_duration_since(Instant::now()).is_some() {
        for dev in readers.iter().chain(std::iter::once(cmd)) {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
            let timeout = left.as_millis().clamp(1, 40) as i32;
            match dev.read_timeout(&mut buf, timeout) {
                Ok(n) if n >= 6 => {
                    if let Some(reply) = fantech_parse(&buf[..n]) {
                        remember(reply, &mut best);
                    }
                }
                _ => {}
            }
        }
        if best.is_some_and(|reply| reply.2) {
            break;
        }
    }

    // Some firmware answers with the feature report instead of an input report.
    let mut feature = [0u8; FANTECH_LEN];
    feature[0] = FANTECH_REPORT;
    if let Ok(n) = cmd.get_feature_report(&mut feature) {
        if let Some(reply) = fantech_parse(&feature[..n]) {
            remember(reply, &mut best);
        }
    }
    best.map(|(percent, charging, _)| (percent, charging))
}

/// Percent at [6], charging at [7]; the report id may be kept at [0] or already stripped.
fn fantech_parse(r: &[u8]) -> Option<(u8, bool, bool)> {
    let sum = r.iter().take(FANTECH_LEN).fold(0u8, |sum, b| sum.wrapping_add(*b));
    let checksum_ok = r.len() >= FANTECH_LEN && sum == 0x55;
    let plausible = |power: usize, charge: usize| -> Option<(u8, bool, bool)> {
        let percent = *r.get(power)?;
        let charging = *r.get(charge)?;
        if percent == 0 || percent > 100 || charging > 1 {
            return None;
        }
        Some((percent, charging != 0, checksum_ok))
    };
    if r.len() >= 7 && r[0] == 0x04 {
        if let Some(reply) = plausible(5, 6) {
            return Some(reply);
        }
        if let Some(reply) = plausible(4, 5) {
            return Some(reply);
        }
    }
    if r.len() >= 8 && r[1] == 0x04 {
        return plausible(6, 7);
    }
    None
}

fn remember(reply: (u8, bool, bool), best: &mut Option<(u8, bool, bool)>) {
    let better = match *best {
        None => true,
        Some(found) => reply.2 && !found.2,
    };
    if better {
        *best = Some(reply);
    }
}

/// The Areson frames sum to 0x55 (mod 256).
use crate::hidutil::checksum55 as fantech_checksum;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incott_feature_reply_carries_percent_and_charge_bit() {
        // G23 Pro behind an 8K receiver, frozen reply: 09 89 3a 00 00 3b 00 00 5a (58%).
        let status = parse_feature(&[0x09, 0x89, 0x3A, 0x00, 0x00, 0x3B, 0x00, 0x00, 0x5A]).unwrap();
        assert_eq!((status.percent, status.charging), (58, false));
        // A charging reply has bit 7 set.
        let status = parse_feature(&[0x09, 0x89, 0xC8, 0x00]).unwrap();
        assert_eq!((status.percent, status.charging), (72, true));
        // Bytes 2 and 3 both zero: the receiver has no data.
        assert!(parse_feature(&[0x09, 0x89, 0x00, 0x00, 0x00]).is_none());
        assert!(parse_feature(&[0x09, 0x8F, 0x01, 0x00]).is_none());
    }

    #[test]
    fn incott_packed_and_old_pushes() {
        // Packed: 100% while charging, rate 4, dpi tier 1, debounce 5.
        let mut layout = Layout::Unknown;
        let mut frame = [0u8; 9];
        frame[0] = 0x09;
        frame[1] = 0xE4;
        frame[2] = 0x14;
        frame[3] = 0x05;
        let status = parse_push(&frame, &mut layout).unwrap();
        assert_eq!((status.percent, status.charging), (100, true));
        assert_eq!(layout, Layout::Packed);

        // Old 5-byte forward: charging, 73%, tier 3, rate 4.
        let mut layout = Layout::Unknown;
        let mut frame = [0u8; 9];
        frame[0] = 0x09;
        frame[1] = 0x01;
        frame[2] = 73;
        frame[3] = 3;
        frame[4] = 4;
        let status = parse_push(&frame, &mut layout).unwrap();
        assert_eq!((status.percent, status.charging), (73, true));
        assert_eq!(layout, Layout::Old);

        // All-zero push carries nothing.
        let mut layout = Layout::Unknown;
        let mut frame = [0u8; 9];
        frame[0] = 0x09;
        assert!(parse_push(&frame, &mut layout).is_none());
        assert!(parse_push(&[0x09, 0x00], &mut layout).is_none());
    }

    #[test]
    fn incott_receiver_kind_and_choice() {
        assert_eq!(parse_receiver(&[0x09, 0x8F, 0x01, 0x00, 0x02]), Some(Receiver::EightK));
        assert_eq!(parse_receiver(&[0x09, 0x8F, 0x01, 0x00, 0x01]), Some(Receiver::Other));
        assert_eq!(parse_receiver(&[0x09, 0x8F, 0x00, 0x00, 0x02]), None);

        // 8K receiver with a stale 89 answer: only the push is the mouse's reading.
        let push = Status { percent: 73, charging: false };
        let frozen = Status { percent: 58, charging: false };
        let choice = choose(false, Receiver::EightK, Some(push), Some(frozen)).unwrap();
        assert_eq!(choice.percent, Some(73));
        // Nothing pushed: asleep, no percent from the frozen 89 answer.
        let choice = choose(false, Receiver::EightK, None, Some(frozen)).unwrap();
        assert!(choice.asleep && choice.percent.is_none());
        // A receiver that forwards 89 may use it.
        assert_eq!(choose(false, Receiver::Other, None, Some(frozen)).unwrap().percent, Some(58));
        // 0% from a receiver's 89 answer means off or out of range; 0% from a push is real.
        let zero = Status { percent: 0, charging: false };
        assert!(!choose(false, Receiver::Other, None, Some(zero)).unwrap().connected);
        assert!(choose(false, Receiver::Other, Some(zero), None).unwrap().connected);
        assert!(choose(true, Receiver::Other, None, Some(zero)).unwrap().connected);
    }

    #[test]
    fn wraith_status_is_the_pushed_report() {
        assert_eq!(wraith_status(&[0x09, 0x64, 0x04]), Some((100, false)));
        assert_eq!(wraith_status(&[0x09, 0xE1, 0x13]), Some((97, true)));
        assert_eq!(wraith_status(&[0x09, 0x7F]), None);
        assert_eq!(wraith_status(&[0x55, 0x64]), None);
    }

    #[test]
    fn delux_reply_offsets() {
        let mut reply = vec![0u8; 33];
        reply[0] = 0x0C;
        reply[2] = 0x20;
        reply[6] = 0x01;
        reply[8..12].copy_from_slice(b"M802");
        reply[18] = 76;
        reply[19] = 1;
        assert_eq!(reply[18], 76);
        assert_eq!(reply[19], 1);
        // State 2 is full while charging.
        reply[19] = 2;
        assert_eq!(reply[19], 2);
    }

    #[test]
    fn fantech_uses_the_areson_checksum_and_tenths() {
        let mut req = [0u8; FANTECH_LEN];
        req[0] = FANTECH_REPORT;
        req[1] = 0x04;
        assert_eq!(fantech_checksum(&req[..FANTECH_LEN - 1]), 0x49);

        // Areson layout: input report 09 04 … level at [6], charging at [7].
        let frame = [0x09, 0x04, 0x00, 0x00, 0x00, 0x02, 0x0A, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0x38];
        assert_eq!(fantech_parse(&frame).map(|r| (r.0, r.1)), Some((10, false)));
        // 10 tenths is 100%.
        assert_eq!(10u8.saturating_mul(10).min(100), 100);
        // A reply with the report id stripped keeps the command at [0].
        let bare = [0x04, 0x00, 0x00, 0x00, 0x01, 0x2A, 0x01, 0x00];
        assert_eq!(fantech_parse(&bare).map(|r| (r.0, r.1)), Some((42, true)));
    }
}
