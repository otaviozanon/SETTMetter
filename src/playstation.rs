//! Sony DualSense / DualSense Edge / DualShock 4, over USB or Bluetooth.
//!
//! Battery comes from the controller's regular input report, which it streams constantly;
//! we only read, never write to the controller. Byte offsets (including the report id):
//!
//! | Controller | Connection | Report id | Status byte |
//! |---|---|---|---|
//! | DualSense  | USB       | 0x01 (64 bytes) | 53 |
//! | DualSense  | Bluetooth | 0x31 (78 bytes) | 54 |
//! | DualShock 4| USB       | 0x01 (64 bytes) | 30 |
//! | DualShock 4| Bluetooth | 0x11 (78 bytes) | 32 |
//!
//! Over Bluetooth, controllers only send the full reports once a game or Steam has enabled
//! them; until then the battery isn't available and the controller isn't shown.

use crate::device::{Kind, Observation, State};
use crate::diag;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const SONY: u16 = 0x054C;
const DUALSHOCK4: &[u16] = &[0x05C4, 0x09CC, 0x0BA0];
const DUALSENSE: u16 = 0x0CE6;
const DUALSENSE_EDGE: u16 = 0x0DF2;
const READ_WINDOW: Duration = Duration::from_millis(300);

pub fn scan(api: &HidApi) -> Vec<Observation> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for d in api.device_list() {
        let pid = d.product_id();
        let dualsense = pid == DUALSENSE || pid == DUALSENSE_EDGE;
        if d.vendor_id() != SONY || !(dualsense || DUALSHOCK4.contains(&pid)) {
            continue;
        }
        // The gamepad collection (Generic Desktop / Game Pad) carries the input reports.
        if d.usage_page() != 0x01 || d.usage() != 0x05 || !seen.insert(d.path().to_owned()) {
            continue;
        }
        // Not the serial: it identifies the controller.
        let key = format!("ps:{pid:04x}:{}", seen.len());
        let dev = match api.open_path(d.path()) {
            Ok(dev) => dev,
            Err(e) => {
                diag::event(&key, format!("open failed: {e}"));
                continue;
            }
        };
        let (percent, charging) = match read_status(&dev, dualsense) {
            Ok((percent, charging, report)) => {
                diag::event(&key, format!("battery via input report 0x{report:02X}"));
                (percent, charging)
            }
            Err(why) => {
                diag::event(&key, why);
                continue;
            }
        };

        let name = match pid {
            DUALSENSE_EDGE => "DualSense Edge",
            DUALSENSE => "DualSense",
            _ => "DualShock 4",
        };
        let serial = d.serial_number().unwrap_or_default();
        let key = if serial.is_empty() { d.path().to_string_lossy().into_owned() } else { serial.to_string() };
        out.push(Observation {
            id: format!("ps:{pid:04x}:{key}"),
            name: Some(name.to_string()),
            kind: Some(Kind::Gamepad),
            state: State::Battery { percent, charging },
        });
    }
    out
}

/// (percent, charging, report id), or why there's no reading (for diagnostics).
fn read_status(dev: &HidDevice, dualsense: bool) -> Result<(u8, bool, u8), String> {
    let deadline = Instant::now() + READ_WINDOW;
    let mut buf = [0u8; 128];
    let mut last = None;
    while Instant::now() < deadline {
        let n = dev.read_timeout(&mut buf, 50).map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            continue;
        }
        last = Some((buf[0], n));
        let status = match (dualsense, buf[0]) {
            (true, 0x01) if n >= 64 => buf[53],
            (true, 0x31) if n >= 78 => buf[54],
            (false, 0x01) if n >= 64 => buf[30],
            (false, 0x11) if n >= 78 => buf[32],
            _ => continue, // short "basic" reports carry no battery
        };
        let (percent, charging) = if dualsense { dualsense_status(status) } else { dualshock4_status(status) };
        return Ok((percent, charging, buf[0]));
    }
    Err(match last {
        Some((id, n)) => format!("only basic input reports (last: 0x{id:02X}, {n} bytes); over Bluetooth a game or Steam must enable the full one"),
        None => "no input reports".into(),
    })
}

/// Low nibble: level 0..10. High nibble: 0 discharging, 1 charging, 2 full.
fn dualsense_status(s: u8) -> (u8, bool) {
    let percent = ((s & 0x0F) * 10 + 5).min(100);
    match s >> 4 {
        0x1 => (percent, true),
        0x2 => (100, false),
        _ => (percent, false),
    }
}

/// Low nibble: level (0..10 on battery, 0..11 on cable). Bit 4: cable connected.
fn dualshock4_status(s: u8) -> (u8, bool) {
    let level = s & 0x0F;
    if s & 0x10 != 0 {
        if level < 10 {
            (level * 10 + 5, true)
        } else {
            (100, false) // full
        }
    } else {
        ((level * 10 + 5).min(100), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dualsense() {
        assert_eq!(dualsense_status(0x08), (85, false)); // discharging, level 8
        assert_eq!(dualsense_status(0x13), (35, true)); // charging, level 3
        assert_eq!(dualsense_status(0x2A), (100, false)); // full
        assert_eq!(dualsense_status(0x0A), (100, false)); // level 10 caps at 100
    }

    #[test]
    fn dualshock4() {
        assert_eq!(dualshock4_status(0x05), (55, false)); // on battery
        assert_eq!(dualshock4_status(0x14), (45, true)); // cable, charging
        assert_eq!(dualshock4_status(0x1B), (100, false)); // cable, full
    }
}
