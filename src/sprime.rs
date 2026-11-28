//! Sprime PM1 wireless mouse (Nordic vendor id 0x1915), on its 2.4 GHz receiver or the USB cable.
//!
//! Protocol from Sprime's web driver (sprime.pro): the same feature report 0x05, command 0x15
//! status query as the Ninjutso Sora V2 (see ninjutso.rs).
//!   SET_FEATURE report 0x05, 32-byte payload: `05 15 00 00 01 00 …`
//!   GET_FEATURE report 0x05: `05 15` … [9] percent, [10] charging, [11] fully charged, [12]
//!   online. Indices include the report id, as in WebHID's returned DataView.
//!
//! Product ids from the web driver's WebHID filters: cable AC11 (black) / AC12 (white) / AC13
//! (red); receivers AC1C and AD8C. The bootloader (AC0B) is never touched.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::Duration;

const VID: u16 = 0x1915;
const CABLES: &[u16] = &[0xAC11, 0xAC12, 0xAC13];
const RECEIVERS: &[u16] = &[0xAC1C, 0xAD8C];
const REPORT_ID: u8 = 0x05;
const CMD: u8 = 0x15;
const LEN: usize = 32;

#[derive(Default)]
pub struct Sprime;

impl Sprime {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        let mut pids: Vec<u16> = api
            .device_list()
            .filter(|d| d.vendor_id() == VID && (CABLES.contains(&d.product_id()) || RECEIVERS.contains(&d.product_id())))
            .map(|d| d.product_id())
            .collect();
        pids.sort_unstable();
        pids.dedup();

        for pid in pids {
            let key = format!("sprime:{VID:04x}:{pid:04x}");
            let cable = CABLES.contains(&pid);
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0xFFA0 && d.usage() == 0x0001
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::feature_len(&desc, REPORT_ID).is_none_or(|n| n < LEN) {
                    continue;
                }
                match query(&dev) {
                    Ok(Some(reply)) => {
                        diag::event(&key, format!("battery via report 0x{REPORT_ID:02x}"));
                        out.push(observation(&key, "Sprime PM1", state(&reply, cable)));
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => diag::event(&key, format!("i/o failed: {e}")),
                }
            }
        }
        out
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

fn query(dev: &HidDevice) -> hidapi::HidResult<Option<[u8; LEN]>> {
    let mut req = [0u8; LEN];
    req[0] = REPORT_ID;
    req[1] = CMD;
    req[4] = 0x01;
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(90));
        }
        dev.send_feature_report(&req)?;
        std::thread::sleep(Duration::from_millis(15));
        let mut buf = [0u8; LEN];
        buf[0] = REPORT_ID;
        let n = dev.get_feature_report(&mut buf)?;
        if n >= 13 && buf[0] == REPORT_ID && buf[1] == CMD && buf[9] <= 100 {
            return Ok(Some(buf));
        }
    }
    Ok(None)
}

/// On the cable the mouse answers [12] = 0 but counts as connected. A fully charged mouse reports
/// 100% and is not charging.
fn state(r: &[u8], cable: bool) -> State {
    if !(cable || r[12] != 0) {
        return State::Unreachable;
    }
    let full = r[11] != 0;
    State::Battery { percent: if full { 100 } else { r[9] }, charging: r[10] != 0 && !full }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(percent: u8, charging: u8, full: u8, online: u8) -> [u8; LEN] {
        let mut r = [0u8; LEN];
        r[0] = REPORT_ID;
        r[1] = CMD;
        r[9] = percent;
        r[10] = charging;
        r[11] = full;
        r[12] = online;
        r
    }

    #[test]
    fn charging_and_full_semantics() {
        assert_eq!(state(&reply(87, 0, 0, 1), false), State::Battery { percent: 87, charging: false });
        assert_eq!(state(&reply(87, 1, 0, 1), false), State::Battery { percent: 87, charging: true });
        // Fully charged: 100% and not charging.
        assert_eq!(state(&reply(87, 1, 1, 1), false), State::Battery { percent: 100, charging: false });
        // On the cable the link byte is 0.
        assert_eq!(state(&reply(87, 0, 0, 0), true), State::Battery { percent: 87, charging: false });
        // Receiver without a link: off or out of range.
        assert_eq!(state(&reply(87, 0, 0, 0), false), State::Unreachable);
    }
}
