//! Logitech / ASTRO wireless headsets: the A50 X and A50 Gen 5 (046D:0B0B / 0B1C), plus the
//! 9886 base stations (A50 Gen 4, A50 Gen 3 and A20 Wireless).
//!
//! A50 X and Gen 5: vendor collection FF32:0074, 64-byte report 0x02.
//! `02 0C 03 00 06 0C …` asks for the status; the reply is
//! `02 0C <len> 00 06 <handle> <percent> … <charging>`, percent in `[6]` and charging in `[8]`.
//!
//! A50 Gen 4 / Gen 3 and A20: vendor collection FF32 / FF31 / FF00, usage 0x0074, report
//! 0x02. `02 <cmd>` is answered by `02 02 <len> <payload>`, where status 01 means the command
//! was refused and the headset is not linked. Command 0x54 gives dock/link state (bit 0
//! docked, bit 1 live), 0x7C the battery (bit 7 charging, bits 0..6 percent). A50 Gen 3
//! rejects 0x7C and keeps the level in the 0x72 "current UI" read instead: the field-04 group
//! (`data[12..15]`) is `02 04 <battery>`.
//!
//! Some Gen 3 firmware declares no report id, so the frame is retried behind a leading 00
//! report-id byte; the framing that works is remembered per product id.
//!
//! Protocol from Wireless-Device-Battery-Monitor (drivers/astro.cpp), which credits
//! HeadsetControl, eh-fifty, AstroBar and OpenAstroCC. Only these read-only queries are sent.
//! Skipped on purpose: the cached reading Astro Command Center's exclusive lock would need,
//! and the grace period after the headset stops answering.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{DeviceInfo, HidApi, HidDevice, HidResult};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const LOGITECH: u16 = 0x046D;
const ASTRO: u16 = 0x9886;

/// 9886 base stations: (product id, name).
const LEGACY: &[(u16, &str)] = &[
    // A50 Gen 4 (Mimas)
    (0x002C, "ASTRO A50 Gen 4"),
    (0x002D, "ASTRO A50 Gen 4"),
    (0x002A, "ASTRO A50 Gen 4"),
    // A50 Gen 3 (Europa)
    (0x0015, "ASTRO A50 Gen 3"),
    (0x0014, "ASTRO A50 Gen 3"),
    (0x0018, "ASTRO A50 Gen 3"),
    (0x001D, "ASTRO A50 Gen 3"),
    (0x0026, "ASTRO A50 Gen 3"),
    // A20 Wireless (Aquarius)
    (0x0037, "ASTRO A20 Wireless"),
    (0x0038, "ASTRO A20 Wireless"),
    (0x0039, "ASTRO A20 Wireless"),
    (0x0043, "ASTRO A20 Wireless"),
    (0x0044, "ASTRO A20 Wireless"),
    (0x0045, "ASTRO A20 Wireless"),
];

#[derive(Clone, Copy, PartialEq)]
enum Framing {
    Unknown,
    ReportId02,
    NoReportId,
}

#[derive(Clone, Copy, PartialEq)]
enum Answer {
    Ok,
    Refused,
    Silent,
}

#[derive(Default)]
pub struct Astro {
    /// Which framing each base station answered on.
    framing: HashMap<u16, Framing>,
    /// Keys that answered once: a headset that goes quiet keeps its last reading.
    seen: HashSet<String>,
}

impl Astro {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();

        // A50 X and A50 Gen 5.
        for (pid, name) in [(0x0B0B, "ASTRO A50 X"), (0x0B1C, "ASTRO A50 Gen 5")] {
            let key = format!("astro:{pid:04x}");
            let mut state = None;
            for info in api.device_list().filter(|d| {
                d.vendor_id() == LOGITECH && d.product_id() == pid && d.usage_page() == 0xFF32 && d.usage() == 0x0074
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, 0x02) != Some(64) || probe::input_len(&desc, 0x02) != Some(64) {
                    continue;
                }
                match a50x(&dev) {
                    Ok(Some(s)) => {
                        state = Some(s);
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => diag::event(&key, format!("i/o failed: {e}")),
                }
            }
            self.add(&mut out, &key, name, state);
        }

        // A50 Gen 4 / Gen 3 and A20 Wireless.
        for &(pid, name) in LEGACY {
            let key = format!("astro:{pid:04x}");
            let mut state = None;
            for info in api.device_list().filter(|d| d.vendor_id() == ASTRO && d.product_id() == pid) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if !legacy_accept(info, &desc) {
                    continue;
                }
                match legacy(&dev, pid, &mut self.framing) {
                    Ok(Some(s)) => {
                        state = Some(s);
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => diag::event(&key, format!("i/o failed: {e}")),
                }
            }
            self.add(&mut out, &key, name, state);
        }
        out
    }

    fn add(&mut self, out: &mut Vec<Observation>, key: &str, name: &str, state: Option<State>) {
        match state {
            Some(state) => {
                self.seen.insert(key.into());
                out.push(observation(key, name, state));
            }
            None if self.seen.contains(key) => out.push(observation(key, name, State::Unreachable)),
            None => {}
        }
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Headset), state }
}

fn drain(dev: &HidDevice) -> HidResult<()> {
    let mut buf = [0u8; 128];
    while dev.read_timeout(&mut buf, 0)? > 0 {}
    Ok(())
}

fn a50x(dev: &HidDevice) -> HidResult<Option<State>> {
    drain(dev)?;
    let mut req = [0u8; 64];
    req[0] = 0x02;
    req[1] = 0x0C;
    req[2] = 3;
    req[4] = 0x06;
    req[5] = 0x0C;
    dev.write(&req)?;

    let mut buf = [0u8; 128];
    let deadline = Instant::now() + Duration::from_secs(1);
    for _ in 0..8 {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
        let n = match dev.read_timeout(&mut buf, left.as_millis().max(1) as i32) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(e),
        };
        if let Some(s) = a50x_reply(&buf[..n]) {
            return Ok(Some(s));
        }
    }
    Ok(None)
}

/// Ignores unsolicited volume/mix events and malformed or partial frames.
fn a50x_reply(r: &[u8]) -> Option<State> {
    if r.len() < 9 || r[0] != 0x02 || r[1] != 0x0C {
        return None;
    }
    let len = usize::from(r[2]);
    if len < 6 || len + 3 > r.len() || r[3] != 0 || r[4] != 0x06 || !matches!(r[5], 0 | 0x0C) || r[6] > 100 {
        return None;
    }
    Some(State::Battery { percent: r[6], charging: r[8] != 0 })
}

fn legacy_accept(info: &DeviceInfo, desc: &[u8]) -> bool {
    matches!(info.usage_page(), 0xFF32 | 0xFF31 | 0xFF00)
        && probe::input_len(desc, 0x02).is_some_and(|n| n >= 64)
        && probe::output_len(desc, 0x02).is_some_and(|n| n >= 64)
}

/// The legacy status query, framing included. `Silent` means no frame came back at all.
fn ask(dev: &HidDevice, framing: Framing, cmd: u8, payload: &[u8]) -> HidResult<(Answer, Vec<u8>)> {
    drain(dev)?;
    let mut req = Vec::with_capacity(4 + payload.len());
    if framing == Framing::NoReportId {
        req.push(0x00);
    }
    req.extend_from_slice(&[0x02, cmd]);
    if !payload.is_empty() {
        req.push(payload.len() as u8);
        req.extend_from_slice(payload);
    }
    dev.write(&req)?;

    let mut buf = [0u8; 128];
    let deadline = Instant::now() + Duration::from_millis(500);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let n = match dev.read_timeout(&mut buf, left.as_millis().max(1) as i32) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(e),
        };
        if n < 4 || buf[0] != 0x02 {
            continue;
        }
        match buf[1] {
            0x01 => return Ok((Answer::Refused, Vec::new())), // understood, but refused
            0x02 => return Ok((Answer::Ok, buf[..n].to_vec())),
            _ => {}
        }
    }
    Ok((Answer::Silent, Vec::new()))
}

fn legacy(dev: &HidDevice, pid: u16, framing: &mut HashMap<u16, Framing>) -> HidResult<Option<State>> {
    let known = framing.get(&pid).copied().unwrap_or(Framing::Unknown);
    let order = if known == Framing::NoReportId {
        [Framing::NoReportId, Framing::ReportId02]
    } else {
        [Framing::ReportId02, Framing::NoReportId]
    };

    // 0x54 (dock / link) is optional: some Gen 3 firmware may not answer it.
    let mut answer = Answer::Silent;
    let mut reply = Vec::new();
    let mut used = known;
    for f in order {
        let (a, r) = ask(dev, f, 0x54, &[])?;
        answer = a;
        reply = r;
        used = f;
        if a != Answer::Silent {
            break;
        }
        if known != Framing::Unknown {
            break; // framing already proven: a miss is a real miss
        }
    }
    if answer != Answer::Silent {
        framing.insert(pid, used);
    }

    let have_status = answer == Answer::Ok;
    let mut docked = false;
    if have_status {
        let st = reply[3];
        docked = st & 0x01 != 0;
        if !(docked || st & 0x02 != 0) {
            return Ok(Some(State::Unreachable));
        }
    }

    let (mut bat_answer, mut bat) = ask(dev, used, 0x7C, &[])?;
    if bat_answer == Answer::Refused && have_status {
        // A50 Gen 3 rejects 0x7C; its level is in the 0x72 "current UI" read.
        if let Some(value) = gen3(dev, used)? {
            bat_answer = Answer::Ok;
            bat = vec![0x02, 0x02, 0x03, value];
        }
    }

    if bat_answer == Answer::Ok
        && let Some((percent, charge_flag)) = legacy_battery(&bat)
    {
        let charging = charge_flag || (docked && percent < 100);
        return Ok(Some(State::Battery { percent, charging }));
    }
    if bat_answer == Answer::Refused && !have_status {
        return Ok(Some(State::Unreachable));
    }
    Ok(have_status.then_some(State::Unreachable))
}

/// Gen 3 battery from `02 02 <len> <data…>`: `data[12..15]` is `02 04 <level>`.
fn gen3(dev: &HidDevice, framing: Framing) -> HidResult<Option<u8>> {
    let (answer, r) = ask(dev, framing, 0x72, &[0x01])?;
    if answer != Answer::Ok {
        return Ok(None);
    }
    Ok(gen3_value(&r))
}

fn gen3_value(r: &[u8]) -> Option<u8> {
    if r.len() < 3 || (r[2] & 0x3F) < 16 || r.len() < 3 + 16 {
        return None;
    }
    let data = &r[3..];
    if data[12] != 0x02 || data[13] != 0x04 || (data[14] & 0x7F) > 100 {
        return None;
    }
    Some(data[14])
}

/// Bit 7 charging, bits 0..6 the level.
fn legacy_battery(r: &[u8]) -> Option<(u8, bool)> {
    let value = *r.get(3)?;
    let percent = value & 0x7F;
    (percent <= 100).then_some((percent, value & 0x80 != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_frame(payload: &[u8]) -> Vec<u8> {
        let mut r = vec![0x02, 0x02, payload.len() as u8];
        r.extend_from_slice(payload);
        r
    }

    #[test]
    fn parses_a50x_status_reply() {
        // Reply layout from HeadsetControl: handle 00, 87%, charging flag at [8].
        let mut r = vec![0u8; 64];
        r[..7].copy_from_slice(&[0x02, 0x0C, 0x06, 0x00, 0x06, 0x00, 87]);
        assert_eq!(a50x_reply(&r), Some(State::Battery { percent: 87, charging: false }));
        r[8] = 1;
        assert_eq!(a50x_reply(&r), Some(State::Battery { percent: 87, charging: true }));
        r[1] = 0x0D; // a volume event, not the status
        assert_eq!(a50x_reply(&r), None);
        r[1] = 0x0C;
        r[6] = 101;
        assert_eq!(a50x_reply(&r), None);
        assert_eq!(a50x_reply(&r[..8]), None); // short
    }

    #[test]
    fn parses_legacy_battery_bits() {
        assert_eq!(legacy_battery(&legacy_frame(&[95])), Some((95, false)));
        assert_eq!(legacy_battery(&legacy_frame(&[0x80 | 55])), Some((55, true)));
        assert_eq!(legacy_battery(&legacy_frame(&[101])), None);
        assert_eq!(legacy_battery(&[0x02, 0x02, 0x01]), None);
    }

    #[test]
    fn parses_gen3_current_ui() {
        let mut payload = [0u8; 16];
        payload[12] = 0x02;
        payload[13] = 0x04;
        payload[14] = 87;
        assert_eq!(gen3_value(&legacy_frame(&payload)), Some(87));
        payload[14] = 0x80 | 42; // still a level: bits 0..6, bit 7 is the charge flag
        assert_eq!(gen3_value(&legacy_frame(&payload)), Some(0x80 | 42));
        payload[13] = 0x05; // not the field-04 group
        assert_eq!(gen3_value(&legacy_frame(&payload)), None);
        let mut short = legacy_frame(&payload);
        short[2] = 8; // length byte says too little data
        assert_eq!(gen3_value(&short), None);
    }

    #[test]
    fn legacy_names_are_unique() {
        for (i, (pid, name)) in LEGACY.iter().enumerate() {
            assert!(LEGACY[i + 1..].iter().all(|(p, _)| p != pid), "pid {pid:04x} twice");
            assert!(name.starts_with("ASTRO"));
        }
    }
}
