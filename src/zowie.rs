//! BenQ ZOWIE wireless mice (VID 04A5). ZOWIE ships no software; the vendor commands here were
//! reverse-engineered by the community. Only the read-only battery commands are sent: other
//! EC-CW commands reboot the receiver and unpair the mouse.
//!
//! * EC-CW series (receiver 8006, cable 8005): unnumbered 64-byte output report `00 A5 03`.
//!   The reply is `[mV lo][mV hi][percent]`; mV = 0 means the mouse is not linked. The Enhanced
//!   Receiver declares its collection on usage page 00FF instead of FFxx.
//! * DW series (U2-DW, S2-DW …) and U2: output report 08 `B4 15` on usage page FF03, reply input
//!   report 09 on FF04: mV = ([1] * 256 + [2]) * 10 and [4] must be C8. Receiver 800A (DW), 8008
//!   (U2).
//!
//! On the charging dock the mouse charges over contacts, so over radio only the voltage moves.
//! Charging is inferred from its trend: a rise of 50 mV above the lowest reading means it went
//! on the dock, a drop of 20 mV below the highest means it came off.
//!
//! Protocol and ids from Wireless-Device-Battery-Monitor (drivers/zowie.cpp), which credits
//! yleshchuk/zowie-battery and IgnacioBarb/zowie-battery-tray.

use crate::device::{Kind, Observation, State};
use crate::hidutil::drain as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const VID: u16 = 0x04A5;
const RECEIVER: u16 = 0x8006;
const CABLE: u16 = 0x8005;
const DW_RECEIVER: u16 = 0x800A;
const U2_RECEIVER: u16 = 0x8008;

#[derive(Default)]
pub struct Zowie {
    /// Voltage trend per receiver, for the charging guess.
    trend: HashMap<u16, Trend>,
    /// Last name seen per product id, so a sleeping mouse keeps its model.
    names: HashMap<u16, String>,
}

impl Zowie {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut pids: Vec<u16> = api.device_list().filter(|d| d.vendor_id() == VID).map(|d| d.product_id()).collect();
        pids.sort_unstable();
        pids.dedup();

        let mut out = Vec::new();
        for pid in pids {
            let found = if pid == RECEIVER || pid == CABLE { self.cw(api, pid) } else { self.dw(api, pid) };
            match found {
                Some((name, state)) => {
                    self.names.insert(pid, name.clone());
                    out.push(observation(pid, &name, state));
                }
                None => {
                    if let Some(name) = self.names.get(&pid) {
                        out.push(observation(pid, name, State::Unreachable));
                    }
                }
            }
        }
        out
    }

    fn cw(&self, api: &HidApi, pid: u16) -> Option<(String, State)> {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID
                && d.product_id() == pid
                && (d.usage_page() >= 0xFF00 || d.usage_page() == 0x00FF)
        }) {
            let Some(dev) = api.open_path(info.path()).ok() else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if caps(probe::output_len(&desc, 0)) != 65 {
                continue;
            }
            let Some((mv, level)) = read_cw(&dev) else { continue };
            return Some(("ZOWIE EC-CW".into(), cw_state(pid, mv, level)));
        }
        None
    }

    fn dw(&mut self, api: &HidApi, pid: u16) -> Option<(String, State)> {
        let mut command = None;
        let mut reply = None;
        for info in api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == pid) {
            match info.usage_page() {
                0xFF03 => command = Some((info.path().to_owned(), info.product_string().unwrap_or("").to_string())),
                0xFF04 => reply = Some(info.path().to_owned()),
                _ => {}
            }
        }
        let (command_path, product) = command?;
        let reply_path = reply?;

        // Open the reply channel first, as the vendor tool does.
        let reply = api.open_path(&reply_path).ok()?;
        let command = api.open_path(&command_path).ok()?;
        if probe::descriptor(&reply).and_then(|d| probe::input_len(&d, 0x09)) != Some(16) {
            return None;
        }
        if probe::descriptor(&command).and_then(|d| probe::output_len(&d, 0x08)) != Some(16) {
            return None;
        }
        let mv = read_dw(&command, &reply)?;
        if mv == 0 {
            return Some((dw_name(pid, &product), State::Unreachable));
        }
        let cable = pid != DW_RECEIVER && pid != U2_RECEIVER;
        let charging = cable || self.trend.entry(pid).or_default().update(mv);
        Some((dw_name(pid, &product), State::Battery { percent: percent(mv), charging }))
    }
}

fn observation(pid: u16, name: &str, state: State) -> Observation {
    Observation { id: format!("zowie:{pid:04x}"), name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

fn caps(len: Option<usize>) -> usize {
    len.map_or(0, |l| l + 1)
}

fn cw_state(pid: u16, mv: u16, level: u8) -> State {
    if mv == 0 {
        return State::Unreachable;
    }
    let percent = if (1..=100).contains(&level) { level } else { percent(i32::from(mv)) };
    State::Battery { percent, charging: pid == CABLE }
}

fn dw_name(pid: u16, product: &str) -> String {
    if pid == U2_RECEIVER {
        "ZOWIE U2".into()
    } else if product.trim().is_empty() || product.contains("Receiver") {
        "ZOWIE DW mouse".into()
    } else {
        product.trim().into()
    }
}

fn read_cw(dev: &HidDevice) -> Option<(u16, u8)> {
    flush(dev);
    dev.write(&[0x00, 0xA5, 0x03]).ok()?;

    let deadline = Instant::now() + Duration::from_millis(1000);
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(r) = cw_frame(&buf[..n]) {
                    return Some(r);
                }
            }
            Err(_) => return None,
        }
    }
    None
}

fn cw_frame(r: &[u8]) -> Option<(u16, u8)> {
    if r.len() < 3 {
        return None;
    }
    let mv = u16::from(r[0]) | u16::from(r[1]) << 8;
    // 0 is the "not linked" marker; anything else must look like a cell voltage.
    if mv != 0 && !(3000..=4500).contains(&mv) {
        return None;
    }
    Some((mv, r[2]))
}

enum Dw {
    Frame(i32),
    Reject,
    Other,
}

fn dw_frame(r: &[u8]) -> Dw {
    if r.len() < 5 || r[0] != 0x09 {
        return Dw::Other;
    }
    if r[1] == 0xE6 || r[4] != 0xC8 {
        return Dw::Reject; // no battery, or not a battery reply
    }
    Dw::Frame((i32::from(r[1]) * 256 + i32::from(r[2])) * 10)
}

fn read_dw(command: &HidDevice, reply: &HidDevice) -> Option<i32> {
    flush(reply);
    command.write(&[0x08, 0xB4, 0x15]).ok()?;

    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match reply.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(0) => break,
            Ok(n) => match dw_frame(&buf[..n]) {
                Dw::Frame(mv) => return Some(mv),
                Dw::Reject => return None,
                Dw::Other => {}
            },
            Err(_) => return None,
        }
    }
    None
}

/// Voltage to percent, anchored on the mouse's own LED thresholds (10 / 25 / 50 %).
fn percent(mv: i32) -> u8 {
    const MV: [i32; 6] = [4200, 4000, 3890, 3760, 3680, 3500];
    const PCT: [i32; 6] = [100, 75, 50, 25, 10, 0];
    if mv >= MV[0] {
        return PCT[0] as u8;
    }
    for i in 1..MV.len() {
        if mv >= MV[i] {
            let span = MV[i - 1] - MV[i];
            let p = PCT[i] + (PCT[i - 1] - PCT[i]) * (mv - MV[i]) / if span != 0 { span } else { 1 };
            return p.clamp(0, 100) as u8;
        }
    }
    PCT[PCT.len() - 1] as u8
}

/// Whether the voltage looks like it is rising on a charging dock. One per receiver.
#[derive(Default)]
struct Trend {
    has: bool,
    charging: bool,
    peak: i32,
    trough: i32,
}

impl Trend {
    fn update(&mut self, mv: i32) -> bool {
        if !self.has {
            self.has = true;
            // A resting Li-ion cell never sits above ~4.2 V.
            self.charging = mv >= 4250;
            self.peak = mv;
            self.trough = mv;
        } else if self.charging {
            self.peak = self.peak.max(mv);
            if mv <= self.peak - 20 {
                self.charging = false;
                self.trough = mv;
            }
        } else {
            self.trough = self.trough.min(mv);
            if mv >= self.trough + 50 {
                self.charging = true;
                self.peak = mv;
            }
        }
        self.charging
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voltage_curve_hits_the_led_thresholds() {
        assert_eq!(percent(4300), 100);
        assert_eq!(percent(4200), 100);
        assert_eq!(percent(4000), 75);
        assert_eq!(percent(3890), 50);
        assert_eq!(percent(3760), 25);
        assert_eq!(percent(3680), 10);
        assert_eq!(percent(3500), 0);
        assert_eq!(percent(3200), 0);
        assert_eq!(percent(3945), 62); // halfway between 75 % and 50 %
    }

    #[test]
    fn trend_follows_a_dock_session() {
        let mut t = Trend::default();
        assert!(!t.update(4200)); // balancing, on battery
        assert!(t.update(4250)); // +50 mV: went on the dock
        assert!(t.update(4240)); // small wobble, still charging
        assert!(!t.update(4229)); // -20 mV from the peak: came off
        assert!(t.update(4279)); // +50 mV from the trough: back on
    }

    #[test]
    fn cw_parses_voltage_and_rejects_noise() {
        // 3900 mV, firmware says 51 %.
        assert_eq!(cw_frame(&[0x3C, 0x0F, 51]), Some((3900, 51)));
        // Not linked.
        assert_eq!(cw_frame(&[0x00, 0x00, 0x00]), Some((0, 0)));
        // 0 mV with a level byte still means not linked; 1 V is not a cell.
        assert_eq!(cw_state(0x8006, 0, 50), State::Unreachable);
        assert_eq!(cw_frame(&[0xE8, 0x03, 50]), None);
        assert_eq!(cw_frame(&[0x3C]), None);
    }

    #[test]
    fn dw_parses_millivolts_and_checks_the_marker() {
        // mV = (0x01 * 256 + 0x86) * 10 = 3900.
        assert!(matches!(dw_frame(&[0x09, 0x01, 0x86, 0x00, 0xC8]), Dw::Frame(3900)));
        assert!(matches!(dw_frame(&[0x09, 0xE6, 0x3C, 0x00, 0xC8]), Dw::Reject));
        assert!(matches!(dw_frame(&[0x09, 0x0F, 0x3C, 0x00, 0xC7]), Dw::Reject));
        assert!(matches!(dw_frame(&[0x08, 0x0F, 0x3C, 0x00, 0xC8]), Dw::Other));
    }

    #[test]
    fn cables_share_the_cw_name_and_charge() {
        assert_eq!(cw_state(0x8005, 3900, 0), State::Battery { percent: percent(3900), charging: true });
    }
}
