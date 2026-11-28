//! CHERRY wireless keyboards and mice.
//!
//! MX 3.0S Wireless (dongle 046A:00EA, cable 046A:00E3): 64-byte report 0x04 on FF1C:0092.
//! Frame `04 <sum lo> <sum hi> 1A 03 00 00 55 …`, checksum = sum of bytes 3..end; reply bytes 8
//! and 9 are the level and the USB-powered (charging) flag. The framing comes from
//! B1Chat/BatteryWatch, verified on a PIXIU 75 receiver; the MX 3.0S uses the same channel.
//!
//! Cherry XTRFY 1K Mouse Receiver (046A:0330): the shared Compx 17-byte report 0x08 on
//! FF02:0002, checksum = 0x55 - sum(first 16). Command 03 reports the link, command 04 the
//! level (byte 6), charging (byte 7) and millivolts (bytes 8-9, big endian).
//!
//! Both from Wireless-Device-Battery-Monitor (drivers/cherry.cpp). Read-only.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::checksum55 as checksum;
use crate::hidutil::drain_len as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x046A;
const PAGE: u16 = 0xFF1C;
const USAGE: u16 = 0x0092;
const LEN: usize = 64;
const REPORT_ID: u8 = 0x04;
const STATUS: u8 = 0x1A;

/// Cable first: the keyboard is awake and powered then, even if the dongle is still attached.
const MODELS: &[(u16, &str)] = &[(0x00E3, "USB cable"), (0x00EA, "2.4 GHz receiver")];

#[derive(Default)]
pub struct Cherry;

impl Cherry {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        let mut found = false;
        for (pid, _) in MODELS {
            let infos: Vec<_> = api
                .device_list()
                .filter(|d| {
                    d.vendor_id() == VID && d.product_id() == *pid && d.usage_page() == PAGE && d.usage() == USAGE
                })
                .collect();
            if infos.is_empty() {
                continue;
            }
            found = true;
            for info in infos {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::input_len(&desc, REPORT_ID) != Some(LEN) || probe::output_len(&desc, REPORT_ID) != Some(LEN) {
                    continue;
                }
                match query_keyboard(&dev) {
                    Ok(Some(reply)) if reply[8] <= 100 => {
                        let percent = reply[8];
                        let charging = reply[9] != 0;
                        diag::event("cherry:046a:00ea", format!("battery via report 04 ({percent}%)"));
                        out.push(Observation {
                            id: "cherry:046a:00ea".into(),
                            name: Some("CHERRY MX 3.0S Wireless".into()),
                            kind: Some(Kind::Keyboard),
                            state: State::Battery { percent, charging },
                        });
                        xtrfy(api, &mut out);
                        return out;
                    }
                    Ok(Some(_)) => {
                        // Answered without a usable level: keep the last reading, dimmed.
                        out.push(Observation {
                            id: "cherry:046a:00ea".into(),
                            name: Some("CHERRY MX 3.0S Wireless".into()),
                            kind: Some(Kind::Keyboard),
                            state: State::Unreachable,
                        });
                        xtrfy(api, &mut out);
                        return out;
                    }
                    Ok(None) => {}
                    Err(e) => diag::event("cherry:046a:00ea", format!("i/o failed: {e}")),
                }
            }
        }
        if found {
            out.push(Observation {
                id: "cherry:046a:00ea".into(),
                name: Some("CHERRY MX 3.0S Wireless".into()),
                kind: Some(Kind::Keyboard),
                state: State::Unreachable,
            });
        }
        xtrfy(api, &mut out);
        out
    }
}

/// The one frame the MX 3.0S driver sends.
fn keyboard_query() -> [u8; LEN] {
    let mut f = [0u8; LEN];
    f[0] = REPORT_ID;
    f[3] = STATUS;
    f[4] = 0x03;
    f[7] = 0x55;
    let sum = f[3..].iter().fold(0u8, |s, b| s.wrapping_add(*b));
    f[1] = sum;
    f[2] = 0;
    f
}

fn query_keyboard(dev: &HidDevice) -> hidapi::HidResult<Option<Vec<u8>>> {
    let query = keyboard_query();
    for _ in 0..2 {
        // The dongle only answers while the keyboard is awake.
        flush(dev, LEN);
        dev.write(&query)?;
        let deadline = Instant::now() + Duration::from_millis(400);
        let mut buf = [0u8; LEN];
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let ms = (left.as_millis() as i32).clamp(1, 100);
            match dev.read_timeout(&mut buf, ms) {
                Ok(0) => {}
                Ok(n) if n >= 10 && buf[0] == REPORT_ID && buf[3] == STATUS => {
                    return Ok(Some(buf[..n].to_vec()));
                }
                Ok(_) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(None)
}

// --- Cherry XTRFY mouse receiver ------------------------------------------------------------

const XTRFY_PID: u16 = 0x0330;
const XTRFY_LEN: usize = 17;
const XTRFY_REPORT: u8 = 0x08;
const XTRFY_LINK: u8 = 0x03;
const XTRFY_POWER: u8 = 0x04;

fn xtrfy(api: &HidApi, out: &mut Vec<Observation>) {
    const KEY: &str = "cherry:046a:0330";
    for info in api.device_list().filter(|d| {
        d.vendor_id() == VID && d.product_id() == XTRFY_PID && d.usage_page() == 0xFF02 && d.usage() == 0x0002
    }) {
        let Ok(dev) = api.open_path(info.path()) else { return };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if probe::input_len(&desc, XTRFY_REPORT) != Some(XTRFY_LEN)
            || probe::output_len(&desc, XTRFY_REPORT) != Some(XTRFY_LEN)
        {
            continue;
        }

        let mut answered = false;
        let mut connected = false;
        let mut reading = None;
        match exchange(&dev, XTRFY_LINK) {
            Ok(Some(r)) => {
                answered = true;
                connected = r[6] != 0;
            }
            Ok(None) => {}
            Err(e) => diag::event(KEY, format!("i/o failed: {e}")),
        }
        if !answered || connected {
            match exchange(&dev, XTRFY_POWER) {
                Ok(Some(r)) => {
                    answered = true;
                    connected = true;
                    if r[6] <= 100 {
                        let millivolts = (u16::from(r[8]) << 8) | u16::from(r[9]);
                        diag::event(KEY, format!("battery via report 08 ({}%, {millivolts} mV)", r[6]));
                        reading = Some((r[6], r[7] != 0));
                    }
                }
                Ok(None) => {}
                Err(e) => diag::event(KEY, format!("i/o failed: {e}")),
            }
        }

        let state = if !answered {
            State::Unreachable // switched off or asleep: keep the last reading
        } else if !connected {
            State::Off
        } else {
            match reading {
                Some((percent, charging)) => State::Battery { percent, charging },
                None => State::Unreachable,
            }
        };
        out.push(Observation {
            id: KEY.into(),
            name: Some("Cherry XTRFY wireless mouse".into()),
            kind: Some(Kind::Mouse),
            state,
        });
        return;
    }
}

fn xtrfy_query(cmd: u8) -> [u8; XTRFY_LEN] {
    let mut f = [0u8; XTRFY_LEN];
    f[0] = XTRFY_REPORT;
    f[1] = cmd;
    f[XTRFY_LEN - 1] = checksum(&f[..XTRFY_LEN - 1]);
    f
}

fn exchange(dev: &HidDevice, cmd: u8) -> hidapi::HidResult<Option<Vec<u8>>> {
    flush(dev, XTRFY_LEN);
    dev.write(&xtrfy_query(cmd))?;
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut buf = [0u8; XTRFY_LEN];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let ms = (left.as_millis() as i32).clamp(1, 100);
        match dev.read_timeout(&mut buf, ms) {
            Ok(0) => {}
            Ok(n) if n == XTRFY_LEN && buf[0] == XTRFY_REPORT && buf[1] == cmd && checksum(&buf[..XTRFY_LEN - 1]) == buf[XTRFY_LEN - 1] => {
                return Ok(Some(buf.to_vec()));
            }
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// The XTRFY frames sum to 0x55 (mod 256).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mx30s_query_checksum() {
        let q = keyboard_query();
        assert_eq!(q[0], 0x04);
        assert_eq!(q[3], 0x1A);
        // 0x1A + 0x03 + 0x55 = 0x72
        assert_eq!(u16::from_le_bytes([q[1], q[2]]), 0x72);
    }

    #[test]
    fn mx30s_reply_layout() {
        let mut r = [0u8; LEN];
        r[0] = REPORT_ID;
        r[3] = STATUS;
        r[8] = 95;
        r[9] = 1;
        assert_eq!((r[8], r[9] != 0), (95, true));
    }

    #[test]
    fn xtrfy_captured_frame() {
        // Live from a Cherry XTRFY 1K receiver: 95%, on battery, 4084 mV.
        let frame = [0x08, 0x04, 0x00, 0x00, 0x00, 0x02, 0x5f, 0x00, 0x0f, 0xf4, 0, 0, 0, 0, 0, 0, 0xe5];
        assert_eq!(checksum(&frame[..16]), 0xe5);
        assert_eq!((frame[6], frame[7] != 0, (u16::from(frame[8]) << 8) | u16::from(frame[9])), (95, false, 4084));
        let mut bad = frame;
        bad[16] ^= 0xFF;
        assert_ne!(checksum(&bad[..16]), bad[16]);
    }

    #[test]
    fn xtrfy_link_request_checksum() {
        assert_eq!(checksum(&xtrfy_query(XTRFY_LINK)[..16]), xtrfy_query(XTRFY_LINK)[16]);
        assert_eq!(checksum(&xtrfy_query(XTRFY_POWER)[..16]), 0x49);
    }
}
