//! Royal Kludge (RK) MG6 Pro wireless mouse, vendor id 2E3C.
//!
//! Protocol from RK's web driver (drive.rkgaming.com): the MG6 Pro uses the "QiWang" protocol.
//! Its getDeviceBattery() sends an unencrypted 64-byte frame as output report 0:
//!   `[0]` msg type 1 (forwarded to the mouse), `[1]` main config type 1 with data length 0 in
//!   the high nibble, `[2]` sub config type 2 (GET_BATTER_VOL), `[3]` property byte with bit 0 =
//!   READ.
//! The reply has the same first three bytes; `[3]` bit 4 is access_res (1 = the mouse answered),
//! `[4..5]` voltage (mV, little endian), `[6..7]` percent, `[8..9]` temperature and `[10]` USB
//! plugged in. Replies with access_res 0 mean the receiver answered for a mouse that did not.
//!
//! 00A6 / 00E6 are the 2.4 GHz dongle (vendor page FF00:0001), 00A5 / 00E5 the USB cable
//! (FFF2:0001). Protocol from Wireless-Device-Battery-Monitor (drivers/royal_kludge.cpp); only
//! the battery read is sent.

use crate::device::{Kind, Observation, State};
use crate::hidutil::drain as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const VID: u16 = 0x2E3C;
const DONGLE_PIDS: [u16; 2] = [0x00A6, 0x00E6];
const CABLE_PIDS: [u16; 2] = [0x00A5, 0x00E5];
const DONGLE_PAGE: u16 = 0xFF00;
const CABLE_PAGE: u16 = 0xFFF2;
const WAIT: Duration = Duration::from_millis(600);

#[derive(Default)]
pub struct RoyalKludge {
    seen: HashSet<String>,
}

impl RoyalKludge {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for &pid in DONGLE_PIDS.iter().chain(CABLE_PIDS.iter()) {
            let cable = CABLE_PIDS.contains(&pid);
            let page = if cable { CABLE_PAGE } else { DONGLE_PAGE };
            let key = format!("rk:{pid:04x}");
            let mut found = None;
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == page && d.usage() == 0x0001
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if caps(probe::output_len(&desc, 0)) != 65 || caps(probe::input_len(&desc, 0)) != 65 {
                    continue;
                }
                match poll(&dev) {
                    Some(Poll::Battery { percent, plugged }) => {
                        found = Some(State::Battery { percent, charging: plugged || cable });
                        break;
                    }
                    Some(Poll::Asleep) => {
                        // The receiver answered, the mouse did not.
                        found = Some(State::Unreachable);
                        break;
                    }
                    None => {}
                }
            }
            match found {
                Some(state) => {
                    self.seen.insert(key.clone());
                    out.push(observation(&key, state));
                }
                None => {
                    if self.seen.contains(&key) {
                        out.push(observation(&key, State::Unreachable));
                    }
                }
            }
        }
        out
    }
}

fn observation(key: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some("RK MG6 Pro".into()), kind: Some(Kind::Mouse), state }
}

/// Windows' report length: hidapi hides the report-id byte of an unnumbered report.
fn caps(len: Option<usize>) -> usize {
    len.map_or(0, |l| l + 1)
}

enum Poll {
    Battery { percent: u8, plugged: bool },
    /// The receiver answered for a mouse that did not.
    Asleep,
}

fn poll(dev: &HidDevice) -> Option<Poll> {
    let mut answered = false;
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        flush(dev);
        if dev.write(&[0x00, 0x01, 0x01, 0x02, 0x01]).is_err() {
            break;
        }
        let deadline = Instant::now() + WAIT;
        let mut buf = [0u8; 64];
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match dev.read_timeout(&mut buf, left.as_millis() as i32) {
                Ok(0) => break,
                Ok(n) => match reply(&buf[..n]) {
                    Reply::Other => {} // other notifications on this collection
                    Reply::Denied => {
                        answered = true;
                        break;
                    }
                    Reply::Battery { percent, plugged } => {
                        return Some(Poll::Battery { percent, plugged });
                    }
                },
                Err(_) => return None,
            }
        }
    }
    answered.then_some(Poll::Asleep)
}

enum Reply {
    Other,
    Denied,
    Battery { percent: u8, plugged: bool },
}

fn reply(r: &[u8]) -> Reply {
    if r.len() < 11 || (r[0] & 0x1F) != 0x01 || (r[1] & 0x0F) != 0x01 || r[2] != 0x02 {
        return Reply::Other;
    }
    if (r[3] >> 4) & 1 == 0 {
        return Reply::Denied; // access_res 0: the mouse did not answer
    }
    let percent = u16::from(r[6]) | u16::from(r[7]) << 8;
    if percent > 100 {
        return Reply::Denied;
    }
    Reply::Battery { percent: percent as u8, plugged: r[10] != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> [u8; 16] {
        // 0x3C00 = 3900 mV, 90 %, 0x012C = 30.0 degrees, USB plugged in.
        [0x01, 0x01, 0x02, 0x10, 0x2C, 0x01, 0x5A, 0x00, 0x01, 0x2C, 0x01, 0x00, 0, 0, 0, 0]
    }

    #[test]
    fn parses_a_battery_reply() {
        assert!(matches!(reply(&frame()), Reply::Battery { percent: 90, plugged: true }));
    }

    #[test]
    fn other_notifications_and_denied_reads_are_ignored() {
        let mut r = frame();
        r[2] = 0x03; // another subsystem
        assert!(matches!(reply(&r), Reply::Other));

        r = frame();
        r[3] = 0x00; // access_res 0
        assert!(matches!(reply(&r), Reply::Denied));

        r = frame();
        r[6] = 0x65; // 101 %
        assert!(matches!(reply(&r), Reply::Denied));

        assert!(matches!(reply(&frame()[..10]), Reply::Other));
    }
}
