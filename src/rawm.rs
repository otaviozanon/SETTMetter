//! RAWM / SADES mice on Nordic/Panchip receivers and USB cables (VID 1915).
//!
//! RAWM HUB "Web 2.0" protocol: unnumbered 64-byte reports on FF00:0001. The host sends a JSON
//! device-info query and keeps pinging; replies are an event stream (`FF FF FF FF`, command/length,
//! body). `CMD_QUERY_RESULT` carries `{"battery":..,"chr":..,"pi":..,"dn":".."}`, and
//! `CMD_NOTIFY` type 0x17 carries `[battery, charging]`.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/rawm.cpp and rawm_protocol.h), verified
//! on a Leviathan V4 through the RAWM HS Receiver.

use crate::device::{Kind, Observation, State};
use crate::hidutil::drain as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VID: u16 = 0x1915;
const RECEIVERS: &[u16] = &[0x232A, 0x232B, 0x2335, 0x2336, 0x233B, 0x233C, 0x2346, 0x2347, 0x234B];
const MICE: &[u16] = &[
    0x2328, 0x2329, 0x232C, 0x232D, 0x232E, 0x232F, 0x2330, 0x2331, 0x2332, 0x2333, 0x2334, 0x2337, 0x2338,
    0x2339, 0x233A, 0x233E, 0x233F, 0x2340, 0x2341, 0x2343, 0x2344, 0x2348, 0x2349, 0x234A, 0x2352,
];
const QUERY: u8 = 0x01;
const QUERY_RESULT: u8 = 0x02;
const NOTIFY: u8 = 0x0B;
const PING: u8 = 0x0E;
const NOTIFY_BATTERY: u8 = 0x17;

#[derive(Default)]
pub struct Rawm {
    seen: HashSet<String>,
}

impl Rawm {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out: Vec<Observation> = Vec::new();
        for &pid in RECEIVERS.iter().chain(MICE.iter()) {
            let receiver = RECEIVERS.contains(&pid);
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0xFF00 && d.usage() == 0x0001
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, 0).unwrap_or(0) < 64 || probe::input_len(&desc, 0).unwrap_or(0) < 64 {
                    continue;
                }
                flush(&dev);
                let deadline = Instant::now() + Duration::from_millis(1200);
                let mut link = Link::default();
                let Some(mut status) = query(&dev, &mut link, None, deadline) else { continue };
                if receiver {
                    let Some(channel) = query(&dev, &mut link, Some(0), deadline) else {
                        continue;
                    };
                    status = channel;
                }
                let Some(percent) = status.percent else { continue };
                let mouse_pid = status.json_int("pi").filter(|v| *v > 0 && *v <= 0xFFFF).map(|v| v as u16);
                let key = format!("rawm:{:04x}", mouse_pid.unwrap_or(pid));
                self.seen.insert(key.clone());
                let mut name = status.json_string("dn").unwrap_or_default();
                if name.is_empty() {
                    name = "wireless mouse".into();
                }
                if !name.contains("RAWM") && !name.contains("SADES") {
                    name = format!("RAWM {name}");
                }
                let state = State::Battery { percent, charging: status.charging };
                if let Some(old) = out.iter_mut().find(|o| o.id == key) {
                    // Prefer the cable's charging state when both links answer.
                    if !receiver {
                        old.state = state;
                    }
                } else {
                    out.push(Observation { id: key, name: Some(name), kind: Some(Kind::Mouse), state });
                }
            }
        }
        let missing: Vec<String> =
            self.seen.iter().filter(|k| !out.iter().any(|o| o.id == **k)).cloned().collect();
        for key in missing {
            out.push(Observation {
                id: key,
                name: Some("RAWM wireless mouse".into()),
                kind: Some(Kind::Mouse),
                state: State::Unreachable,
            });
        }
        out
    }
}

#[derive(Default)]
struct Link {
    direct: Stream,
    channel0: Stream,
}

impl Link {
    fn feed(&mut self, report: &[u8]) {
        if report.is_empty() {
            return;
        }
        let (stream, r) = if report[0] & 0xC0 == 0xC0 {
            if report[0] & 0x3F != 0 {
                return;
            }
            (&mut self.channel0, &report[1..])
        } else {
            (&mut self.direct, report)
        };
        if r.is_empty() || r[0] & 0xC0 != 0x80 {
            return;
        }
        let n = usize::from(r[0] & 0x3F).min(r.len() - 1);
        stream.append(&r[1..1 + n]);
    }
}

#[derive(Default)]
struct Stream {
    buf: Vec<u8>,
    status: Status,
}

#[derive(Default)]
struct Status {
    have_info: bool,
    json: String,
    percent: Option<u8>,
    charging: bool,
}

impl Status {
    fn json_int(&self, key: &str) -> Option<i64> {
        let needle = format!("\"{key}\":");
        let at = self.json.find(&needle)? + needle.len();
        let rest = &self.json[at..];
        let end = rest.find(|c: char| !c.is_ascii_digit() && c != '-').unwrap_or(rest.len());
        rest[..end].parse().ok()
    }

    fn json_string(&self, key: &str) -> Option<String> {
        let needle = format!("\"{key}\":\"");
        let at = self.json.find(&needle)? + needle.len();
        let end = self.json[at..].find('"')?;
        Some(self.json[at..at + end].to_string())
    }
}

impl Stream {
    fn append(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        if self.buf.len() > 8192 {
            let drain = self.buf.len() - 4096;
            self.buf.drain(..drain);
        }
        self.parse();
    }

    fn parse(&mut self) {
        let mut o = 0;
        while o + 6 <= self.buf.len() {
            if !self.buf[o..o + 4].iter().all(|b| *b == 0xFF) {
                o += 1;
                continue;
            }
            let h = o + 4;
            let cmd = self.buf[h] & 0x0F;
            let len = (usize::from(self.buf[h] & 0xF0) << 4) | usize::from(self.buf[h + 1]);
            if len < 2 {
                o = h;
                continue;
            }
            if h + len > self.buf.len() {
                break;
            }
            let body = self.buf[h + 2..h + len].to_vec();
            if cmd == QUERY_RESULT {
                self.on_info(&body);
            } else if cmd == NOTIFY && body.len() >= 3 && body[0] == NOTIFY_BATTERY && body[1] <= 100 {
                self.status.percent = Some(body[1]);
                self.status.charging = body[2] == 1;
            }
            o = h + len;
        }
        self.buf.drain(..o);
    }

    fn on_info(&mut self, body: &[u8]) {
        let mut s = String::from_utf8_lossy(body).to_string();
        if let Some(z) = s.find('\0') {
            s.truncate(z);
        }
        if !s.starts_with('{') {
            return;
        }
        self.status.have_info = true;
        self.status.json = s;
        if let Some(v) = self.status.json_int("battery").filter(|v| (0..=100).contains(v)) {
            self.status.percent = Some(v as u8);
        }
        if let Some(v) = self.status.json_int("chr") {
            self.status.charging = v == 1;
        }
    }
}

fn query(dev: &HidDevice, link: &mut Link, channel: Option<u8>, deadline: Instant) -> Option<Status> {
    send(dev, &query_event(), channel)?;
    while Instant::now() < deadline {
        send(dev, &ping_event(), channel)?;
        pump(dev, link, 80, deadline);
        let s = if channel.is_some() { &link.channel0.status } else { &link.direct.status };
        if s.have_info {
            return Some(Status {
                have_info: true,
                json: s.json.clone(),
                percent: s.percent,
                charging: s.charging,
            });
        }
    }
    None
}

fn send(dev: &HidDevice, event: &[u8], channel: Option<u8>) -> Option<()> {
    let mut packet = vec![0u8];
    if let Some(ch) = channel {
        packet.push(0xC0 | (ch & 0x3F));
    }
    packet.push(0x80 | (event.len() as u8 & 0x3F));
    packet.extend_from_slice(event);
    dev.write(&packet).ok()?;
    Some(())
}

fn query_event() -> Vec<u8> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut e = vec![QUERY, 0, 0x03, 0, 0];
    e.extend_from_slice(&secs.to_le_bytes());
    e[1] = e.len() as u8;
    e
}

fn ping_event() -> [u8; 4] {
    [PING, 4, 0, 0]
}

fn pump(dev: &HidDevice, link: &mut Link, ms: u64, deadline: Instant) {
    let end = (Instant::now() + Duration::from_millis(ms)).min(deadline);
    let mut buf = [0u8; 64];
    while Instant::now() < end {
        let left = end.saturating_duration_since(Instant::now()).as_millis().max(1) as i32;
        match dev.read_timeout(&mut buf, left) {
            Ok(0) => break,
            Ok(n) => link.feed(&buf[..n]),
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_query_result_json() {
        let mut s = Stream::default();
        let json = br#"{"battery":83,"chr":1,"pi":9034,"dn":"Leviathan V4"}"#;
        let mut ev = vec![0xFF, 0xFF, 0xFF, 0xFF, QUERY_RESULT, (json.len() + 2) as u8];
        ev.extend_from_slice(json);
        s.append(&ev);
        assert!(s.status.have_info);
        assert_eq!(s.status.percent, Some(83));
        assert!(s.status.charging);
        assert_eq!(s.status.json_int("pi"), Some(9034));
        assert_eq!(s.status.json_string("dn").as_deref(), Some("Leviathan V4"));
    }

    #[test]
    fn query_event_length_and_ping() {
        let e = query_event();
        assert_eq!(e[0], QUERY);
        assert_eq!(usize::from(e[1]), e.len());
        assert_eq!(ping_event(), [PING, 4, 0, 0]);
    }
}
