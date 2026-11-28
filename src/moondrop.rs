//! MOONDROP GM01 Pro wireless headset (35D8:0133, WalkPlay chipset).
//!
//! 2.4 GHz dongle: the WalkPlay control channel sits on a consumer-control collection with
//! 64-byte input/output reports, report id `0x4B`. The USB interface number varies (MI_03 or
//! MI_04&Col03). Commands are `[4B][dir][cmd][len][data...]` with dir `80` = read, `01` = write;
//! only reads are sent here:
//!
//! ```text
//! [4B][80][27][00] -> [4B][80][27][06][00][00][00][00][link][percent]
//! [4B][80][31][00] -> [4B][80][31][01][link]
//! ```
//!
//! `link` is `01` while the headset is on and paired, `percent` is the level the Moondrop Link app
//! shows. Command `31` is answered by the dongle itself and says `00` once the headset switched to
//! its USB cable. On the cable the headset enumerates on interface 3 with a single
//! consumer-control collection (no `&colNN` in the path) and no control channel, so there is no
//! level to read: it is reported as charging and the app keeps the last level, dimmed.
//!
//! Verified on hardware (firmware 1.5.3); protocol framing from devicePEQ (walkplayHidHandler.js),
//! battery command found with a read-only scan of the READ ids. Ported from moondrop.cpp.

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const VID: u16 = 0x35D8; // WalkPlay; shared with other DACs/dongles, so only known product ids
const PID: u16 = 0x0133;
const KEY: &str = "moondrop:0133";
const REPORT: u8 = 0x4B;
const READ: u8 = 0x80;
const CMD_BATTERY: u8 = 0x27;
const CMD_LINK: u8 = 0x31;
const PAGE_CONSUMER: u16 = 0x000C;
const USAGE_CONSUMER_CONTROL: u16 = 0x0001;

/// Fresh instances are made on every poll, so the last level lives in a module static: the cable
/// reports charging but no level, and the app keeps the last one.
static LAST: Mutex<Option<u8>> = Mutex::new(None);

#[derive(Default)]
pub struct Moondrop;

impl Moondrop {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut on_cable = false;
        let mut dongle_unlinked = false;
        let mut percent = None;

        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == PAGE_CONSUMER && d.usage() == USAGE_CONSUMER_CONTROL
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            let out_len = probe::output_len(&desc, REPORT).unwrap_or(0);
            let in_len = probe::input_len(&desc, REPORT).unwrap_or(0);
            if out_len < 4 || in_len < 10 {
                // Interface 3 has a single collection (no &colNN) on the cable and no control channel.
                on_cable |= info.interface_number() == 3
                    && !info.path().to_string_lossy().to_ascii_lowercase().contains("&col");
                continue;
            }
            if query(&dev, out_len, in_len.max(64), CMD_LINK, 1).is_some_and(|reply| link_up(&reply) == Some(false)) {
                dongle_unlinked = true;
                continue;
            }
            if let Some(level) = query(&dev, out_len, in_len.max(64), CMD_BATTERY, 6).and_then(|reply| battery_level(&reply)) {
                percent = Some(level);
                break;
            }
        }

        let state = match (percent, on_cable, dongle_unlinked) {
            (Some(percent), ..) => {
                *LAST.lock().unwrap() = Some(percent);
                State::Battery { percent, charging: false }
            }
            // On the cable the level is unknown: keep the last one and show it as charging.
            (None, true, _) => match *LAST.lock().unwrap() {
                Some(percent) => State::Battery { percent, charging: true },
                None => return Vec::new(),
            },
            (None, false, true) => State::Off,
            // Present but silent: let the app keep the last value through its grace period.
            (None, false, false) => State::Unreachable,
        };
        if matches!(state, State::Off) {
            // The dongle says the headset is off; don't reuse the level after it comes back.
            *LAST.lock().unwrap() = None;
        }
        vec![Observation { id: KEY.into(), name: Some("MOONDROP GM01 Pro".into()), kind: Some(Kind::Headset), state }]
    }
}

/// Sends a READ for `cmd` and waits for its reply (`[4B][80][cmd][len]...`).
fn query(dev: &HidDevice, out_len: usize, in_len: usize, cmd: u8, payload_len: usize) -> Option<Vec<u8>> {
    drain(dev, in_len);
    let mut request = vec![0u8; out_len];
    request[..4].copy_from_slice(&[REPORT, READ, cmd, 0x00]);
    dev.write(&request).ok()?;

    let deadline = Instant::now() + Duration::from_millis(400);
    let mut buf = vec![0u8; in_len];
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        let n = dev.read_timeout(&mut buf, left.as_millis().max(1) as i32).ok()?;
        if n == 0 {
            return None;
        }
        let reply = &buf[..n];
        if reply.len() >= 4 + payload_len
            && reply[0] == REPORT
            && reply[1] == READ
            && reply[2] == cmd
            && usize::from(reply[3]) >= payload_len
            && reply.len() >= 4 + usize::from(reply[3])
        {
            return Some(reply.to_vec());
        }
    }
}

fn drain(dev: &HidDevice, len: usize) {
    let mut buf = vec![0u8; len];
    while dev.read_timeout(&mut buf, 0).is_ok_and(|n| n > 0) {}
}

/// `[4B][80][31][01][link]`: answered by the dongle itself, 0 once the headset switched to its cable.
fn link_up(reply: &[u8]) -> Option<bool> {
    if reply.len() < 5 || reply[0] != REPORT || reply[1] != READ || reply[2] != CMD_LINK || reply[3] < 1 {
        return None;
    }
    Some(reply[4] != 0)
}

/// `[4B][80][27][06][00][00][00][00][link][percent]`: the level only counts while linked.
fn battery_level(reply: &[u8]) -> Option<u8> {
    if reply.len() < 10 || reply[0] != REPORT || reply[1] != READ || reply[2] != CMD_BATTERY || reply[3] < 6 {
        return None;
    }
    (reply[8] != 0).then_some(reply[9]).filter(|p| *p <= 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_reply() {
        // [4B][80][27][06][00][00][00][00][link=01][percent=50]: matched Moondrop Link at 50%.
        assert_eq!(battery_level(&[0x4B, 0x80, 0x27, 0x06, 0, 0, 0, 0, 0x01, 0x32]), Some(50));
        // Headset not linked: no level.
        assert_eq!(battery_level(&[0x4B, 0x80, 0x27, 0x06, 0, 0, 0, 0, 0x00, 0x32]), None);
        // Out-of-range level, short reply, another command.
        assert_eq!(battery_level(&[0x4B, 0x80, 0x27, 0x06, 0, 0, 0, 0, 0x01, 0x65]), None);
        assert_eq!(battery_level(&[0x4B, 0x80, 0x27, 0x06, 0, 0, 0, 0, 0x01]), None);
        assert_eq!(battery_level(&[0x4B, 0x80, 0x31, 0x01, 0x01]), None);
    }

    #[test]
    fn link_reply() {
        assert_eq!(link_up(&[0x4B, 0x80, 0x31, 0x01, 0x01]), Some(true));
        // On the cable the headset drops the 2.4 GHz link; the dongle answers 00.
        assert_eq!(link_up(&[0x4B, 0x80, 0x31, 0x01, 0x00]), Some(false));
        assert_eq!(link_up(&[0x4B, 0x80, 0x27, 0x06]), None);
    }
}
