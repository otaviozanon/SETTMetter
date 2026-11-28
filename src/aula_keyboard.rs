//! AULA / MCHOSE / Redragon wireless keyboards on their "BY" 2.4 GHz receivers, and the AULA
//! HERO 68 Air 8K receiver.
//!
//! BY receivers (3554:FA09, 3554:FB40, 41E4:2001, and 05AC:024F when it has the MCHOSE layout):
//! 20-byte report 0x13 on the FF02/FF04 usage 0002 collection. Request `13 4A <packets>
//! ... <sum>`; the reply starts with 0x13, echoes command 0x4A with its top bit masked, and
//! carries the level at byte 5 and the state at byte 6 (high nibble 1 = charging). AULA frames
//! end with a sum checksum over bytes 0..18; MCHOSE firmware leaves the trailer 0, accepted only
//! as a single-packet frame (byte 2 = 1, byte 3 = 0). Receivers with a generic product string are
//! asked for their six-byte model code (command 0x05) the way M HUB does, which names MCHOSE and
//! Redragon models and tells an MCHOSE keyboard from an AULA one.
//!
//! HERO 68 Air (372E:106C): acknowledged 20-byte frames on FF60:0061 with numbered report 9:
//! `09 66`, three control/counter bytes, 14 payload bytes and a checksum. A message is four data
//! frames plus a host trailer; every frame is acknowledged. Battery request `09 87 00 00 01 00`;
//! reply `87 00 00 01 00 02 <level> <flags>` (flags bit 0x10 = charging), or a pushed
//! `FE 05 <level> <flags>`.
//!
//! Protocol and ids from Wireless-Device-Battery-Monitor (drivers/aula.cpp), itself from
//! PyFlat/Device-Battery-Info, womier-l65-linux, sanshoku, M HUB, and a USB capture of AULA Hub
//! for the HERO. Read-only; the only writes are the queries and protocol acknowledgements.
//!
//! Upstream backs off receivers that never answer (the Redragon K673 shares 3554:FA09 and is
//! woken by every request): after three misses over five minutes, retries slow to 5, 10, 20, 40
//! and 60 minutes, reset by unplugging. That is kept. The "seen before" shortcut upstream uses to
//! spare known keyboards is not available here, so a keyboard asleep at startup can take up to a
//! few minutes to be listed again.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::drain as flush;
use crate::hidutil::sum_range;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const BY_REPORT: u8 = 0x13;
const BY_BATTERY: u8 = 0x4A;
const BY_MODEL: u8 = 0x05;
const BY_LEN: usize = 20;
const BY_RESEND: Duration = Duration::from_millis(1300);
const BY_TIMEOUT: Duration = Duration::from_secs(4);
const BACKOFF_AFTER: Duration = Duration::from_secs(5 * 60);
const BACKOFF_FIRST: u64 = 5 * 60;
const BACKOFF_MAX: u64 = 60 * 60;

/// BY receivers. 3554:FB40 and the Apple id are in upstream's table too; 05AC:024F is only used
/// with the MCHOSE layout (SONiX dongles share the id and are read by sonix.rs).
const RECEIVERS: &[(u16, u16)] = &[(0x3554, 0xFA09), (0x3554, 0xFB40), (0x41E4, 0x2001), (0x05AC, 0x024F)];

/// M HUB's keyboardPwds table: six-byte model code returned by command 0x05.
const MODELS: &[([u8; 6], &str)] = &[
    ([0x03, 0x00, 0x00, 0x00, 0x02, 0x1C], "MCHOSE G87"),
    ([0x03, 0x00, 0x00, 0x00, 0x00, 0x77], "MCHOSE K99"),
    ([0x03, 0x00, 0x00, 0x00, 0x01, 0xDF], "MCHOSE K99 V2"),
    ([0x03, 0x00, 0x00, 0x00, 0x01, 0x32], "MCHOSE G98"),
    ([0x03, 0x00, 0x00, 0x00, 0x00, 0xD6], "MCHOSE X75"),
    ([0x0A, 0x00, 0x00, 0x00, 0x00, 0x03], "MCHOSE G98 Pro"),
    ([0x03, 0x00, 0x00, 0x00, 0x01, 0xC6], "MCHOSE X75 V2"),
    ([0x03, 0x00, 0x00, 0x00, 0x01, 0x15], "MCHOSE G75 Pro"),
    ([0x0A, 0x00, 0x00, 0x00, 0x00, 0x18], "MCHOSE G98 V2"),
    ([0x0A, 0x00, 0x00, 0x00, 0x00, 0x2B], "MCHOSE UT98"),
    ([0x0A, 0x00, 0x00, 0x00, 0x00, 0x01], "MCHOSE Z75"),
    ([0x0A, 0x00, 0x00, 0x00, 0x00, 0x09], "MCHOSE Z75S"),
    ([0x03, 0x00, 0x00, 0x00, 0x01, 0xC7], "MCHOSE KX75"),
    ([0x03, 0x00, 0x00, 0x00, 0x01, 0x74], "MCHOSE GX87"),
    ([0x03, 0x00, 0x00, 0x00, 0x01, 0xED], "MCHOSE GX87 V2"),
    ([0x03, 0x00, 0x00, 0x00, 0x01, 0x35], "MCHOSE K87"),
    ([0x03, 0x00, 0x00, 0x00, 0x03, 0x35], "MCHOSE G75 V2"),
    ([0x0A, 0x00, 0x00, 0x00, 0x00, 0x34], "MCHOSE G87 V2"),
    ([0x03, 0x00, 0x00, 0x00, 0x00, 0x6A], "Redragon Horus Pro"),
    ([0x03, 0x00, 0x00, 0x00, 0x03, 0xDA], "Redragon Horus Pro V2"),
];

const HERO_VID: u16 = 0x372E;
const HERO_PID: u16 = 0x106C;
const HERO_ID: u8 = 0x09;
const HERO_MAGIC: u8 = 0x66;
const HERO_DATA: u8 = 0x0E;
const HERO_ACK: u8 = 0x00;
const HERO_TRAILER_HOST: u8 = 0x08;
const HERO_TRAILER_DEV: u8 = 0x07;
const HERO_DEADLINE: Duration = Duration::from_secs(1);
const HERO_RESEND: Duration = Duration::from_millis(150);

#[derive(Default)]
struct ByState {
    answered: bool,
    misses: u32,
    backoffs: u32,
    first_miss: Option<Instant>,
    next_try: Option<Instant>,
    /// Last name shown, reused while the keyboard sleeps instead of degrading to "AULA wireless
    /// keyboard" when the 0x05 model query stops answering.
    name: Option<String>,
}

#[derive(Default)]
pub struct AulaKeyboard {
    by: HashMap<(u16, u16), ByState>,
}

impl AulaKeyboard {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        self.by_keyboards(api, &mut out);
        self.hero(api, &mut out);
        out
    }

    fn by_keyboards(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        let present: HashSet<(u16, u16)> = api
            .device_list()
            .filter(|d| RECEIVERS.contains(&(d.vendor_id(), d.product_id())))
            .map(|d| (d.vendor_id(), d.product_id()))
            .collect();
        self.by.retain(|key, _| present.contains(key));

        for (vid, pid) in RECEIVERS {
            if !present.contains(&(*vid, *pid)) {
                continue;
            }
            if *vid == 0x05AC && !has_mchose_layout(api, *vid, *pid) {
                continue; // a SONiX dongle
            }
            let st = self.by.entry((*vid, *pid)).or_default();
            let now = Instant::now();
            if !st.answered && st.next_try.is_some_and(|t| now < t) {
                continue;
            }

            let mut tried = false;
            let mut answered = false;
            let mut fallback_name = None;
            for info in api.device_list().filter(|d| d.vendor_id() == *vid && d.product_id() == *pid) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if !is_by_collection(*vid, info, &desc) {
                    continue;
                }
                let Some(out_len) = probe::output_len(&desc, BY_REPORT).filter(|n| *n >= BY_LEN) else { continue };
                if probe::input_len(&desc, BY_REPORT).unwrap_or(0) < BY_LEN {
                    continue;
                }
                tried = true;
                let product = info.product_string().unwrap_or("");
                let generic = product.contains("2.4G Wireless Receiver") || product.contains("MCHOSE 2.4G Wireless");
                let model = if generic || *vid == 0x05AC {
                    identify_model(&dev, out_len)
                } else {
                    None
                };
                if fallback_name.is_none() {
                    fallback_name = Some(by_name(*vid, model, generic, product));
                }
                match exchange(&dev, BY_BATTERY, BY_TIMEOUT, out_len) {
                    Some(r) if (1..=100).contains(&r[5]) => {
                        let charging = r[6] >> 4 == 1;
                        let name = by_name(*vid, model, generic, product);
                        st.name = Some(name.clone());
                        diag::event(&key(*vid, *pid), format!("battery via report 13 4A ({}%)", r[5]));
                        out.push(Observation {
                            id: key(*vid, *pid),
                            name: Some(name),
                            kind: Some(Kind::Keyboard),
                            state: State::Battery { percent: r[5], charging },
                        });
                        answered = true;
                        break;
                    }
                    Some(_) => {}
                    None => {}
                }
            }

            if answered {
                st.answered = true;
            } else if tried && !st.answered {
                let now = Instant::now();
                st.misses = st.misses.saturating_add(1);
                st.first_miss.get_or_insert(now);
                if st.misses >= 3 && st.first_miss.is_some_and(|t| now.duration_since(t) >= BACKOFF_AFTER) {
                    let doublings = st.backoffs.min(4);
                    st.backoffs = st.backoffs.saturating_add(1);
                    let secs = (BACKOFF_FIRST << doublings).min(BACKOFF_MAX);
                    st.next_try = Some(now + Duration::from_secs(secs));
                }
            }
            if !answered {
                if let Some(name) = st.name.clone().or(fallback_name) {
                    diag::event(&key(*vid, *pid), "no reply (asleep, off or not this protocol)");
                    out.push(Observation {
                        id: key(*vid, *pid),
                        name: Some(name),
                        kind: Some(Kind::Keyboard),
                        state: State::Unreachable,
                    });
                }
            }
        }
    }

    fn hero(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == HERO_VID
                && d.product_id() == HERO_PID
                && d.usage_page() == 0xFF60
                && d.usage() == 0x0061
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            let Some(out_len) = probe::output_len(&desc, HERO_ID).filter(|n| *n >= BY_LEN) else { continue };
            if probe::input_len(&desc, HERO_ID).unwrap_or(0) < BY_LEN {
                continue;
            }
            let state = match hero_battery(&dev, out_len) {
                Some((percent, charging)) => {
                    diag::event("aula:372e:106c", format!("battery via HERO protocol ({percent}%)"));
                    State::Battery { percent, charging }
                }
                None => State::Unreachable,
            };
            out.push(Observation {
                id: "aula:372e:106c".into(),
                name: Some("AULA wireless keyboard".into()),
                kind: Some(Kind::Keyboard),
                state,
            });
            return;
        }
    }
}

fn key(vid: u16, pid: u16) -> String {
    format!("aula:{vid:04x}:{pid:04x}")
}

fn by_name(vid: u16, model: Option<&str>, generic: bool, product: &str) -> String {
    if let Some(model) = model {
        return model.into();
    }
    if vid == 0x05AC && !generic && !product.is_empty() {
        return product.into();
    }
    if vid == 0x41E4 || vid == 0x05AC {
        "MCHOSE wireless keyboard".into()
    } else {
        "AULA wireless keyboard".into()
    }
}

/// 05AC:024F is shared with the SONiX dongles; the MCHOSE BY layout has an FF04:0002 feature
/// collection on interface 1. shortcut: the feature report's layout is not inspected, only the
/// collection's presence; upgrade if a SONiX dongle ever exposes FF04:0002 there too.
fn has_mchose_layout(api: &HidApi, vid: u16, pid: u16) -> bool {
    api.device_list().any(|d| {
        d.vendor_id() == vid
            && d.product_id() == pid
            && d.interface_number() == 1
            && d.usage_page() == 0xFF04
            && d.usage() == 0x0002
    })
}

fn is_by_collection(vid: u16, info: &hidapi::DeviceInfo, desc: &[u8]) -> bool {
    let page = info.usage_page();
    let usage = info.usage();
    if vid == 0x05AC {
        info.interface_number() == 1
            && page == 0xFF02
            && usage == 0x0002
            && probe::input_len(desc, BY_REPORT) == Some(BY_LEN)
            && probe::output_len(desc, BY_REPORT) == Some(BY_LEN)
    } else {
        (page == 0xFF02 || page == 0xFF04)
            && usage == 0x0002
            && probe::input_len(desc, BY_REPORT).unwrap_or(0) >= BY_LEN
            && probe::output_len(desc, BY_REPORT).unwrap_or(0) >= BY_LEN
    }
}

fn sum(frame: &[u8]) -> u8 {
    frame.iter().fold(0u8, |s, b| s.wrapping_add(*b))
}

fn by_request(cmd: u8, packets: u8, len: usize) -> Vec<u8> {
    let mut req = vec![0u8; len.max(BY_LEN)];
    req[0] = BY_REPORT;
    req[1] = cmd;
    req[2] = packets;
    req[19] = sum(&req[..19]);
    req
}

/// Reply check: report 0x13, command with the top bit masked, not an all-zero echo, and either a
/// valid sum or MCHOSE's zeroed single-packet trailer.
fn by_reply_ok(r: &[u8], cmd: u8) -> bool {
    if r.len() < BY_LEN || r[0] != BY_REPORT || r[1] & 0x7F != cmd {
        return false;
    }
    if r[4..19].iter().all(|b| *b == 0) {
        return false;
    }
    r[19] == sum(&r[..19]) || (r[19] == 0 && r[2] == 1 && r[3] == 0)
}

/// Sends the request and waits for the matching reply, resending every 1.3 s within `total`.
fn exchange(dev: &HidDevice, cmd: u8, total: Duration, len: usize) -> Option<Vec<u8>> {
    let start = Instant::now();
    let end = start + total;
    let mut sent = 0u32;
    flush(dev);
    let mut buf = [0u8; 64];
    loop {
        let now = Instant::now();
        if now >= end {
            return None;
        }
        if now >= start + BY_RESEND * sent {
            if dev.write(&by_request(cmd, (sent & 1) as u8, len)).is_err() {
                return None;
            }
            sent += 1;
        }
        let until = (start + BY_RESEND * sent).min(end);
        let ms = until.saturating_duration_since(Instant::now()).as_millis().clamp(1, 1300) as i32;
        match dev.read_timeout(&mut buf, ms) {
            Ok(n) if n >= BY_LEN => {
                let r = &buf[..n];
                if by_reply_ok(r, cmd) {
                    return Some(r.to_vec());
                }
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// The receiver's model code is at bytes 5..10; one capture had it at 4..9.
fn model_name(r: &[u8]) -> Option<&'static str> {
    for off in [5usize, 4] {
        if r.len() < off + 6 {
            continue;
        }
        if let Some((_, name)) = MODELS.iter().find(|(code, _)| &r[off..off + 6] == code) {
            return Some(name);
        }
    }
    None
}

fn identify_model(dev: &HidDevice, len: usize) -> Option<&'static str> {
    let r = exchange(dev, BY_MODEL, Duration::from_millis(1300), len)?;
    model_name(&r)
}

// --- HERO 68 Air ----------------------------------------------------------------------------



fn hero_header(counter: i32, index: u8, kind: u8) -> [u8; 20] {
    let mut f = [0u8; 20];
    f[0] = HERO_ID;
    f[1] = HERO_MAGIC;
    f[2] = 0x05 | if counter & 4 != 0 { 0x80 } else { 0 };
    f[3] = index | if counter & 2 != 0 { 0x80 } else { 0 };
    f[4] = kind | if counter & 1 != 0 { 0x80 } else { 0 };
    f
}

fn hero_data_frame(counter: i32, index: u8, msg: &[u8; 56]) -> [u8; 20] {
    let mut f = hero_header(counter, index, HERO_DATA);
    for i in 0..14 {
        f[5 + i] = msg[(index as usize - 1) * 14 + i];
    }
    f[19] = sum_range(&f, 1, 19);
    f
}

fn hero_trailer(counter: i32, msg: &[u8; 56]) -> [u8; 20] {
    let mut f = hero_header(counter, 5, HERO_TRAILER_HOST);
    f[12] = 0xFFu8.wrapping_sub(sum_range(msg, 0, msg.len()));
    f[13] = sum_range(&f, 1, 13);
    f
}

fn hero_ack(r: &[u8]) -> [u8; 20] {
    let mut f = [0u8; 20];
    f[0] = HERO_ID;
    f[1] = HERO_MAGIC;
    f[2] = r[2];
    f[3] = r[3];
    f[4] = r[4] & 0x80;
    f[5] = sum_range(&f, 1, 5);
    f
}

fn is_ack_for(r: &[u8], f: &[u8; 20]) -> bool {
    r.len() >= 6
        && r[4] & 0x7F == HERO_ACK
        && r[2] == f[2]
        && r[3] == f[3]
        && r[4] & 0x80 == f[4] & 0x80
        && r[5] == sum_range(r, 1, 5)
}

/// Battery reply or pushed telemetry from the first data frame of a keyboard message; returns
/// whether this opened a reply that a trailer will close.
fn hero_inspect(r: &[u8], level: &mut Option<u8>, flags: &mut Option<u8>) -> bool {
    if r.len() < 13 {
        return false;
    }
    if r[5] == 0x87 && (1..=100).contains(&r[11]) {
        *level = Some(r[11]);
        *flags = Some(r[12]);
        true
    } else {
        if r[5] == 0xFE && r[6] == 0x05 && (1..=100).contains(&r[7]) && level.is_none() {
            *level = Some(r[7]);
            *flags = Some(r[8]);
        }
        false
    }
}

fn hero_battery(dev: &HidDevice, len: usize) -> Option<(u8, bool)> {
    let mut msg = [0u8; 56];
    msg[0] = 0x09;
    msg[1] = 0x87;
    msg[4] = 0x01;
    flush(dev);
    let mut link = HeroLink::new(dev, len);
    if !link.send_message(&msg) {
        return None;
    }
    link.wait_for_battery();
    let level = link.level?;
    Some((level, link.flags.is_some_and(|f| f & 0x10 != 0)))
}

struct HeroLink<'a> {
    dev: &'a HidDevice,
    len: usize,
    end: Instant,
    level: Option<u8>,
    flags: Option<u8>,
    in_reply: bool,
    reply_done: bool,
    io_error: bool,
}

impl<'a> HeroLink<'a> {
    fn new(dev: &'a HidDevice, len: usize) -> Self {
        HeroLink {
            dev,
            len,
            end: Instant::now() + HERO_DEADLINE,
            level: None,
            flags: None,
            in_reply: false,
            reply_done: false,
            io_error: false,
        }
    }

    fn remaining(&self) -> i32 {
        self.end.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32
    }

    fn send_message(&mut self, msg: &[u8; 56]) -> bool {
        let mut counter = 1;
        for index in 1..=4u8 {
            if !self.send_frame(&hero_data_frame(counter, index, msg)) {
                return false;
            }
            counter = (counter + 1) & 7;
        }
        self.send_frame(&hero_trailer(counter, msg))
    }

    fn wait_for_battery(&mut self) {
        while !(self.level.is_some() && self.reply_done) {
            let left = self.remaining();
            if left <= 0 {
                break;
            }
            let mut r = [0u8; 20];
            if !self.read_frame(&mut r, left) {
                break;
            }
        }
    }

    fn write_frame(&mut self, f: &[u8; 20]) -> bool {
        let mut buf = vec![0u8; self.len.max(20)];
        buf[..20].copy_from_slice(f);
        if self.dev.write(&buf).is_err() {
            self.io_error = true;
            return false;
        }
        true
    }

    fn send_frame(&mut self, f: &[u8; 20]) -> bool {
        while self.remaining() > 0 {
            if !self.write_frame(f) {
                return false;
            }
            let resend_at = Instant::now() + HERO_RESEND;
            loop {
                let now = Instant::now();
                let until = resend_at.min(self.end);
                if now >= until {
                    break;
                }
                let ms = until.saturating_duration_since(now).as_millis().clamp(1, 150) as i32;
                let mut r = [0u8; 20];
                if !self.read_frame(&mut r, ms) {
                    if self.io_error {
                        return false;
                    }
                    break;
                }
                if is_ack_for(&r, f) {
                    return true;
                }
            }
        }
        false
    }

    fn read_frame(&mut self, r: &mut [u8; 20], wait_ms: i32) -> bool {
        let mut wait = wait_ms;
        while wait > 0 {
            let before = Instant::now();
            let n = match self.dev.read_timeout(r, wait) {
                Ok(n) => n,
                Err(_) => {
                    self.io_error = true;
                    return false;
                }
            };
            let took = before.elapsed().as_millis().min(i32::MAX as u128) as i32;
            wait -= took.max(1);
            if n < 20 || r[0] != HERO_ID || r[1] != HERO_MAGIC {
                continue;
            }
            let kind = r[4] & 0x7F;
            if kind == HERO_DATA {
                if r[19] != sum_range(r, 1, 19) {
                    continue; // damaged: no ACK, the keyboard resends
                }
                let ack = hero_ack(r);
                self.write_frame(&ack);
                if r[3] & 0x7F == 1 {
                    self.in_reply = hero_inspect(r, &mut self.level, &mut self.flags);
                }
            } else if kind == HERO_TRAILER_DEV {
                let ack = hero_ack(r);
                self.write_frame(&ack);
                if self.in_reply {
                    self.reply_done = true;
                }
                self.in_reply = false;
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_request_checksum() {
        let req = by_request(BY_BATTERY, 0, BY_LEN);
        assert_eq!(&req[..3], &[0x13, 0x4A, 0x00]);
        assert_eq!(req[19], 0x5D); // 0x13 + 0x4A
        assert_eq!(req[19], sum(&req[..19]));
    }

    #[test]
    fn by_reply_checksum_and_mchose_variant() {
        let mut r = [0u8; BY_LEN];
        r[0] = BY_REPORT;
        r[1] = BY_BATTERY;
        r[5] = 80;
        r[6] = 0x10; // AULA cable flag, high nibble 1
        r[19] = sum(&r[..19]);
        assert!(by_reply_ok(&r, BY_BATTERY));
        assert_eq!((r[5], r[6] >> 4 == 1), (80, true));

        let mut mchose = r;
        mchose[19] = 0;
        mchose[2] = 1;
        mchose[3] = 0;
        assert!(by_reply_ok(&mchose, BY_BATTERY));

        let mut echo = r;
        echo[4..19].fill(0);
        assert!(!by_reply_ok(&echo, BY_BATTERY));
        let mut wrong = r;
        wrong[1] = 0x0A; // the receiver's unrelated status frame
        assert!(!by_reply_ok(&wrong, BY_BATTERY));
        let mut bad = r;
        bad[19] ^= 0xFF;
        assert!(!by_reply_ok(&bad, BY_BATTERY));
    }

    #[test]
    fn model_codes_at_both_offsets() {
        let mut r = [0u8; BY_LEN];
        r[5..11].copy_from_slice(&[0x03, 0x00, 0x00, 0x00, 0x01, 0xDF]);
        assert_eq!(model_name(&r), Some("MCHOSE K99 V2"));
        let mut older = [0u8; BY_LEN];
        older[4..10].copy_from_slice(&[0x03, 0x00, 0x00, 0x00, 0x00, 0x6A]);
        assert_eq!(model_name(&older), Some("Redragon Horus Pro"));
        assert_eq!(model_name(&[0u8; BY_LEN]), None);
    }

    #[test]
    fn names_follow_the_upstream_rules() {
        assert_eq!(by_name(0x3554, None, false, "AULA F75"), "AULA wireless keyboard");
        assert_eq!(by_name(0x3554, Some("Redragon Horus Pro"), false, "AULA F75"), "Redragon Horus Pro");
        assert_eq!(by_name(0x41E4, None, true, "MCHOSE 2.4G Wireless"), "MCHOSE wireless keyboard");
        assert_eq!(by_name(0x05AC, None, false, "K87"), "K87");
        assert_eq!(by_name(0x05AC, None, false, ""), "MCHOSE wireless keyboard");
    }

    #[test]
    fn hero_data_frame_checksum() {
        let mut msg = [0u8; 56];
        msg[0] = 0x09;
        msg[1] = 0x87;
        msg[4] = 0x01;
        let f = hero_data_frame(1, 1, &msg);
        assert_eq!(&f[..5], &[0x09, 0x66, 0x05, 0x01, 0x8E]); // counter 1 sets byte 4's top bit
        assert_eq!(f[19], sum_range(&f, 1, 19));
        let f2 = hero_data_frame(2, 2, &msg);
        assert_eq!(f2[2], 0x05); // counter bit 1 is byte 3
        assert_eq!(f2[3], 0x82);
        assert_eq!(f2[4], 0x0E);
    }

    #[test]
    fn hero_trailer_and_ack() {
        let mut msg = [0u8; 56];
        msg[0] = 0x09;
        msg[1] = 0x87;
        let t = hero_trailer(5, &msg);
        assert_eq!(t[3], 5);
        assert_eq!(t[4] & 0x7F, 0x08);
        assert_eq!(t[4] & 0x80, 0x80); // counter bit 1
        assert_eq!(t[12], 0xFFu8.wrapping_sub(sum_range(&msg, 0, 56)));
        assert_eq!(t[13], sum_range(&t, 1, 13));

        let ack = hero_ack(&t);
        assert_eq!(ack[4] & 0x7F, 0x00);
        assert_eq!(ack[4] & 0x80, t[4] & 0x80);
        assert!(is_ack_for(&ack, &t));
        assert!(!is_ack_for(&ack, &hero_header(5, 4, HERO_DATA)));
    }

    #[test]
    fn hero_battery_reply_and_push() {
        // The reply starts at frame byte 5: 87 00 00 01 00 02 <level> <flags>.
        let mut msg = [0u8; 56];
        msg[..8].copy_from_slice(&[0x87, 0x00, 0x00, 0x01, 0x00, 0x02, 78, 0x10]);
        let f = hero_data_frame(1, 1, &msg);
        let mut level = None;
        let mut flags = None;
        assert!(hero_inspect(&f, &mut level, &mut flags));
        assert_eq!((level, flags), (Some(78), Some(0x10)));

        let mut push = [0u8; 56];
        push[..4].copy_from_slice(&[0xFE, 0x05, 55, 0x00]);
        let f = hero_data_frame(1, 1, &push);
        let mut level = None;
        let mut flags = None;
        assert!(!hero_inspect(&f, &mut level, &mut flags));
        assert_eq!((level, flags), (Some(55), Some(0x00)));
    }
}
