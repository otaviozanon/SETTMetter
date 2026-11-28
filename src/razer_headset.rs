//! Razer BlackShark and Barracuda headsets with their own vendor protocols (not the 90-byte
//! Razer feature report used for mice and keyboards).
//!
//! * BlackShark V3 / V3 Pro (0576/0577 Pro, 0579/057A V3, 0A4E/0A55 Pro for Xbox), interface 5:
//!   64-byte output report 02, `[02][00][60][00 00][00][size][00 00][dir][class][sub][argc][args]`,
//!   `[62]` = XOR of bytes 0..61. GET battery = class 21 -> `[level %][charging]`.
//!   The headset drops its 2.4 GHz link if asked every few seconds, so it is queried at most
//!   once every 5 minutes and the last reading is reused in between.
//! * BlackShark V2 Pro (2020) 0528 and Barracuda X (2021) 0536: Avnera battery query through a
//!   64-byte feature report FF on interface 3, FF00:0001.
//! * BlackShark V2 Pro (2023) 0555: 64-byte output report 02, "PA" envelope.
//! * Barracuda X (2022) 054E/0550 (YS-Tech, CRC + 'O'/'F' ack) and 0552 (Macronix, app mode):
//!   Airoha frames bridged over USB HID.
//! * BlackShark V3 X 057D (dongle) / 057C (cable): interface 0, 64-byte feature report 07,
//!   `[62]` = XOR of bytes 3..60, battery = class 07 cmd 80, charging cmd 84.
//!
//! Protocol details from OpenRazer PRs #2794, #2862 and #2899, ReedME/omarchy-blackshark-v3,
//! HaloBattery, mehdibouchami/razer-barracuda-x-battery, nightdevx/barracuda-x-battery,
//! justik13/razer-blackshark-v2-hyperspeed-webhid, tochtlani/synapseless and
//! itayavra/batterywatch. Not verified in this project.
//!
//! Only battery/charging queries are sent. The remote-mode frames select where the receiver
//! routes a command, as Synapse does; they are switched on for one query and off again and are
//! not stored on the headset.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const VID: u16 = 0x1532;
const MIN_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
struct Cached {
    at: Instant,
    observation: Observation,
}

pub struct RazerHeadset {
    /// Reading kept for the link-protecting 5-minute window (V3 and V3 X).
    cache: HashMap<u16, Cached>,
    /// Last percentage reported while not charging (the Avnera receiver can jump while charging).
    last_percent: HashMap<u16, u8>,
    hs_seq: u8,
    v3x_txid: u8,
}

impl Default for RazerHeadset {
    fn default() -> Self {
        Self {
            cache: HashMap::new(),
            last_percent: HashMap::new(),
            hs_seq: 0,
            v3x_txid: 0x40,
        }
    }
}

impl RazerHeadset {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        self.blackshark_v2_hyperspeed(api, &mut out);
        self.barracuda_x_chroma(api, &mut out);
        self.barracuda_x(api, &mut out);
        self.avnera(api, 0x0528, "Razer BlackShark V2 Pro (2020)", &mut out);
        self.avnera(api, 0x0536, "Razer Barracuda X (2021)", &mut out);
        self.blackshark_v2_pro_2023(api, &mut out);
        self.blackshark_v3x(api, &mut out);
        self.blackshark_v3(api, &mut out);
        out
    }

    // ---- BlackShark V2 HyperSpeed: MediaTek vendor HID (not the V2 Pro PA protocol) ----------

    fn blackshark_v2_hyperspeed(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        const PID: u16 = 0x0565;
        const NAME: &str = "Razer BlackShark V2 HyperSpeed";
        // Prefer the specific vendor control interface so audio/telephony endpoints are not touched.
        for page in [0xFF14, 0xFF00] {
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == PID
                    && d.usage_page() == page
                    && d.usage() == 0x01
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, 0x02).unwrap_or(0) < 64 || probe::input_len(&desc, 0x02).unwrap_or(0) < 64 {
                    continue;
                }
                let Some(level) = self.hs_query(&dev, 0x21, false) else { continue };
                if level > 100 {
                    continue;
                }
                let charging = self.hs_query(&dev, 0x2A, false).is_some_and(|v| v > 0);
                diag::event(&format!("razer:{PID:04x}"), format!("battery via GET 21: {level}%"));
                out.push(obs(PID, NAME, State::Battery { percent: level, charging }));
                return;
            }
        }
    }

    // ---- Barracuda X Chroma ------------------------------------------------------------------

    fn barracuda_x_chroma(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        const PID: u16 = 0x0574;
        const NAME: &str = "Razer Barracuda X Chroma";
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID
                && d.product_id() == PID
                && d.interface_number() == 3
                && d.usage_page() == 0xFF14
                && d.usage() == 0x0001
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, 0x02).unwrap_or(0) < 64 || probe::input_len(&desc, 0x02).unwrap_or(0) < 64 {
                continue;
            }
            let Some(level) = self.hs_query(&dev, 0x21, true) else { continue };
            if level > 100 {
                continue;
            }
            let charging = self.hs_query(&dev, 0x2A, true) == Some(1);
            diag::event(&format!("razer:{PID:04x}"), format!("battery via GET 21: {level}%"));
            out.push(obs(PID, NAME, State::Battery { percent: level, charging }));
            return;
        }
    }

    // ---- Barracuda X (2022): YS-Tech 054E/0550 and Macronix 0552 -----------------------------

    fn barracuda_x(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        const NAME: &str = "Razer Barracuda X (2022)";
        for pid in [0x054E, 0x0550, 0x0552] {
            let macronix = pid == 0x0552;
            let usage = if macronix { 0x0000 } else { 0x0001 };
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0xFF00 && d.usage() == usage
            }) {
                let Some(link) = Link::open(api, info.path(), macronix) else { continue };
                let mut link = link;
                let Some(desc) = probe::descriptor(&link.dev) else { continue };
                let report = if macronix { 0x01 } else { 0x02 };
                if probe::output_len(&desc, report).unwrap_or(0) < 64 || probe::input_len(&desc, report).unwrap_or(0) < 64 {
                    continue;
                }
                if !link.remote_mode(true) {
                    // Receiver without a connected headset (off, out of range, Bluetooth mode).
                    continue;
                }
                let level = link.get(0x21);
                let cable = if level.is_some() { link.get(0x2A) } else { None };
                link.remote_mode(false);
                let Some(level) = level else { continue };
                if level > 100 {
                    continue;
                }
                diag::event(&format!("razer:{pid:04x}"), format!("battery via Airoha GET 21: {level}%"));
                out.push(obs(pid, NAME, State::Battery { percent: level, charging: cable == Some(1) }));
                break;
            }
        }
    }

    // ---- Avnera feature-report headsets ------------------------------------------------------

    fn avnera(&mut self, api: &HidApi, pid: u16, name: &'static str, out: &mut Vec<Observation>) {
        const FRESH: [u8; 9] = [0xFF, 0x0F, 0x05, 0xFE, 0x12, 0x04, 0x1F, 0x08, 0x05];
        const STALE: [u8; 9] = [0xFF, 0x01, 0x00, 0xFE, 0x12, 0x04, 0x1F, 0x08, 0x05];
        let key = format!("razer:{pid:04x}");
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID
                && d.product_id() == pid
                && d.interface_number() == 3
                && d.usage_page() == 0xFF00
                && d.usage() == 0x0001
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::feature_len(&desc, 0xFF) != Some(64) {
                continue;
            }
            let mut request = [0u8; 64];
            request[..9].copy_from_slice(&[0xFF, 0x0A, 0x00, 0xFD, 0x04, 0x12, 0xF1, 0x02, 0x05]);
            if dev.send_feature_report(&request).is_err() {
                continue;
            }

            let mut found = None;
            for read in 0..5 {
                std::thread::sleep(Duration::from_millis(100)); // Synapse waits for the receiver to fetch status.
                let mut buf = [0u8; 64];
                buf[0] = 0xFF;
                let Ok(n) = dev.get_feature_report(&mut buf) else { continue };
                if n < 15 {
                    continue;
                }
                let fresh = buf[..9] == FRESH;
                // Give an in-flight request time to replace the old payload before reporting offline.
                let offline = read == 4 && buf[..9] == STALE;
                if fresh || offline {
                    found = Some((fresh, buf));
                    break;
                }
            }
            let Some((fresh, r)) = found else { continue };
            if !fresh {
                diag::event(&key, "receiver reports the headset offline");
                out.push(obs(pid, name, State::Off));
                continue;
            }
            let mv = u16::from(r[12]) << 8 | u16::from(r[13]);
            if mv == 0 {
                diag::event(&key, "receiver reports the headset offline");
                out.push(obs(pid, name, State::Off));
                continue;
            }
            let charging = r[11] == 0x09;
            let state = match (charging, r[14] <= 100) {
                (false, true) => {
                    self.last_percent.insert(pid, r[14]);
                    State::Battery { percent: r[14], charging: false }
                }
                // While charging the receiver can jump to 80%; keep the last real reading.
                (true, _) => match self.last_percent.get(&pid) {
                    Some(&percent) => State::Battery { percent, charging: true },
                    None if r[14] <= 100 => State::Battery { percent: r[14], charging: true },
                    None => State::Unreachable,
                },
                (false, false) => State::Unreachable,
            };
            diag::event(&key, format!("battery via feature 0xFF: {mv} mV, {state:?}"));
            out.push(obs(pid, name, state));
            return;
        }
    }

    // ---- BlackShark V2 Pro (2023): "PA" envelope ---------------------------------------------

    fn blackshark_v2_pro_2023(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        const PID: u16 = 0x0555;
        const NAME: &str = "Razer BlackShark V2 Pro (2023)";
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == 0xFF00
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, 0x02).unwrap_or(0) < 64 || probe::input_len(&desc, 0x02).unwrap_or(0) < 16 {
                continue;
            }
            let Some(level) = pa_query(&dev, 0x21) else { continue };
            if level > 100 {
                continue;
            }
            let charging = pa_query(&dev, 0x2A).is_some_and(|v| v > 0);
            diag::event(&format!("razer:{PID:04x}"), format!("battery via PA GET 21: {level}%"));
            out.push(obs(PID, NAME, State::Battery { percent: level, charging }));
            return;
        }
    }

    // ---- BlackShark V3 X ---------------------------------------------------------------------

    fn blackshark_v3x(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        const NAME: &str = "Razer BlackShark V3 X";
        for pid in [0x057D, 0x057C] {
            let cache_key = format!("razer:{pid:04x}");
            if !api.device_list().any(|d| d.vendor_id() == VID && d.product_id() == pid && d.interface_number() == 0) {
                continue;
            }
            if let Some(cached) = self.cached(pid) {
                out.push(cached);
                continue;
            }
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.interface_number() == 0
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::feature_len(&desc, 0x07).unwrap_or(0) < 64 {
                    continue;
                }
                let Some(level) = self.v3x_get(&dev, 0x80) else { continue };
                if level > 100 {
                    continue;
                }
                let charging = self.v3x_get(&dev, 0x84) == Some(1);
                let observation = obs(pid, NAME, State::Battery { percent: level, charging });
                diag::event(&cache_key, format!("battery via class 07 cmd 80: {level}%"));
                self.cache.insert(pid, Cached { at: Instant::now(), observation: observation.clone() });
                out.push(observation);
                break;
            }
        }
    }

    fn v3x_get(&mut self, dev: &HidDevice, cmd: u8) -> Option<u8> {
        for _ in 0..4 {
            self.v3x_txid = self.v3x_txid.wrapping_add(1);
            let txid = self.v3x_txid;
            let mut req = [0u8; 64];
            req[0] = 0x07;
            req[2] = txid;
            req[6] = 0x02;
            req[7] = 0x07;
            req[8] = cmd;
            req[62] = req[3..=60].iter().fold(0u8, |c, b| c ^ b);
            if dev.send_feature_report(&req).is_err() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(60));
            let mut r = [0u8; 64];
            r[0] = 0x07;
            let Ok(n) = dev.get_feature_report(&mut r) else { continue };
            if n < 63 || r[2] != txid || r[7] != 0x07 || r[8] != cmd || r[6] < 2 {
                continue;
            }
            if r[1] != 0x02 && r[1] != 0x01 {
                return None;
            }
            return Some(r[10]);
        }
        None
    }

    // ---- BlackShark V3 / V3 Pro --------------------------------------------------------------

    fn blackshark_v3(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for pid in [0x0576, 0x0577, 0x0579, 0x057A, 0x0A4E, 0x0A55] {
            if !api.device_list().any(|d| d.vendor_id() == VID && d.product_id() == pid && d.interface_number() == 5) {
                continue;
            }
            if let Some(cached) = self.cached(pid) {
                out.push(cached);
                continue;
            }
            let key = format!("razer:{pid:04x}");
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.interface_number() == 5
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, 0x02).unwrap_or(0) < 64 || probe::input_len(&desc, 0x02).unwrap_or(0) < 15 {
                    continue;
                }
                // Handshake the headset expects before reads (init, charging state on both links).
                let _ = dev.write(&v3_packet(0x00, 0x02));
                let _ = dev.write(&v3_packet(0x00, 0x2A));
                let _ = dev.write(&v3_packet(0x80, 0x2A));
                std::thread::sleep(Duration::from_millis(250));
                drain(&dev);

                let Some((level, headset_charging)) = v3_query(&dev, 0x21) else { continue };
                if level > 100 {
                    continue;
                }
                drain(&dev);
                let cable_charging = v3_query(&dev, 0x2A).is_some_and(|(v, _)| v == 1);
                let charging = headset_charging || cable_charging;
                let observation = obs(pid, v3_name(pid), State::Battery { percent: level, charging });
                diag::event(&key, format!("battery via class 21: {level}%"));
                self.cache.insert(pid, Cached { at: Instant::now(), observation: observation.clone() });
                out.push(observation);
                break;
            }
        }
    }

    // ---- helpers -----------------------------------------------------------------------------

    fn cached(&self, pid: u16) -> Option<Observation> {
        self.cache
            .get(&pid)
            .filter(|c| c.at.elapsed() < MIN_INTERVAL)
            .map(|c| c.observation.clone())
    }

    fn hs_query(&mut self, dev: &HidDevice, cmd: u8, chroma: bool) -> Option<u8> {
        let seq = if chroma {
            0x60
        } else {
            let s = 0x60 | (self.hs_seq & 0x1F);
            self.hs_seq = self.hs_seq.wrapping_add(1);
            s
        };
        drain(dev);
        if dev.write(&hs_frame(cmd, seq)).is_err() {
            return None;
        }
        let deadline = Instant::now() + Duration::from_millis(650);
        let mut buf = [0u8; 1024];
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now()).as_millis().max(1) as i32;
            let n = match dev.read_timeout(&mut buf, left.min(250)) {
                Ok(0) => continue,
                Ok(n) => n,
                Err(_) => return None,
            };
            if let Some(value) = parse_hs(&buf[..n], cmd, seq, chroma) {
                return Some(value);
            }
        }
        None
    }
}

fn obs(pid: u16, name: &'static str, state: State) -> Observation {
    Observation { id: format!("razer:{pid:04x}"), name: Some(name.into()), kind: Some(Kind::Headset), state }
}

fn v3_name(pid: u16) -> &'static str {
    if pid == 0x0A4E || pid == 0x0A55 {
        return "Razer BlackShark V3 Pro for Xbox";
    }
    if pid == 0x0576 || pid == 0x0577 {
        "Razer BlackShark V3 Pro"
    } else {
        "Razer BlackShark V3"
    }
}

// ---- BlackShark V2 HyperSpeed / Barracuda X Chroma (0x60 sequence protocol) -----------------

fn hs_frame(cmd: u8, seq: u8) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[0] = 0x02;
    b[2] = seq;
    b[6] = 0x04;
    b[9] = 0x80; // route from RF dongle to headset
    b[10] = cmd;
    b[62] = b[..62].iter().fold(0u8, |c, x| c ^ x);
    b
}

/// Reply: `[02][..][seq][..][cmd]..[12] = argc, [13] = value`, `[62]` = XOR of bytes 1..61.
fn parse_hs(r: &[u8], cmd: u8, seq: u8, chroma: bool) -> Option<u8> {
    if r.len() < 64 || r[0] != 0x02 || r[2] != seq || r[10] != cmd || r[12] < 1 {
        return None;
    }
    if r[1..62].iter().fold(0u8, |c, b| c ^ b) != r[62] {
        return None;
    }
    // Chroma uses the fixed 0x60 transaction id and rejects stale events and receiver errors.
    if chroma
        && (r[1] != 0x02
            || r[3] != 0
            || r[4] != 0
            || r[5] != 0
            || r[6] != 0x05
            || r[7] != 0
            || r[8] != 0x80
            || r[9] != 0x80
            || r[11] != 0x01
            || r[12] != 0x01)
    {
        return None;
    }
    Some(r[13])
}

// ---- BlackShark V3 / V3 Pro: XOR packet, class 21 -------------------------------------------

fn v3_packet(dir: u8, class: u8) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[0] = 0x02;
    b[2] = 0x60;
    b[6] = 0x04; // data size: dir, class, sub, argc
    b[9] = dir;
    b[10] = class;
    b[62] = b[..62].iter().fold(0u8, |c, x| c ^ x);
    b
}

/// One 600 ms read of a class reply: `[13]` value (battery or cable state), `[14]` charging.
fn v3_query(dev: &HidDevice, class: u8) -> Option<(u8, bool)> {
    if dev.write(&v3_packet(0x80, class)).is_err() {
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(600);
    let mut buf = [0u8; 1024];
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now()).as_millis().max(1) as i32;
        let n = match dev.read_timeout(&mut buf, left.min(250)) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(_) => return None,
        };
        let r = &buf[..n];
        if n >= 15 && r[0] == 0x02 && r[2] == 0x60 && r[10] == class {
            return Some((r[13], r[14] != 0));
        }
    }
    None
}

// ---- BlackShark V2 Pro (2023): PA envelope --------------------------------------------------

fn pa_frame(total: u8, inner: u8, kind: u8, cmd: u8, flag: u8) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[0] = 0x02;
    b[1] = 0x80;
    b[2] = total;
    b[5] = b'P';
    b[6] = b'A';
    b[7] = inner;
    b[9] = kind;
    b[10] = cmd;
    b[11] = flag;
    b
}

fn pa_remote(dev: &HidDevice, on: bool) -> bool {
    let ok = dev.write(&pa_frame(0x07, 0x0E, 0x02, 0xE1, u8::from(on))).is_ok();
    std::thread::sleep(Duration::from_millis(35));
    ok
}

fn pa_query(dev: &HidDevice, cmd: u8) -> Option<u8> {
    drain(dev);
    pa_remote(dev, true); // sacrificial: wakes a dozing link
    let mut value = None;
    for _ in 0..3 {
        pa_remote(dev, true);
        if dev.write(&pa_frame(0x08, 0x08, 0x03, cmd, 0)).is_err() {
            break;
        }
        let deadline = Instant::now() + Duration::from_millis(150);
        let mut buf = [0u8; 1024];
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now()).as_millis().max(1) as i32;
            let n = match dev.read_timeout(&mut buf, left.min(150)) {
                Ok(0) => continue,
                Ok(n) => n,
                Err(_) => break,
            };
            let r = &buf[..n];
            if n >= 16 && r[0] == 0x02 && r[12] == cmd && r[13] == 0x01 && r[14] >= 1 {
                value = Some(r[15]);
                break;
            }
        }
        if value.is_some() {
            break;
        }
    }
    pa_remote(dev, false);
    value
}

// ---- Barracuda X (2022): Airoha frames over the YS-Tech / Macronix bridge -------------------

struct Link {
    dev: HidDevice,
    macronix: bool,
    stream: Vec<u8>,
    seq: u8,
}

impl Link {
    /// Opens the collection; the Macronix receiver is switched to app mode first.
    fn open(api: &HidApi, path: &std::ffi::CStr, macronix: bool) -> Option<Link> {
        let dev = api.open_path(path).ok()?;
        let link = Link { dev, macronix, stream: Vec::new(), seq: 0 };
        if !macronix {
            return Some(link);
        }
        drain(&link.dev);
        if link.dev.write(&[0x01, 0x40]).is_err() {
            return None;
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut buf = [0u8; 1024];
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now()).as_millis().max(1) as i32;
            let n = match link.dev.read_timeout(&mut buf, left.min(250)) {
                Ok(0) => continue,
                Ok(n) => n,
                Err(_) => return None,
            };
            if n >= 4 && buf[0] == 0x01 && buf[1] == 0x40 {
                return (buf[2] == 0x01 && buf[3] == 0x01).then_some(link);
            }
        }
        None
    }

    /// One Airoha command `[50 41][group][seq][body..]`; the sequence number is kept in `seq`.
    fn command(&mut self, group: u8, body: &[u8]) -> bool {
        self.seq = self.seq % 127 + 1;
        let mut payload = vec![0x50, 0x41, group, self.seq];
        payload.extend_from_slice(body);
        let mut f = [0u8; 64];
        if self.macronix {
            f[0] = 0x01;
            f[1] = 0x80;
            f[2] = payload.len() as u8;
            f[3..3 + payload.len()].copy_from_slice(&payload);
            return self.dev.write(&f).is_ok();
        }
        f[0] = 0x02;
        f[1] = 0x80;
        f[2] = payload.len() as u8;
        f[5..5 + payload.len()].copy_from_slice(&payload);
        let crc = crc16(&f);
        f[3] = (crc & 0xFF) as u8;
        f[4] = (crc >> 8) as u8;
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(50));
            }
            if self.dev.write(&f).is_err() {
                return false;
            }
            let mut status = [0u8; 64];
            status[0] = 0x02;
            let Ok(n) = self.dev.get_input_report(&mut status) else { return false };
            if n < 2 {
                return false;
            }
            match status[1] {
                b'O' => return true, // taken
                b'F' => continue,    // busy
                _ => return false,   // the bridge refused it
            }
        }
        false
    }

    /// Waits for the first reply frame accepted by `matcher`; unrelated frames are skipped.
    fn await_frame<F: Fn(&[u8]) -> bool>(&mut self, matcher: F, timeout: Duration) -> Option<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 1024];
        loop {
            while let Some(frame) = next_frame(&mut self.stream) {
                if matcher(&frame) {
                    return Some(frame);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            let left = deadline.saturating_duration_since(Instant::now()).as_millis().max(1) as i32;
            let n = match self.dev.read_timeout(&mut buf, left.min(250)) {
                Ok(0) => continue,
                Ok(n) => n,
                Err(_) => return None,
            };
            let r = &buf[..n];
            if self.macronix {
                if r.len() >= 3 && r[0] == 0x01 && r[1] == 0x80 && usize::from(r[2]) + 3 <= r.len() {
                    self.stream.extend_from_slice(&r[3..3 + usize::from(r[2])]);
                }
            } else if r.len() >= 2 && r[0] == 0x02 && usize::from(r[1]) + 2 <= r.len() {
                self.stream.extend_from_slice(&r[2..2 + usize::from(r[1])]);
            }
        }
    }

    fn remote_mode(&mut self, on: bool) -> bool {
        if !self.command(0x0E, &[0x02, 0xE1, u8::from(on)]) {
            return false;
        }
        let seq = self.seq;
        self.await_frame(|r| r.len() >= 13 && r[10] == 0x0E && r[11] == (0x80 | seq), Duration::from_millis(800))
            .is_some_and(|f| f[12] == 0)
    }

    fn get(&mut self, cmd: u8) -> Option<u8> {
        if !self.command(0x08, &[0x03, cmd, 0x00, 0x00]) {
            return None;
        }
        let frame = self.await_frame(
            |r| r.len() >= 13 && r[2] == 0x08 && r[10] == cmd && r[11] == 0x01,
            Duration::from_millis(1000),
        )?;
        if frame[12] >= 1 && frame.len() >= 14 {
            Some(frame[13])
        } else {
            None
        }
    }
}

/// Next `50 49` frame in the stream: 10-byte header, payload length in bytes 8-9.
fn next_frame(stream: &mut Vec<u8>) -> Option<Vec<u8>> {
    if stream.len() < 2 {
        return None;
    }
    match stream.windows(2).position(|w| w == [0x50, 0x49]) {
        Some(at) => {
            stream.drain(..at);
        }
        None => {
            stream.clear();
            return None;
        }
    }
    if stream.len() < 10 {
        return None;
    }
    let len = 10 + (usize::from(stream[8]) | usize::from(stream[9]) << 8);
    if stream.len() < len {
        return None;
    }
    let frame = stream[..len].to_vec();
    stream.drain(..len);
    Some(frame)
}

use crate::hidutil::crc16_xmodem as crc16;

fn drain(dev: &HidDevice) {
    let mut stale = [0u8; 1024];
    while let Ok(n) = dev.read_timeout(&mut stale, 0) {
        if n == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v3_packet_xor_checksum() {
        // [02][00][60][00 00][00][04][00 00][80][21] -> XOR of bytes 0..61.
        let p = v3_packet(0x80, 0x21);
        assert_eq!(p[0], 0x02);
        assert_eq!(p[2], 0x60);
        assert_eq!(p[6], 0x04);
        assert_eq!(p[9], 0x80);
        assert_eq!(p[10], 0x21);
        assert_eq!(p[62], 0x02 ^ 0x60 ^ 0x04 ^ 0x80 ^ 0x21);
        assert_eq!(p[62], 0xC7);
    }

    #[test]
    fn hs_frame_and_reply_checksum() {
        let f = hs_frame(0x21, 0x63);
        assert_eq!(&f[..3], &[0x02, 0x00, 0x63]);
        assert_eq!(f[6], 0x04);
        assert_eq!(f[9], 0x80);
        assert_eq!(f[10], 0x21);
        assert_eq!(f[62], f[..62].iter().fold(0u8, |c, b| c ^ b));

        // Reply: report 02, transaction 02, seq, class 21, argc 1, value 55.
        let mut r = [0u8; 64];
        r[0] = 0x02;
        r[1] = 0x02;
        r[2] = 0x63;
        r[10] = 0x21;
        r[12] = 0x01;
        r[13] = 55;
        r[14] = 0x01; // charging on the same reply
        r[62] = r[1..62].iter().fold(0u8, |c, b| c ^ b);
        assert_eq!(parse_hs(&r, 0x21, 0x63, false), Some(55));
        assert_eq!(parse_hs(&r, 0x21, 0x64, false), None); // stale sequence
        r[62] ^= 0xFF;
        assert_eq!(parse_hs(&r, 0x21, 0x63, false), None); // bad checksum
    }

    #[test]
    fn hs_chroma_needs_the_full_header() {
        let mut r = [0u8; 64];
        r[0] = 0x02;
        r[1] = 0x02;
        r[2] = 0x60;
        r[6] = 0x05;
        r[8] = 0x80;
        r[9] = 0x80;
        r[10] = 0x21;
        r[11] = 0x01;
        r[12] = 0x01;
        r[13] = 77;
        r[62] = r[1..62].iter().fold(0u8, |c, b| c ^ b);
        assert_eq!(parse_hs(&r, 0x21, 0x60, true), Some(77));
        r[9] = 0x01; // not the headset route
        r[62] = r[1..62].iter().fold(0u8, |c, b| c ^ b);
        assert_eq!(parse_hs(&r, 0x21, 0x60, true), None);
    }

    #[test]
    fn pa_frame_layout() {
        // [02][80][08][00 00]['P']['A'][08][00][03][21][00]
        let f = pa_frame(0x08, 0x08, 0x03, 0x21, 0);
        assert_eq!(&f[..3], &[0x02, 0x80, 0x08]);
        assert_eq!(&f[5..7], b"PA");
        assert_eq!(f[7], 0x08);
        assert_eq!((f[9], f[10], f[11]), (0x03, 0x21, 0x00));
        let on = pa_frame(0x07, 0x0E, 0x02, 0xE1, 1);
        assert_eq!(&on[5..7], b"PA");
        assert_eq!((on[9], on[10], on[11]), (0x02, 0xE1, 1));
    }

    #[test]
    fn crc16_is_xmodem() {
        assert_eq!(crc16(b"123456789"), 0x31C3);
        // A command frame keeps its CRC in bytes 3-4, little endian.
        let mut f = [0u8; 64];
        f[0] = 0x02;
        f[1] = 0x80;
        f[2] = 8;
        f[5..9].copy_from_slice(&[0x50, 0x41, 0x08, 0x01]);
        let crc = crc16(&f);
        f[3] = (crc & 0xFF) as u8;
        f[4] = (crc >> 8) as u8;
        let mut zeroed = f;
        zeroed[3] = 0;
        zeroed[4] = 0;
        assert_eq!(crc16(&zeroed), u16::from(f[3]) | u16::from(f[4]) << 8);
    }

    #[test]
    fn airoha_stream_frames_skip_noise() {
        // Noise, then [50 49] + 8-byte header with a 2-byte payload.
        let mut stream = vec![0xFF, 0x00, 0x50, 0x49, 0, 0, 0, 0, 0, 0, 2, 0, 0xAA, 0xBB, 0xCC];
        let frame = next_frame(&mut stream).expect("frame");
        assert_eq!(frame.len(), 12);
        assert_eq!(&frame[..2], &[0x50, 0x49]);
        assert_eq!(&frame[10..], &[0xAA, 0xBB]);
        assert_eq!(stream, [0xCC]);
    }

    #[test]
    fn names_and_cable_ids() {
        assert_eq!(v3_name(0x0576), "Razer BlackShark V3 Pro");
        assert_eq!(v3_name(0x0577), "Razer BlackShark V3 Pro");
        assert_eq!(v3_name(0x0A4E), "Razer BlackShark V3 Pro for Xbox");
        assert_eq!(v3_name(0x0579), "Razer BlackShark V3");
        assert_eq!(v3_name(0x057A), "Razer BlackShark V3");
    }
}
