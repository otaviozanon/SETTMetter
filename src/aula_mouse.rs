//! AULA SC580 / SC580X mouse, on its USB cable (088D:062E) or 2.4 GHz receiver (089D:062F).
//!
//! The vendor collection is FF02:0000 with 33-byte reports (report id 0 + 32-byte payload).
//! Command 0x30 (GetDeviceInfo, read-only) is what the vendor app polls for the battery:
//! request payload `30 00 01 00 … 00`; the reply echoes 0x30, status 0 OK / 1 busy / 2 mouse
//! offline / 3 data error, and its data (payload bytes 4..30) holds `M137`, firmware, sensor id,
//! unknown, charge state (0 battery, 1 charging, 2 full), level 0..100 and an online flag.
//! Status 0 is checked with a sum over bytes 4..30. A reply to a previous command means "send
//! again". Pushed `D1 <online> <level> <charging>` frames are ignored.
//!
//! While charging the cable reports a fixed 100 regardless of the real level, so the last real
//! reading is kept and only the charging mark changes (charge state 2 keeps its 100). The cable
//! answers first and the receiver is left alone when it does.
//!
//! Protocol from Crucialjun/omarchy-aula-SC580 (PROTOCOL.md, sc580ctl, MIT), reverse engineered
//! from CBMouse.exe; verified upstream on an SC580 over both connections. Read-only.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::drain as flush;
use crate::hidutil::sum_range;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const INFO_CMD: u8 = 0x30;
const PAYLOAD: usize = 32;
const DEADLINE: Duration = Duration::from_secs(1);
const RESEND: Duration = Duration::from_millis(250);

/// (vid, pid, on a cable)
const IDS: &[(u16, u16, bool)] = &[(0x088D, 0x062E, true), (0x089D, 0x062F, false)];

#[derive(Default)]
pub struct AulaMouse {
    /// Last real (non-charging) level, shown while the cable reports its fixed 100.
    last: Option<u8>,
}

impl AulaMouse {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for (vid, pid, cable) in IDS {
            if let Some(o) = self.query(api, *vid, *pid, *cable) {
                out.push(o);
                return out; // the cable wins; the receiver is not asked when it answered
            }
        }
        out
    }

    fn query(&mut self, api: &HidApi, vid: u16, pid: u16, cable: bool) -> Option<Observation> {
        let infos: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == vid && d.product_id() == pid && d.usage_page() == 0xFF02 && d.usage() == 0)
            .collect();
        let mut saw = false;
        for info in infos {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            let Some(payload) = probe::output_len(&desc, 0) else { continue };
            if payload < PAYLOAD || probe::input_len(&desc, 0).unwrap_or(0) < PAYLOAD {
                continue;
            }
            saw = true;
            let len = payload + 1; // report id 0
            match sc_info(&dev, len) {
                ScResult::Ok(data) => return Some(self.observation(&data)),
                ScResult::Offline => return Some(offline_observation()),
                ScResult::Failed => {}
            }
        }
        // A silent receiver means the mouse is asleep or off; a silent cable means "not here".
        if saw && !cable {
            Some(offline_observation())
        } else {
            None
        }
    }

    fn observation(&mut self, data: &[u8]) -> Observation {
        let state = match sc_battery(data) {
            Battery::Level(percent) => {
                self.last = Some(percent);
                State::Battery { percent, charging: false }
            }
            Battery::ChargingUnknown => match self.last {
                Some(percent) => State::Battery { percent, charging: true },
                None => State::Unreachable,
            },
            Battery::Offline | Battery::NoReading => State::Unreachable,
        };
        diag::event("aula:sc580", format!("battery via GetDeviceInfo: {state:?}"));
        Observation {
            id: "aula:sc580".into(),
            name: Some("AULA SC580".into()),
            kind: Some(Kind::Mouse),
            state,
        }
    }
}

fn offline_observation() -> Observation {
    diag::event("aula:sc580", "mouse offline or receiver silent");
    Observation {
        id: "aula:sc580".into(),
        name: Some("AULA SC580".into()),
        kind: Some(Kind::Mouse),
        state: State::Unreachable,
    }
}

#[derive(Debug, PartialEq)]
enum Battery {
    Level(u8),
    /// Charge state 1: the mouse reports a fixed 100, not the real level.
    ChargingUnknown,
    Offline,
    NoReading,
}

/// Data layout from byte 4 of the reply: [8] charge state, [9] level, [10] online.
fn sc_battery(data: &[u8]) -> Battery {
    if data.len() < 11 {
        return Battery::NoReading;
    }
    if data[10] == 0 {
        return Battery::Offline;
    }
    if data[9] > 100 {
        return Battery::NoReading;
    }
    if data[8] == 1 {
        Battery::ChargingUnknown
    } else {
        Battery::Level(data[9])
    }
}

#[derive(Debug, PartialEq)]
enum ScFrame {
    Ok(Vec<u8>),
    Busy,
    Offline,
    /// A reply to an earlier command, or a status that asks for a resend.
    Stale,
    /// Pushed telemetry or a damaged frame: keep listening.
    Ignore,
}

/// One reply frame (report id 0 already stripped). Data is bytes 4..30, checked by byte 31.
fn sc_frame(r: &[u8]) -> ScFrame {
    if r.len() < PAYLOAD {
        return ScFrame::Ignore;
    }
    if r[0] == 0xD1 || r[0] == 0xD2 {
        return ScFrame::Ignore;
    }
    if r[0] != INFO_CMD {
        return ScFrame::Stale;
    }
    match r[1] {
        0 if r[31] == sum_range(r, 4, 31) => ScFrame::Ok(r[4..31].to_vec()),
        0 => ScFrame::Ignore,
        1 => ScFrame::Busy,
        2 => ScFrame::Offline,
        _ => ScFrame::Stale,
    }
}

enum ScResult {
    Ok(Vec<u8>),
    Offline,
    Failed,
}

fn sc_request(len: usize) -> Vec<u8> {
    let mut req = vec![0u8; len.max(PAYLOAD + 1)];
    req[1] = INFO_CMD;
    req[3] = 0x01;
    // The checksum over empty data (payload bytes 4..30) is 0.
    req
}

/// Sends GetDeviceInfo and waits for an OK reply, resending on busy or a stale reply within one
/// second. A busy status waits 300 ms first.
fn sc_info(dev: &HidDevice, len: usize) -> ScResult {
    let end = Instant::now() + DEADLINE;
    flush(dev);
    let req = sc_request(len);
    let mut buf = [0u8; 64];
    loop {
        if Instant::now() >= end {
            return ScResult::Failed;
        }
        if dev.write(&req).is_err() {
            return ScResult::Failed;
        }
        let resend_at = Instant::now() + RESEND;
        let mut resend = false;
        while !resend {
            let now = Instant::now();
            let until = resend_at.min(end);
            if now >= until {
                break;
            }
            let ms = until.saturating_duration_since(now).as_millis().clamp(1, 250) as i32;
            match dev.read_timeout(&mut buf, ms) {
                Ok(n) if n >= PAYLOAD => match sc_frame(&buf[..n]) {
                    ScFrame::Ok(data) => return ScResult::Ok(data),
                    ScFrame::Offline => return ScResult::Offline,
                    ScFrame::Busy => {
                        let sleep = end.saturating_duration_since(Instant::now()).as_millis().min(300) as u64;
                        std::thread::sleep(Duration::from_millis(sleep));
                        resend = true;
                    }
                    ScFrame::Stale => resend = true,
                    ScFrame::Ignore => {}
                },
                Ok(_) => {}
                Err(_) => return ScResult::Failed,
            }
        }
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout() {
        let req = sc_request(33);
        assert_eq!(&req[..5], &[0x00, 0x30, 0x00, 0x01, 0x00]);
        assert_eq!(req[32], 0); // sum over empty data
    }

    #[test]
    fn reply_status_and_checksum() {
        let mut r = [0u8; PAYLOAD];
        r[0] = INFO_CMD;
        r[1] = 0;
        let mut data = [0u8; 27];
        data[0..4].copy_from_slice(b"M137");
        data[9] = 87;
        data[10] = 1;
        r[4..31].copy_from_slice(&data);
        r[31] = sum_range(&r, 4, 31);
        assert_eq!(sc_frame(&r), ScFrame::Ok(data.to_vec()));

        let mut busy = r;
        busy[1] = 1;
        assert_eq!(sc_frame(&busy), ScFrame::Busy);
        let mut offline = r;
        offline[1] = 2;
        assert_eq!(sc_frame(&offline), ScFrame::Offline);
        let mut stale = r;
        stale[0] = 0x31;
        assert_eq!(sc_frame(&stale), ScFrame::Stale);
        let mut push = r;
        push[0] = 0xD1;
        assert_eq!(sc_frame(&push), ScFrame::Ignore);
        let mut bad = r;
        bad[31] ^= 0xFF;
        assert_eq!(sc_frame(&bad), ScFrame::Ignore);
    }

    #[test]
    fn battery_states() {
        let data = |state: u8, level: u8, online: u8| [0u8, 0, 0, 0, 0, 0, 0, 0, state, level, online];
        assert_eq!(sc_battery(&data(0, 87, 1)), Battery::Level(87));
        assert_eq!(sc_battery(&data(1, 100, 1)), Battery::ChargingUnknown);
        assert_eq!(sc_battery(&data(2, 100, 1)), Battery::Level(100)); // full keeps its 100
        assert_eq!(sc_battery(&data(0, 87, 0)), Battery::Offline);
        assert_eq!(sc_battery(&data(0, 101, 1)), Battery::NoReading);
        assert_eq!(sc_battery(&[0u8; 5]), Battery::NoReading);
    }
}
