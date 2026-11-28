//! Finalmouse UltralightX (ULX) and Starlight X (SLX) on their 2.4 GHz dongles (VID 361D).
//!
//! Frames on the vendor collection (usage page FF00), zero-padded to the report length:
//! `[report id][inner length = 2 + n][0x80 | command][n][n payload bytes]`. A write with an empty
//! payload is a GET; replies come back as input reports with the plain command.
//!
//! * ULX dongle 0100 (FF00:0001): output report 04, replies on input report 05.
//! * SLX dongles 0300 / 0301: output report 01, replies on input 02.
//!
//! Only GETs are sent: the read xpanel starts with (ULX lift-off 21, SLX profile roster 57), then
//! wake/all (96), which makes the dongle push its status burst. From that burst: command 5 is the
//! battery voltage (mV, LE) mapped with xpanel's calibration curve, 38 the state of charge
//! `[percent][mV LE]` (preferred when present), 37 the charging state and 36 the link state
//! (0 = no link to the mouse).
//!
//! Protocol from xpanel, as ported by OpenMouse mouse-protocol and
//! xBambooz/FinalmousePollingRateSwitcher, via Wireless-Device-Battery-Monitor
//! (drivers/finalmouse.cpp). The mouse on its own cable (0102, 0310, 0311) is not covered: its
//! report ids are not known.

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const VID: u16 = 0x361D;
const VOLTAGE: u8 = 5;
const LINK: u8 = 36;
const CHARGING: u8 = 37;
const STATE_OF_CHARGE: u8 = 38;
const WAKE_ALL: u8 = 96;
const BURST_WAIT: Duration = Duration::from_millis(1000);
/// Once the level is in, the charging flag normally follows within the same burst.
const TAIL_WAIT: Duration = Duration::from_millis(150);

struct Model {
    pid: u16,
    name: &'static str,
    slx: bool,
    out_id: u8,
    in_id: u8,
    /// The read xpanel sends before wake/all.
    first_get: u8,
}

const MODELS: &[Model] = &[
    Model { pid: 0x0100, name: "Finalmouse UltralightX", slx: false, out_id: 0x04, in_id: 0x05, first_get: 21 },
    Model { pid: 0x0300, name: "Finalmouse Starlight X", slx: true, out_id: 0x01, in_id: 0x02, first_get: 57 },
    Model { pid: 0x0301, name: "Finalmouse Starlight X", slx: true, out_id: 0x01, in_id: 0x02, first_get: 57 },
];

#[derive(Default)]
pub struct Finalmouse {
    seen: HashSet<String>,
}

impl Finalmouse {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for model in MODELS {
            let key = format!("finalmouse:{:04x}", model.pid);
            let mut infos: Vec<_> = api
                .device_list()
                .filter(|d| {
                    d.vendor_id() == VID
                        && d.product_id() == model.pid
                        && d.usage_page() == 0xFF00
                        && (model.slx || d.usage() == 1)
                })
                .collect();
            // The SLX vendor usage is not known; try usage 1 (the ULX one) first.
            infos.sort_by_key(|d| d.usage() != 1);

            let mut state = None;
            for info in infos {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::input_len(&desc, model.in_id).unwrap_or(0) < 4
                    || probe::output_len(&desc, model.out_id).unwrap_or(0) < 4
                {
                    continue;
                }
                if let Some(s) = query(&dev, model) {
                    state = Some(s);
                    break;
                }
            }

            if let Some(state) = state {
                self.seen.insert(key.clone());
                out.push(observation(&key, model.name, state));
            } else if self.seen.contains(&key) {
                out.push(observation(&key, model.name, State::Unreachable));
            }
        }
        out
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

#[derive(Default)]
struct Status {
    soc: Option<u8>,
    mv: i32,
    charging: Option<u8>,
    link: Option<u8>,
}

fn get(dev: &HidDevice, report: u8, command: u8) -> hidapi::HidResult<()> {
    dev.write(&[report, 2, 0x80 | command, 0]).map(|_| ())
}

fn query(dev: &HidDevice, m: &Model) -> Option<State> {
    get(dev, m.out_id, m.first_get).ok()?;
    get(dev, m.out_id, WAKE_ALL).ok()?;

    let mut st = Status::default();
    let mut deadline = Instant::now() + BURST_WAIT;
    let mut shortened = false;
    let mut buf = [0u8; 64];
    loop {
        let level = st.soc.is_some() || (!m.slx && st.mv > 0);
        if level && st.charging.is_some() {
            break;
        }
        if level && !shortened {
            deadline = deadline.min(Instant::now() + TAIL_WAIT);
            shortened = true;
        }
        let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(0) => break,
            Ok(n) => decode(&buf[..n], m.in_id, &mut st),
            Err(_) => return None,
        }
    }

    // The SLX's own percentage is preferred over its voltage.
    let percent = match st.soc {
        Some(soc) => Some(soc),
        None if st.mv > 0 => Some(percent_from_mv(st.mv)),
        None => None,
    };
    match percent {
        Some(percent) => Some(State::Battery { percent, charging: st.charging == Some(1) }),
        // The dongle answered, the mouse is asleep or off: keep the last value.
        None if st.link == Some(0) => Some(State::Unreachable),
        // No answer at all: nothing to show.
        None => None,
    }
}

fn decode(r: &[u8], in_id: u8, st: &mut Status) {
    if r.len() < 4 || r[0] != in_id {
        return;
    }
    let inner = usize::from(r[1]).min(r.len() - 2);
    if inner < 2 {
        return;
    }
    let n = usize::from(r[3]).min(inner - 2);
    let p = &r[4..4 + n];
    match r[2] {
        VOLTAGE => {
            if let Some(mv) = voltage(p, 2) {
                st.mv = mv;
            }
        }
        STATE_OF_CHARGE => {
            if n >= 3 {
                if p[0] <= 100 {
                    st.soc = Some(p[0]);
                }
                if let Some(mv) = voltage(p, 3) {
                    st.mv = mv;
                }
            }
        }
        CHARGING => {
            if n >= 1 {
                st.charging = Some(p[0]);
            }
        }
        LINK => {
            if n >= 1 {
                st.link = Some(p[0]);
            }
        }
        _ => {}
    }
}

/// A valid voltage at `p[1..]` (two bytes, little endian).
fn voltage(p: &[u8], n: usize) -> Option<i32> {
    if p.len() < n {
        return None;
    }
    let mv = i32::from(p[n - 2]) | i32::from(p[n - 1]) << 8;
    (mv > 0 && mv < 5000).then_some(mv)
}

/// xpanel's voltage curve: linear between the calibration points, rounded to the nearest percent.
fn percent_from_mv(mv: i32) -> u8 {
    const MV: [i32; 7] = [3000, 3620, 3660, 3740, 3880, 4170, 4380];
    const PCT: [f64; 7] = [0.2, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0];
    if mv <= MV[0] {
        return 0;
    }
    for i in 1..MV.len() {
        if mv > MV[i] {
            continue;
        }
        let f = f64::from(mv - MV[i - 1]) / f64::from(MV[i] - MV[i - 1]);
        return (PCT[i - 1] + (PCT[i] - PCT[i - 1]) * f + 0.5).floor() as u8;
    }
    100
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(cmd: u8, payload: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; 4 + payload.len()];
        r[0] = 0x05;
        r[1] = 2 + payload.len() as u8;
        r[2] = cmd;
        r[3] = payload.len() as u8;
        r[4..].copy_from_slice(payload);
        r
    }

    #[test]
    fn decodes_the_status_burst() {
        let mut st = Status::default();
        decode(&frame(VOLTAGE, &[0x6A, 0x0E]), 0x05, &mut st); // 3690 mV
        assert_eq!(st.mv, 3690);

        decode(&frame(STATE_OF_CHARGE, &[87, 0x6A, 0x0E]), 0x05, &mut st);
        assert_eq!(st.soc, Some(87));
        assert_eq!(st.mv, 3690);

        decode(&frame(CHARGING, &[1]), 0x05, &mut st);
        decode(&frame(LINK, &[0]), 0x05, &mut st);
        assert_eq!((st.charging, st.link), (Some(1), Some(0)));
    }

    #[test]
    fn rejects_other_report_ids_and_bad_lengths() {
        let mut st = Status::default();
        decode(&frame(VOLTAGE, &[0x6A, 0x0E]), 0x04, &mut st);
        assert_eq!(st.mv, 0);

        // Inner length claims one payload byte, the payload byte count claims two.
        let mut r = frame(VOLTAGE, &[0x6A, 0x0E]);
        r[1] = 3;
        decode(&r, 0x05, &mut st);
        assert_eq!(st.mv, 0); // only one payload byte fits, so no voltage

        // A 0 mV or out-of-range voltage is not a reading.
        decode(&frame(VOLTAGE, &[0x00, 0x00]), 0x05, &mut st);
        assert_eq!(st.mv, 0);
        decode(&frame(VOLTAGE, &[0xFF, 0xFF]), 0x05, &mut st);
        assert_eq!(st.mv, 0);
    }

    #[test]
    fn voltage_curve_matches_xpanel() {
        assert_eq!(percent_from_mv(3000), 0);
        assert_eq!(percent_from_mv(3620), 5);
        assert_eq!(percent_from_mv(3660), 10);
        assert_eq!(percent_from_mv(3740), 25);
        assert_eq!(percent_from_mv(3880), 50);
        assert_eq!(percent_from_mv(4170), 75);
        assert_eq!(percent_from_mv(4380), 100);
        assert_eq!(percent_from_mv(3690), 16); // between 3660 (10 %) and 3740 (25 %)
        assert_eq!(percent_from_mv(4500), 100);
    }

    #[test]
    fn soc_out_of_range_is_ignored() {
        let mut st = Status::default();
        decode(&frame(STATE_OF_CHARGE, &[101, 0x6A, 0x0E]), 0x05, &mut st);
        assert_eq!(st.soc, None);
        assert_eq!(st.mv, 3690); // the voltage inside the frame is still valid
    }
}
