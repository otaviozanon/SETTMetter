//! Evision 2.4 GHz receivers: Redragon mice, ATK/VGN/VXE and AJAZZ keyboards, and the AULA
//! X9/V9 mouse.
//!
//! The keyboard and Redragon receivers answer output report 0x04 on the vendor collection
//! FF1C:0092: request `04 <seq lo> <seq hi> 1A <mode> 00 …` (31 payload bytes), reply
//! `04 <seq lo> <seq hi> 1A ... 00 00 ... <level> <state>` — byte 8 the level, byte 9 0 on
//! battery and 1/2 while charging. VGN HUB asks with mode 0x06, ATK HUB and the Redragon M999
//! with 0x00/0x02. The AULA receiver needs a START (01) / battery (1A) / END (02) session on
//! the same report, with the level at byte 8 and the charge state at byte 9.
//!
//! Protocol and ids from Wireless-Device-Battery-Monitor (drivers/redragon_evision.cpp,
//! atk_keyboard.cpp, ajazz_ak992.cpp, aula_mouse.cpp and aula_mouse_protocol.h). Only the
//! read-only battery queries are sent. Upstream polls the ATK receiver at most every 10 minutes
//! because asking a sleeping keyboard too often timed out its backlight; that rate limit is kept.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::drain_ok as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x320F;
const REPORT_ID: u8 = 0x04;
const COMMAND: u8 = 0x1A;
const REQ_LEN: usize = 32;

const KEY_REDRAGON: &str = "evision:320f:226f";
const KEY_AULA: &str = "evision:320f:22a0";
const KEY_KEYBOARD: &str = "evision:320f:5088";
const KEY_Z87: &str = "evision:373b:1012";
const KEY_AK992: &str = "evision:320f:50c3";
/// The keyboard receiver is asked at most this often (upstream's kReceiverIntervalMs).
const RECEIVER_INTERVAL: Duration = Duration::from_secs(10 * 60);

struct Model {
    product: &'static str,
    /// Asked the way VGN HUB asks (mode byte 0x06 instead of 0x00).
    vgn: bool,
}

const MODELS: &[Model] = &[
    Model { product: "ATK V75X 2.4G Dongle", vgn: false },
    Model { product: "ATK V75X", vgn: false },
    Model { product: "VGN S99 2.4G Dongle", vgn: true },
    Model { product: "VGN S99", vgn: true },
    Model { product: "VGN V98pro V2 2.4G Dongle", vgn: true },
    Model { product: "VGN V98pro V2", vgn: true },
    Model { product: "VGN V98 PRO 2.4G Dongle", vgn: true },
    Model { product: "VGN V98pro", vgn: true },
    Model { product: "VGN N75 PRO 2.4G Dongle", vgn: true },
    Model { product: "VGN N75 PRO", vgn: true },
    Model { product: "VGN VXE75 V2 2.4G Dongle", vgn: true },
    Model { product: "VGN VXE75 V2", vgn: true },
    Model { product: "VGN VXE75 2.4G Dongle", vgn: true },
    Model { product: "VGN VXE75", vgn: true },
    Model { product: "VXE V87 2.4G Dongle", vgn: true },
    Model { product: "VXE V87", vgn: true },
    Model { product: "VXE V87 RGB", vgn: true },
    // Generic name of the same receiver (Feker Alice80 and others).
    Model { product: "Wireless Gaming Keyboard", vgn: false },
    // Rainy 75 on the same Evision controller, on the cable id.
    Model { product: "Rainy 75", vgn: false },
];

#[derive(Default)]
pub struct Evision {
    seq: u16,
    /// Last keyboard reading, reused between the receiver's 10-minute asks.
    keyboard_last: Option<(u8, bool)>,
    keyboard_next: Option<Instant>,
    z87_last: Option<(u8, bool)>,
    z87_next: Option<Instant>,
}

impl Evision {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        self.redragon(api, &mut out);
        self.aula_mouse(api, &mut out);
        self.ak992(api, &mut out);
        self.z87(api, &mut out);
        self.keyboards(api, &mut out);
        out
    }

    fn next_seq(&mut self) -> u16 {
        self.seq = self.seq % 0xFFFF + 1;
        self.seq
    }

    fn redragon(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        let mut found = None;
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == 0x226F && d.usage_page() == 0xFF1C && d.usage() == 0x0092
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            let Some(out_len) = probe::output_len(&desc, REPORT_ID) else { continue };
            if !(REQ_LEN..=64).contains(&out_len) {
                continue;
            }
            let seq = self.next_seq();
            match evision_query(&dev, seq, 0x02, out_len, 1000) {
                Ok(reading) => {
                    found = Some(reading);
                    if reading.is_some() {
                        break;
                    }
                }
                Err(e) => diag::event(KEY_REDRAGON, format!("i/o failed: {e}")),
            }
        }
        if let Some(reading) = found {
            out.push(observation(KEY_REDRAGON, "Redragon wireless mouse", Kind::Mouse, reading));
        }
    }

    fn aula_mouse(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID
                && d.product_id() == 0x22A0
                && d.interface_number() == 1
                && d.usage_page() == 0xFF1C
                && d.usage() == 0x0092
        }) {
            if !info.product_string().unwrap_or("").trim_end().eq_ignore_ascii_case("AULA-8K Dongle") {
                continue;
            }
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::input_len(&desc, REPORT_ID) != Some(64) || probe::output_len(&desc, REPORT_ID) != Some(64) {
                continue;
            }
            match aula_query(&dev) {
                Ok(reading) => {
                    out.push(observation(KEY_AULA, "AULA wireless mouse (X9 / V9)", Kind::Mouse, reading));
                }
                Err(e) => diag::event(KEY_AULA, format!("i/o failed: {e}")),
            }
            return;
        }
    }

    /// AJAZZ AK992: the same numbered report 04 / command 1A frame, without the ATK handshake.
    fn ak992(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID
                && d.product_id() == 0x50C3
                && d.interface_number() == 1
                && d.usage_page() == 0xFF1C
                && d.usage() == 0x0092
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::input_len(&desc, REPORT_ID) != Some(64)
                || probe::output_len(&desc, REPORT_ID) != Some(64)
                || probe::feature_len(&desc, 0).is_some()
            {
                continue;
            }
            let seq = self.next_seq();
            match evision_query(&dev, seq, 0x00, 64, 600) {
                Ok(reading) => {
                    out.push(observation(KEY_AK992, "AJAZZ AK992", Kind::Keyboard, reading));
                }
                Err(e) => diag::event(KEY_AK992, format!("i/o failed: {e}")),
            }
            return;
        }
    }

    fn z87(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        let now = Instant::now();
        let mut found = false;
        for info in api.device_list().filter(|d| {
            d.vendor_id() == 0x373B && d.product_id() == 0x1012 && d.usage_page() == 0xFF1C && d.usage() == 0x0092
        }) {
            if !info.product_string().unwrap_or("").trim_end().eq_ignore_ascii_case("ATK Z87 Dongle") {
                continue;
            }
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::input_len(&desc, REPORT_ID) != Some(64) || probe::output_len(&desc, REPORT_ID) != Some(64) {
                continue;
            }
            found = true;
            if self.z87_next.is_none_or(|t| now >= t) {
                self.z87_next = Some(now + RECEIVER_INTERVAL);
                let seq = self.next_seq();
                match evision_query(&dev, seq, 0x00, 64, 1000) {
                    Ok(reading) => self.z87_last = reading,
                    Err(e) => diag::event(KEY_Z87, format!("i/o failed: {e}")),
                }
            }
            break;
        }
        if found {
            out.push(observation(KEY_Z87, "ATK Z87", Kind::Keyboard, self.z87_last));
        } else {
            self.z87_last = None;
            self.z87_next = None;
        }
    }

    fn keyboards(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        let now = Instant::now();
        let mut cable_model = false;
        let mut answer: Option<(String, Option<(u8, bool)>)> = None;

        for pid in [0x5055u16, 0x5088u16] {
            // A cable powers the keyboard and is awake; it claims the receiver.
            if pid == 0x5088 && cable_model {
                break;
            }
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0xFF1C && d.usage() == 0x0092
            }) {
                let product = info.product_string().unwrap_or("");
                let Some(model) = find_model(product) else { continue };
                if pid == 0x5055 {
                    cable_model = true;
                }
                let cable = pid == 0x5055;
                let ask = cable || self.keyboard_next.is_none_or(|t| now >= t);
                let mut reading = None;
                if ask {
                    self.keyboard_next = Some(now + RECEIVER_INTERVAL);
                    if let Ok(dev) = api.open_path(info.path()) {
                        if let Some(desc) = probe::descriptor(&dev) {
                            if let Some(out_len) = probe::output_len(&desc, REPORT_ID).filter(|n| (REQ_LEN..=64).contains(n)) {
                                let seq = self.next_seq();
                                match evision_query(&dev, seq, if model.vgn { 0x06 } else { 0x00 }, out_len, 2500) {
                                    Ok(r) => {
                                        reading = r;
                                        if r.is_some() {
                                            self.keyboard_last = r;
                                        }
                                    }
                                    Err(e) => diag::event(KEY_KEYBOARD, format!("i/o failed: {e}")),
                                }
                            }
                        }
                    }
                } else {
                    reading = self.keyboard_last;
                }
                answer = Some((keyboard_name(model.product).to_string(), reading));
                break;
            }
            if answer.is_some() {
                break;
            }
        }

        match answer {
            Some((name, reading)) => {
                out.push(observation(KEY_KEYBOARD, &name, Kind::Keyboard, reading));
            }
            None => {
                self.keyboard_last = None;
                self.keyboard_next = None;
            }
        }
    }
}

fn observation(key: &str, name: &str, kind: Kind, reading: Option<(u8, bool)>) -> Observation {
    let state = match reading {
        Some((percent, charging)) => State::Battery { percent, charging },
        None => State::Unreachable,
    };
    Observation { id: key.into(), name: Some(name.into()), kind: Some(kind), state }
}

/// `<pid>` is the receiver or cable product id, but the device is named from its product string.
fn find_model(product: &str) -> Option<&'static Model> {
    let name = product.trim_end();
    MODELS.iter().find(|m| name.eq_ignore_ascii_case(m.product))
}

/// "ATK V75X 2.4G Dongle" -> "ATK V75X".
fn keyboard_name(product: &str) -> &str {
    const SUFFIX: &str = " 2.4G Dongle";
    let name = product.trim_end();
    match name.get(name.len().saturating_sub(SUFFIX.len())..) {
        Some(tail) if name.len() > SUFFIX.len() && tail.eq_ignore_ascii_case(SUFFIX) => &name[..name.len() - SUFFIX.len()],
        _ => name,
    }
}

fn evision_query(
    dev: &HidDevice,
    seq: u16,
    mode: u8,
    out_len: usize,
    timeout_ms: u64,
) -> hidapi::HidResult<Option<(u8, bool)>> {
    let mut req = vec![0u8; out_len.max(REQ_LEN)];
    req[0] = REPORT_ID;
    req[1] = seq as u8;
    req[2] = (seq >> 8) as u8;
    req[3] = COMMAND;
    req[4] = mode;

    flush(dev)?;
    dev.write(&req)?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let ms = (left.as_millis() as i32).clamp(1, 100);
        match dev.read_timeout(&mut buf, ms) {
            Ok(0) => {}
            Ok(n) => {
                if let Some(v) = parse(&buf[..n], seq) {
                    return Ok(Some(v));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// Byte 8 is the level, byte 9 the charge state (1/2 charging). The sequence number must come
/// back, and bytes 5/6 must be zero as the vendor software checks.
fn parse(r: &[u8], seq: u16) -> Option<(u8, bool)> {
    if r.len() < 10
        || r[0] != REPORT_ID
        || r[1] != seq as u8
        || r[2] != (seq >> 8) as u8
        || r[3] != COMMAND
        || r[5] != 0
        || r[6] != 0
        || r[8] > 100
        || r[9] > 2
    {
        return None;
    }
    Some((r[8], matches!(r[9], 1 | 2)))
}

/// AULA: START (01), battery (1A), END (02), one second in all.
fn aula_query(dev: &HidDevice) -> hidapi::HidResult<Option<(u8, bool)>> {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut link_down = false;

    write_aula(dev, 0x01)?;
    let started = wait_aula(dev, 0x01, deadline.checked_sub(Duration::from_millis(550)).unwrap_or(deadline), &mut link_down)?;
    let mut reading = None;
    if started.is_some() {
        write_aula(dev, 0x1A)?;
        let until = deadline.checked_sub(Duration::from_millis(200)).unwrap_or(deadline);
        if let Some(r) = wait_aula(dev, 0x1A, until, &mut link_down)? {
            reading = aula_battery(&r);
        }
    }
    // End even after a missing acknowledgement, so no transaction stays open.
    write_aula(dev, 0x02)?;
    let _ = wait_aula(dev, 0x02, deadline, &mut link_down)?;

    if link_down {
        return Ok(None);
    }
    Ok(reading)
}

fn write_aula(dev: &HidDevice, cmd: u8) -> hidapi::HidResult<()> {
    let mut req = [0u8; 64];
    req[0] = REPORT_ID;
    req[3] = cmd;
    dev.write(&req).map(|_| ())
}

fn wait_aula(
    dev: &HidDevice,
    cmd: u8,
    until: Instant,
    link_down: &mut bool,
) -> hidapi::HidResult<Option<Vec<u8>>> {
    let mut buf = [0u8; 64];
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        let ms = (left.as_millis() as i32).clamp(1, 100);
        let n = dev.read_timeout(&mut buf, ms)?;
        if n == 0 {
            continue;
        }
        if is_heartbeat(&buf[..n]) {
            *link_down = buf[8] == 0;
            continue;
        }
        if is_aula_reply(&buf[..n], cmd) && (cmd != COMMAND || n >= 10) {
            return Ok(Some(buf[..n].to_vec()));
        }
    }
    Ok(None)
}

fn is_heartbeat(r: &[u8]) -> bool {
    r.len() >= 9 && r[0] == REPORT_ID && r[1] == 0xAA && r[2] == 0 && r[3] == 0xAA && matches!(r[8], 0 | 0xFF)
}

fn is_aula_reply(r: &[u8], cmd: u8) -> bool {
    r.len() >= 8 && r[0] == REPORT_ID && r[1..8] == [0, 0, cmd, 0, 0, 0, 0]
}

/// Zero is the receiver's uninitialised cache, not a level.
fn aula_battery(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 10 || !is_aula_reply(r, COMMAND) || r[8] == 0 || r[8] > 100 || r[9] > 2 {
        return None;
    }
    Some((r[8], r[9] != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evision_reply(seq: u16, level: u8, state: u8) -> Vec<u8> {
        let mut r = vec![0u8; REQ_LEN];
        r[0] = REPORT_ID;
        r[1] = seq as u8;
        r[2] = (seq >> 8) as u8;
        r[3] = COMMAND;
        r[4] = 0x02;
        r[8] = level;
        r[9] = state;
        r
    }

    #[test]
    fn parses_redragon_evision_reply() {
        // M999 on the 320F:226F receiver: 0x61 = 97%, on battery.
        assert_eq!(parse(&evision_reply(7, 0x61, 0), 7), Some((97, false)));
        assert_eq!(parse(&evision_reply(7, 55, 1), 7), Some((55, true)));
        assert_eq!(parse(&evision_reply(7, 55, 2), 7), Some((55, true)));
    }

    #[test]
    fn parses_ak992_frame() {
        // Same reply shape on the AJAZZ AK992's 64-byte report 04.
        let mut r = vec![0u8; 64];
        r[0] = REPORT_ID;
        r[1] = 0x34;
        r[2] = 0x12;
        r[3] = COMMAND;
        r[8] = 93;
        assert_eq!(parse(&r, 0x1234), Some((93, false)));
    }

    #[test]
    fn rejects_wrong_sequence_and_noise() {
        let r = evision_reply(7, 97, 0);
        assert_eq!(parse(&r, 8), None);
        assert_eq!(parse(&r[..9], 7), None);
        let mut wrong = r.clone();
        wrong[3] = 0xFF; // sleeping ATK keyboard
        assert_eq!(parse(&wrong, 7), None);
        let mut wrong = r.clone();
        wrong[5] = 1;
        assert_eq!(parse(&wrong, 7), None);
        assert_eq!(parse(&evision_reply(7, 101, 0), 7), None);
        assert_eq!(parse(&evision_reply(7, 50, 3), 7), None);
    }

    #[test]
    fn aula_reply_and_heartbeat() {
        // Battery reply: report 4, command 1A, level 90, charging.
        assert_eq!(aula_battery(&[0x04, 0, 0, 0x1A, 0, 0, 0, 0, 0x5A, 0x01]), Some((90, true)));
        // Level 0 is the uninitialised cache.
        assert_eq!(aula_battery(&[0x04, 0, 0, 0x1A, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(aula_battery(&[0x04, 0, 0, 0x1A, 0, 0, 0, 0, 101, 0]), None);
        assert!(is_heartbeat(&[0x04, 0xAA, 0, 0xAA, 0, 0, 0, 0, 0]));
        assert!(is_heartbeat(&[0x04, 0xAA, 0, 0xAA, 0, 0, 0, 0, 0xFF]));
        assert!(!is_heartbeat(&[0x04, 0xAA, 0, 0xAA, 0, 0, 0, 0, 1]));
    }

    #[test]
    fn names_follow_the_product_string() {
        assert_eq!(keyboard_name("ATK V75X 2.4G Dongle"), "ATK V75X");
        assert_eq!(keyboard_name("VGN S99"), "VGN S99");
        assert_eq!(keyboard_name("Wireless Gaming Keyboard"), "Wireless Gaming Keyboard");
        assert!(find_model("atk v75x 2.4g dongle  ").is_some());
        assert!(find_model("vgn s99 ").is_some_and(|m| m.vgn));
        assert!(find_model("Some Other Keyboard").is_none());
    }
}
