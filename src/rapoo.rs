//! Rapoo VT gaming mice: passive status reports on the 24AE receiver and cable.
//!
//! The battery is pushed; no command or handshake is sent. Two report layouts:
//! - Nordic 54L, input report 0x07 on the FF00:0002 collection: connection in the low nibble of
//!   byte 1 (0 RF, 1 BT, 2 USB), state at byte 7, percent at byte 8, and the mouse pid at 13..14
//!   when the report is long enough.
//! - VT3 Pro / VT1 Pro Max firmware, input report 0xBB: `BB B0 mode dpi-lo dpi-hi state percent`,
//!   state at byte 5, percent at byte 6. Captured on a VT1 Pro Max receiver (1233) as
//!   `BB B0 71 84 03 01 64` (100%) and on its cable while charging as `BB B0 71 84 03 02 5F` (95%).
//! Only pids confirmed to send the 0xBB report use that layout.
//!
//! Handles stay open between scans (like `Delux`), so the dongle's pushes queue in Windows instead
//! of only being seen by a scan that happens to be listening. A scan drains what queued and then
//! waits up to a second for the next one.
//!
//! Protocol from Wireless-Device-Battery-Monitor (drivers/rapoo.cpp, rapoo_protocol.h), the Rapoo
//! Hub VT_nrf54L decoder, Device-Battery-Info and the usbpcap captures in issues #136 / #2258.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::time::{Duration, Instant};

const VID: u16 = 0x24AE;
const PAGE: u16 = 0xFF00;
const USAGE: u16 = 0x0002;
const NORDIC_REPORT: u8 = 0x07;
const LEGACY_REPORT: u8 = 0xBB;
/// How long a scan waits for a passive report when nothing was queued.
const WAIT: Duration = Duration::from_millis(1000);

struct Model {
    cable: u16,
    receiver: u16,
    name: &'static str,
}

/// Selected names from Rapoo's manifests. Support comes from the collection and a valid frame, so
/// other models on the same protocol work without extending this table.
const MODELS: &[Model] = &[
    Model { cable: 0x4610, receiver: 0x1410, name: "Rapoo VT0" },
    Model { cable: 0x4620, receiver: 0x1420, name: "Rapoo VT0 Max" },
    Model { cable: 0x4615, receiver: 0x1415, name: "Rapoo VT1" },
    Model { cable: 0x4644, receiver: 0x1444, name: "Rapoo VT1 Max" },
    Model { cable: 0x4622, receiver: 0x1422, name: "Rapoo VT2" },
    Model { cable: 0x4623, receiver: 0x1423, name: "Rapoo VT2 Max" },
    Model { cable: 0x4612, receiver: 0x1412, name: "Rapoo VT3" },
    Model { cable: 0x4617, receiver: 0x1417, name: "Rapoo VT3 Max (Gen 2)" },
    Model { cable: 0x4B06, receiver: 0x1B06, name: "Rapoo VT3 Max V2" },
    Model { cable: 0x4611, receiver: 0x1411, name: "Rapoo VT3S" },
    Model { cable: 0x4621, receiver: 0x1421, name: "Rapoo VT3S Max" },
    Model { cable: 0x4613, receiver: 0x1413, name: "Rapoo VT7" },
    Model { cable: 0x4616, receiver: 0x1416, name: "Rapoo VT7 Max" },
    Model { cable: 0x4614, receiver: 0x1414, name: "Rapoo VT9 (Gen 2)" },
    Model { cable: 0x4618, receiver: 0x1418, name: "Rapoo VT9 Max" },
    Model { cable: 0x4415, receiver: 0x1215, name: "Rapoo VT3 Pro" },
    Model { cable: 0x4433, receiver: 0x1233, name: "Rapoo VT1 Pro Max" },
];

/// Receivers/pids confirmed to send the legacy 7-byte 0xBB status report.
const LEGACY_PIDS: &[u16] = &[0x1215, 0x4415, 0x1233, 0x4433, 0x1244];

fn find_model(pid: u16) -> Option<&'static Model> {
    MODELS.iter().find(|m| pid == m.cable || pid == m.receiver)
}

fn legacy_pid(pid: u16) -> bool {
    LEGACY_PIDS.contains(&pid)
}

#[derive(Clone, Copy, PartialEq)]
enum Profile {
    Nordic54L,
    Legacy,
}

struct Reader {
    dev: HidDevice,
    profile: Profile,
    /// HID product string, for models the table does not name.
    product: String,
}

/// One decoded status report. `percent` is None only for the legacy "7F while charging" case.
#[derive(Clone, Copy)]
struct Battery {
    percent: Option<u8>,
    charging: bool,
    mouse_pid: u16,
}

#[derive(Default)]
pub struct Rapoo {
    /// Handles kept open between scans, by collection path.
    open: HashMap<CString, Reader>,
    /// Keys already reported, so a mouse that pushes nothing is Unreachable rather than removed.
    seen: HashSet<String>,
}

impl Rapoo {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        // Collection, product id and product name in device-list order.
        let mut cols: Vec<(u16, CString, String)> = Vec::new();
        for d in api
            .device_list()
            .filter(|d| d.vendor_id() == VID && d.usage_page() == PAGE && d.usage() == USAGE)
        {
            cols.push((d.product_id(), d.path().to_owned(), d.product_string().unwrap_or("").to_string()));
        }
        self.open.retain(|path, _| cols.iter().any(|(_, p, _)| p == path));

        let mut named: HashMap<String, (bool, Observation)> = HashMap::new();
        let mut claimed: HashSet<u16> = HashSet::new();
        for (pid, path, product) in &cols {
            if claimed.contains(pid) {
                continue;
            }
            if !self.open.contains_key(path) {
                let Ok(dev) = api.open_path(path) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                let Some(profile) = profile(&desc, *pid) else { continue };
                self.open.insert(path.clone(), Reader { dev, profile, product: product.clone() });
            }
            let Some(reader) = self.open.get_mut(path) else { continue };
            match read_battery(reader) {
                Ok(Some(battery)) => {
                    claimed.insert(*pid);
                    let model = find_model(battery.mouse_pid).or_else(|| find_model(*pid));
                    let identity = model.map_or(if battery.mouse_pid != 0 { battery.mouse_pid } else { *pid }, |m| m.cable);
                    let key = format!("rapoo:{identity:04x}");
                    let mut name = match (*pid == 0x1244, model) {
                        (true, _) => "Rapoo VT3 Pro Max".to_string(),
                        (_, Some(model)) => model.name.to_string(),
                        (_, None) => reader.product.clone(),
                    };
                    if name.is_empty() || name == "Rapoo Gaming Device" {
                        name = "Rapoo wireless mouse".to_string();
                    }
                    let state = match battery.percent {
                        Some(percent) => State::Battery { percent, charging: battery.charging },
                        None => State::Unreachable,
                    };
                    diag::event(&key, format!("battery via report 0x{:02x}", if reader.profile == Profile::Legacy { LEGACY_REPORT } else { NORDIC_REPORT }));
                    self.seen.insert(key.clone());
                    let is_cable = model.is_some_and(|m| *pid == m.cable);
                    let mut observation = Observation { id: key.clone(), name: Some(name), kind: Some(Kind::Mouse), state };
                    match named.entry(key) {
                        std::collections::hash_map::Entry::Vacant(slot) => {
                            slot.insert((is_cable, observation));
                        }
                        // A charging mouse can report through both cable and receiver: one entry,
                        // preferring the cable's state but keeping any available percentage.
                        std::collections::hash_map::Entry::Occupied(mut slot) => {
                            let (cable, existing) = slot.get_mut();
                            if is_cable && !*cable {
                                if matches!(observation.state, State::Unreachable) && matches!(existing.state, State::Battery { .. }) {
                                    observation.state = existing.state;
                                }
                                *existing = observation;
                                *cable = true;
                            } else if matches!(existing.state, State::Unreachable) && matches!(observation.state, State::Battery { .. }) {
                                existing.state = observation.state;
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    diag::event("rapoo", format!("read failed: {e}"));
                    self.open.remove(path);
                }
            }
        }

        let mut out: Vec<Observation> = named.values().map(|(_, o)| o.clone()).collect();
        for key in &self.seen {
            if !named.contains_key(key) {
                out.push(Observation {
                    id: key.clone(),
                    name: None,
                    kind: Some(Kind::Mouse),
                    state: State::Unreachable,
                });
            }
        }
        out
    }
}

/// Which report layout this collection carries. The lengths are the ones upstream accepts,
/// including the 13-byte VT3 Max Gen 2 revision and the 7-byte legacy collections.
fn profile(desc: &[u8], pid: u16) -> Option<Profile> {
    let legacy = probe::input_len(desc, LEGACY_REPORT);
    if legacy_pid(pid) && legacy == Some(7) {
        return Some(Profile::Legacy);
    }
    match probe::input_len(desc, NORDIC_REPORT) {
        Some(16 | 19) => Some(Profile::Nordic54L),
        Some(13) if pid == 0x1417 => Some(Profile::Nordic54L),
        _ => None,
    }
}

/// Drains what queued since the last scan without waiting, then waits up to [`WAIT`] for one more.
fn read_battery(reader: &mut Reader) -> hidapi::HidResult<Option<Battery>> {
    let mut buf = [0u8; 64];
    for _ in 0..64 {
        match reader.dev.read_timeout(&mut buf, 0) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(battery) = decode(&buf[..n], reader.profile) {
                    return Ok(Some(battery));
                }
            }
            Err(e) => return Err(e),
        }
    }
    let deadline = Instant::now() + WAIT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match reader.dev.read_timeout(&mut buf, left.as_millis().clamp(1, 250) as i32) {
            Ok(0) => {}
            Ok(n) => {
                if let Some(battery) = decode(&buf[..n], reader.profile) {
                    return Ok(Some(battery));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// Decodes one passive status report, which still carries its report id at [0].
fn decode(r: &[u8], profile: Profile) -> Option<Battery> {
    let mut out = Battery { percent: None, charging: false, mouse_pid: 0 };
    let (state, percent) = match profile {
        Profile::Nordic54L => {
            if r.len() < 9 || r[0] != NORDIC_REPORT || r[1] & 0x0F > 2 {
                return None;
            }
            if r.len() >= 15 {
                out.mouse_pid = u16::from_le_bytes([r[13], r[14]]);
            }
            (r[7], r[8])
        }
        Profile::Legacy => {
            if r.len() < 7 || r[0] != LEGACY_REPORT || r[1] != 0xB0 {
                return None;
            }
            (r[5], r[6])
        }
    };
    // 0 = invalid/transition. Some older firmware uses 7F instead of a level while charging.
    if state != 1 && state != 2 {
        return None;
    }
    if percent > 100 && !(profile == Profile::Legacy && state == 2 && percent == 0x7F) {
        return None;
    }
    out.charging = state == 2;
    if percent <= 100 {
        out.percent = Some(percent);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_legacy_reports() {
        // VT1 Pro Max receiver 1233: 100%, on battery.
        let battery = decode(&[0xBB, 0xB0, 0x71, 0x84, 0x03, 0x01, 0x64], Profile::Legacy).unwrap();
        assert_eq!((battery.percent, battery.charging), (Some(100), false));
        // Same mouse on its cable while charging: 95%.
        let battery = decode(&[0xBB, 0xB0, 0x71, 0x84, 0x03, 0x02, 0x5F], Profile::Legacy).unwrap();
        assert_eq!((battery.percent, battery.charging), (Some(95), true));
    }

    #[test]
    fn legacy_7f_while_charging_is_a_charge_state_without_a_level() {
        let battery = decode(&[0xBB, 0xB0, 0x71, 0x84, 0x03, 0x02, 0x7F], Profile::Legacy).unwrap();
        assert_eq!((battery.percent, battery.charging), (None, true));
        // 7F while discharging is not a level.
        assert!(decode(&[0xBB, 0xB0, 0x71, 0x84, 0x03, 0x01, 0x7F], Profile::Legacy).is_none());
    }

    #[test]
    fn nordic_layout_and_optional_mouse_pid() {
        let mut frame = [0u8; 19];
        frame[0] = 0x07;
        frame[1] = 0x00; // RF receiver
        frame[7] = 0x01;
        frame[8] = 64;
        frame[13..15].copy_from_slice(&0x4433u16.to_le_bytes());
        let battery = decode(&frame, Profile::Nordic54L).unwrap();
        assert_eq!((battery.percent, battery.charging, battery.mouse_pid), (Some(64), false, 0x4433));

        // A 13-byte VT3 Max Gen 2 status still carries state and level, but no mouse pid.
        let mut short = [0u8; 13];
        short[0] = 0x07;
        short[1] = 0x02; // USB cable
        short[7] = 0x02;
        short[8] = 55;
        let battery = decode(&short, Profile::Nordic54L).unwrap();
        assert_eq!((battery.percent, battery.charging, battery.mouse_pid), (Some(55), true, 0));

        // State 0 is a transition, and 101 is not a level.
        let mut frame = [0u8; 19];
        frame[0] = 0x07;
        frame[7] = 0;
        frame[8] = 50;
        assert!(decode(&frame, Profile::Nordic54L).is_none());
        frame[7] = 1;
        frame[8] = 101;
        assert!(decode(&frame, Profile::Nordic54L).is_none());
    }

    #[test]
    fn wrong_report_or_connection_byte_is_rejected() {
        assert!(decode(&[0xBB, 0xB1, 0x71, 0x84, 0x03, 0x01, 0x64], Profile::Legacy).is_none());
        assert!(decode(&[0x07, 0x03, 0, 0, 0, 0, 0, 1, 50], Profile::Nordic54L).is_none());
        assert!(decode(&[0xBB], Profile::Legacy).is_none());
        assert!(decode(&[0x07, 0x00, 0, 0, 0, 0, 0, 1], Profile::Nordic54L).is_none());
    }
}
