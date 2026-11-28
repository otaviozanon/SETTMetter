//! MSI wireless mice on their 2.4 GHz dongles and cables.
//!
//! Query models (VERSA 300 ELITE, CLUTCH GM31) answer output report 0 `00 0F 20`
//! zero-padded: the reply starts `0F 20`, and byte 14 holds bit 7 charging plus bits 0-6 the
//! percent. MSI Center's "remote control" (41 80) is deliberately not sent: the 0F 20 query is
//! enough. A level of 0 while not charging is a sleeping mouse, not an empty battery.
//!
//! The CLUTCH GM41 cannot be asked. Its collection only pushes `A0 <raw> 07 <flags> …` while
//! the mouse moves, and <raw> falls as the battery drains; the percent is a linear fit through
//! (0x4D, 57 %), (0x48, 54 %) and (0x45, 53 %). The handle stays open between scans and queued
//! reports are drained without waiting.
//!
//! Protocol and ids from Wireless-Device-Battery-Monitor (drivers/msi.cpp), whose notes credit
//! DARL0K and liamgibsonn for the GM41 captures. Only read-only queries are sent.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::drain as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::time::{Duration, Instant};

const VID: u16 = 0x0DB0;
const WAIT: Duration = Duration::from_millis(1000);

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Query,
    Passive,
}

struct Model {
    pid: u16,
    name: &'static str,
    mode: Mode,
    page: u16,
    usage: u16,
}

const MODELS: &[Model] = &[
    // Dongles and cables first, then the passive GM41.
    Model { pid: 0x9E22, name: "MSI VERSA 300 ELITE WIRELESS", mode: Mode::Query, page: 0xFF0D, usage: 0x00B0 },
    Model { pid: 0x0D3C, name: "MSI Clutch GM31 Lightweight Wireless", mode: Mode::Query, page: 0xFF00, usage: 0x0001 },
    Model { pid: 0x0D3B, name: "MSI Clutch GM31 Lightweight Wireless", mode: Mode::Query, page: 0xFF00, usage: 0x0001 },
    Model { pid: 0x0D4C, name: "MSI Clutch GM41 Lightweight Wireless", mode: Mode::Passive, page: 0xFF10, usage: 0x0006 },
    Model { pid: 0x0D4B, name: "MSI Clutch GM41 Lightweight Wireless", mode: Mode::Passive, page: 0xFF10, usage: 0x0006 },
];

#[derive(Default)]
pub struct Msi {
    /// The GM41 has no query, so its collection stays open and is drained at each scan.
    passive: HashMap<CString, HidDevice>,
    seen: HashSet<String>,
}

impl Msi {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut passive_paths: Vec<CString> = Vec::new();
        for model in MODELS.iter().filter(|m| m.mode == Mode::Passive) {
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == model.pid
                    && d.interface_number() == 1
                    && d.usage_page() == model.page
                    && d.usage() == model.usage
            }) {
                passive_paths.push(info.path().to_owned());
            }
        }
        self.passive.retain(|path, _| passive_paths.contains(path));

        let mut out = Vec::new();
        for model in MODELS {
            let key = format!("msi:{:04x}", model.pid);
            let paths: Vec<CString> = api
                .device_list()
                .filter(|d| {
                    d.vendor_id() == VID
                        && d.product_id() == model.pid
                        && d.interface_number() == 1
                        && d.usage_page() == model.page
                        && d.usage() == model.usage
                })
                .map(|d| d.path().to_owned())
                .collect();

            let mut reading = None;
            for path in &paths {
                match model.mode {
                    Mode::Query => {
                        let Some(dev) = api.open_path(path).ok() else { continue };
                        let Some(desc) = probe::descriptor(&dev) else { continue };
                        // Both directions carry a 64-byte payload plus the report id.
                        if caps(probe::output_len(&desc, 0)) < 65 || caps(probe::input_len(&desc, 0)) < 65 {
                            continue;
                        }
                        match query(&dev) {
                            Ok(Some(r)) => {
                                reading = Some(r);
                                break;
                            }
                            Ok(None) => {}
                            Err(e) => diag::event(&key, format!("i/o failed: {e}")),
                        }
                    }
                    Mode::Passive => {
                        if !self.passive.contains_key(path) {
                            match api.open_path(path) {
                                Ok(dev) => {
                                    self.passive.insert(path.clone(), dev);
                                }
                                Err(e) => {
                                    diag::event(&key, format!("open failed: {e}"));
                                    continue;
                                }
                            }
                        }
                        let dev = self.passive.get(path).unwrap();
                        if let Some(raw) = listen(dev) {
                            diag::event(&key, format!("battery via pushed report (raw 0x{raw:02X})"));
                            reading = Some((gm41_percent(raw), false));
                            break;
                        }
                    }
                }
            }

            if let Some((percent, charging)) = reading {
                self.seen.insert(key.clone());
                let state = if percent == 0 && !charging {
                    // A receiver answers 0 while the mouse is off or asleep; keep the last value.
                    State::Unreachable
                } else {
                    State::Battery { percent, charging }
                };
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

/// Windows' report length: hidapi hides the report-id byte of an unnumbered report.
fn caps(len: Option<usize>) -> usize {
    len.map_or(0, |l| l + 1)
}

fn query(dev: &HidDevice) -> hidapi::HidResult<Option<(u8, bool)>> {
    flush(dev);
    dev.write(&[0x00, 0x0F, 0x20])?;

    let deadline = Instant::now() + WAIT;
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(r) = parse(&buf[..n]) {
                    return Ok(Some(r));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// `[0] = 0F`, `[1] = 20`, then the level in `[14]`: bit 7 charging, bits 0-6 percent.
fn parse(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 15 || r[0] != 0x0F || r[1] != 0x20 {
        return None;
    }
    let level = r[14] & 0x7F;
    if level > 100 {
        return None;
    }
    Some((level, r[14] & 0x80 != 0))
}

fn listen(dev: &HidDevice) -> Option<u8> {
    let mut buf = [0u8; 64];
    let mut found = None;
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
        if let Some(raw) = gm41(&buf[..n]) {
            found = Some(raw);
        }
    }
    found
}

/// The GM41's pushed frame: `A0 <raw> 07 <flags> 94 00 94`.
fn gm41(r: &[u8]) -> Option<u8> {
    if r.len() < 7 || r[0] != 0xA0 || r[2] != 0x07 {
        return None;
    }
    Some(r[1])
}

/// Linear fit through (0x4D, 57 %), (0x48, 54 %) and (0x45, 53 %): about 0.51 % per step.
fn gm41_percent(raw: u8) -> u8 {
    ((i32::from(raw) * 51 + 1760 + 50) / 100).clamp(0, 100) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_gm31_capture() {
        // Pushed frame captured from the GM31 next to MSI Center: 0x56 = 86 %.
        let frame = [0x0F, 0x20, 0x00, 0x00, 0x23, 0x05, 0x7A, 0x91, 0, 0, 0, 0, 0x05, 0x80, 0x56, 0x00];
        assert_eq!(parse(&frame), Some((86, false)));

        let mut charging = frame;
        charging[14] = 0x80 | 42;
        assert_eq!(parse(&charging), Some((42, true)));

        charging[14] = 0x7F; // 127 is not a level
        assert_eq!(parse(&charging), None);
        assert_eq!(parse(&frame[..14]), None); // short
    }

    #[test]
    fn gm41_curve_matches_its_calibration_points() {
        assert_eq!(gm41_percent(0x4D), 57);
        assert_eq!(gm41_percent(0x48), 54);
        assert_eq!(gm41_percent(0x45), 53);
        assert_eq!(gm41_percent(0), 18);
        assert_eq!(gm41_percent(0xFF), 100); // clamped
    }

    #[test]
    fn gm41_frame_needs_its_markers() {
        assert_eq!(gm41(&[0xA0, 0x45, 0x07, 0x00, 0x94, 0x00, 0x94]), Some(0x45));
        assert_eq!(gm41(&[0xA0, 0x45, 0x08, 0x00, 0x94, 0x00, 0x94]), None);
        assert_eq!(gm41(&[0xA0, 0x45, 0x07]), None);
    }
}
