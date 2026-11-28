//! Ninjutso mice: the Sora V2 (1K receiver 1915:AE1C, USB cable 1915:AE11) and the Ten / Ten Air
//! (093A:EB01 receiver, 093A:E020 USB cable).
//!
//! Sora V2, from NinjaForce's bundled createSoraV2 driver (`readState()`): feature report 0x05 on
//! vendor collection 0xFFA0:0001, command 0x15, 32-byte payload `05 15 00 00 01 00 …`. The reply
//! is `05 15 …` with [9] the battery (0 means 100%), [10] charging and [12] online. Indices
//! include the report id, as in WebHID's returned DataView. On the cable the same query answers
//! [12] = 0 (tested on a Sora V2), so the cable counts as connected, as with the Sprime PM1.
//!
//! Ten / Ten Air, from NinjaForce and DrStopor/ninjutso-battery-tray: feature report 0x06
//! (16 bytes) on 0xFF01:0001; commands 0x13 (online), 0x12 (battery %) and 0x11 (charging), the
//! value at byte 8. Verified on a Ten Air.
//!
//! Only these read-only status queries are sent.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::Duration;

const SORA_VID: u16 = 0x1915;
const SORA_RECEIVER: u16 = 0xAE1C;
const SORA_CABLE: u16 = 0xAE11;
const SORA_REPORT: u8 = 0x05;
const SORA_CMD: u8 = 0x15;
const SORA_LEN: usize = 32;

const TEN_VID: u16 = 0x093A;
const TEN_RECEIVER: u16 = 0xEB01;
const TEN_CABLE: u16 = 0xE020;
const TEN_REPORT: u8 = 0x06;
const TEN_ONLINE: u8 = 0x13;
const TEN_PERCENT: u8 = 0x12;
const TEN_CHARGING: u8 = 0x11;
const TEN_LEN: usize = 16;

#[derive(Default)]
pub struct Ninjutso;

impl Ninjutso {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        // The receiver is tried before the cable, as upstream does.
        for pid in [SORA_RECEIVER, SORA_CABLE] {
            let key = format!("ninjutso:{SORA_VID:04x}:{pid:04x}");
            let cable = pid == SORA_CABLE;
            for info in api.device_list().filter(|d| {
                d.vendor_id() == SORA_VID && d.product_id() == pid && d.usage_page() == 0xFFA0 && d.usage() == 0x0001
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::feature_len(&desc, SORA_REPORT).is_none_or(|n| n < SORA_LEN) {
                    continue;
                }
                match sora_query(&dev) {
                    Ok(Some(reply)) => {
                        let state = sora_state(&reply, cable);
                        diag::event(&key, format!("battery via report 0x{SORA_REPORT:02x}"));
                        out.push(observation(&key, "Ninjutso Sora V2", state));
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => diag::event(&key, format!("i/o failed: {e}")),
                }
            }
        }
        self.scan_ten(api, &mut out);
        out
    }

    fn scan_ten(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for pid in [TEN_RECEIVER, TEN_CABLE] {
            let key = format!("ninjutso:ten:{pid:04x}");
            for info in api.device_list().filter(|d| {
                d.vendor_id() == TEN_VID && d.product_id() == pid && d.usage_page() == 0xFF01 && d.usage() == 0x0001
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::feature_len(&desc, TEN_REPORT) != Some(TEN_LEN) {
                    continue;
                }
                let reply = match ten_query(&dev, TEN_ONLINE) {
                    Ok(Some(online)) => online,
                    Ok(None) => continue,
                    Err(e) => {
                        diag::event(&key, format!("i/o failed: {e}"));
                        continue;
                    }
                };
                if reply != 1 {
                    // The dongle says the mouse is off or asleep.
                    continue;
                }
                let percent = match ten_query(&dev, TEN_PERCENT) {
                    Ok(Some(p)) if p <= 100 => p,
                    Ok(_) => continue,
                    Err(e) => {
                        diag::event(&key, format!("i/o failed: {e}"));
                        continue;
                    }
                };
                let charging = match ten_query(&dev, TEN_CHARGING) {
                    Ok(Some(c)) => c != 0,
                    Ok(None) => continue,
                    Err(e) => {
                        diag::event(&key, format!("i/o failed: {e}"));
                        continue;
                    }
                };
                let name = info
                    .product_string()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Ninjutso Ten / Ten Air");
                diag::event(&key, format!("battery {percent}% via report 0x{TEN_REPORT:02x}"));
                out.push(observation(&key, name, ten_state(percent, charging)));
                break;
            }
        }
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

fn sora_query(dev: &HidDevice) -> hidapi::HidResult<Option<[u8; SORA_LEN]>> {
    let mut req = [0u8; SORA_LEN];
    req[0] = SORA_REPORT;
    req[1] = SORA_CMD;
    req[4] = 0x01;
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(90));
        }
        dev.send_feature_report(&req)?;
        std::thread::sleep(Duration::from_millis(15));
        let mut buf = [0u8; SORA_LEN];
        buf[0] = SORA_REPORT;
        let n = dev.get_feature_report(&mut buf)?;
        if n >= 13 && buf[0] == SORA_REPORT && buf[1] == SORA_CMD && buf[9] <= 100 {
            return Ok(Some(buf));
        }
    }
    Ok(None)
}

/// The cable counts as connected even though its reply says [12] = 0; a receiver needs the link.
fn sora_state(r: &[u8], cable: bool) -> State {
    if !(cable || r[12] != 0) {
        return State::Unreachable;
    }
    State::Battery { percent: if r[9] == 0 { 100 } else { r[9] }, charging: r[10] != 0 }
}

/// The Ten reports a real 0% and its own charging flag.
fn ten_state(percent: u8, charging: bool) -> State {
    State::Battery { percent, charging }
}

fn ten_query(dev: &HidDevice, cmd: u8) -> hidapi::HidResult<Option<u8>> {
    let mut req = [0u8; TEN_LEN];
    req[0] = TEN_REPORT;
    req[1] = cmd;
    req[4] = 0x01;
    dev.send_feature_report(&req)?;
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(30));
        let mut buf = [0u8; TEN_LEN];
        buf[0] = TEN_REPORT;
        let n = dev.get_feature_report(&mut buf)?;
        if n == TEN_LEN && buf[0] == TEN_REPORT && buf[1] == cmd {
            return Ok(Some(buf[8]));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sora_reply(percent: u8, charging: u8, online: u8) -> [u8; SORA_LEN] {
        let mut r = [0u8; SORA_LEN];
        r[0] = SORA_REPORT;
        r[1] = SORA_CMD;
        r[9] = percent;
        r[10] = charging;
        r[12] = online;
        r
    }

    #[test]
    fn sora_zero_means_full_and_cable_counts_as_connected() {
        assert_eq!(sora_state(&sora_reply(87, 0, 1), false), State::Battery { percent: 87, charging: false });
        assert_eq!(sora_state(&sora_reply(0, 0, 1), false), State::Battery { percent: 100, charging: false });
        assert_eq!(sora_state(&sora_reply(40, 1, 1), false), State::Battery { percent: 40, charging: true });
        // On the cable the mouse answers [12] = 0 but is charging.
        assert_eq!(sora_state(&sora_reply(40, 1, 0), true), State::Battery { percent: 40, charging: true });
        // Receiver without a link: off or out of range.
        assert_eq!(sora_state(&sora_reply(40, 0, 0), false), State::Unreachable);
    }

    #[test]
    fn ten_keeps_a_real_zero() {
        // The Ten has no "0 means 100%" rule: 0% is 0%.
        assert_eq!(ten_state(0, false), State::Battery { percent: 0, charging: false });
        assert_eq!(ten_state(55, true), State::Battery { percent: 55, charging: true });
        // Charging is a flag, not a level.
        assert_eq!(ten_state(100, true), State::Battery { percent: 100, charging: true });
    }
}
