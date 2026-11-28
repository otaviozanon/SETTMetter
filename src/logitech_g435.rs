//! Logitech G435 LIGHTSPEED headset on the 046D:0ADF dongle.
//!
//! Unlike the HID++ dongles, this one exposes a vendor collection (FF00:0001, unnumbered
//! 64-byte reports) carrying CRC-16 framed messages; "PA" commands inside reach the headset's
//! radio SoC. The battery is read with the OTA GET_BATTERY command, in millivolts.
//!
//! Session from the capture: open (`84 01`), `85 01`, session init (OTA IOCTL_READ_MAX_LEN),
//! REMOTE_GET_MODE / REMOTE_SET_MODE 01 / REMOTE_GET_MODE so the query is forwarded to the
//! headset rather than answered by the dongle, GET_BATTERY, close (`85 02`). SET_MODE 01 only
//! selects where the dongle routes the command; it is not a setting and does not persist.
//! The fixed waits add up to ~1.2 s; a read does not interrupt audio or the microphone.
//!
//! Protocol: github.com/portyporty/g435-battery-percentage (MIT), also used by
//! github.com/deddroid/logitech-g435-battery. Not verified in this project.
//!
//! The older 046D:0ACB dongle uses a different transport and is not handled. The 4C4A:4155
//! Jieli receiver exposes no battery level at all (the upstream driver estimates it from a
//! Bluetooth Hands-Free reading plus a learned drain rate, using SetupAPI, CoreAudio and the
//! registry), so that variant is deliberately not ported.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x046D;
const PID: u16 = 0x0ADF;
const KEY: &str = "logi:0adf";
const NAME: &str = "Logitech G435";
const CAT_OTA: u8 = 0x06;
const CAT_WIRELESS: u8 = 0x0E;
const GET_MODE: u8 = 0xE0;
const SET_MODE: u8 = 0xE1;
const FRAME: usize = 64;

/// OTA_CMD_IOCTL_READ_MAX_LEN, sequence 1 embedded.
const SESSION_INIT: [u8; 19] = [
    0x50, 0x41, 0x06, 0x01, 0x0D, 0x00, 0x25, 0x34, 0x12, 0x5A, 0x5A, 0x01, 0x00, 0x00, 0x00, 0xF0, 0x00, 0x00, 0x00,
];

#[derive(Default)]
pub struct LogitechG435 {}

impl LogitechG435 {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let found = api.device_list().find(|d| {
            d.vendor_id() == VID
                && d.product_id() == PID
                && d.usage_page() == 0xFF00
                && d.usage() == 0x0001
        });
        let Some(info) = found else { return Vec::new() };
        let Ok(dev) = api.open_path(info.path()) else {
            diag::event(KEY, "open failed");
            return Vec::new();
        };
        let Some(desc) = probe::descriptor(&dev) else { return Vec::new() };
        if probe::input_len(&desc, 0).unwrap_or(0) < FRAME || probe::output_len(&desc, 0).unwrap_or(0) < FRAME {
            return Vec::new();
        }

        let mv = read_millivolts(&dev);
        let state = match mv {
            Some(mv) => {
                let percent = percent(mv);
                diag::event(KEY, format!("battery via OTA GET_BATTERY: {mv} mV -> {percent}%"));
                State::Battery { percent, charging: false }
            }
            None => {
                // Dongle present but no reply: the headset is off or out of range.
                diag::event(KEY, "no battery reply (headset off or out of range)");
                State::Off
            }
        };
        vec![Observation { id: KEY.into(), name: Some(NAME.into()), kind: Some(Kind::Headset), state }]
    }
}

/// Runs the capture's session and always closes it.
fn read_millivolts(dev: &HidDevice) -> Option<u16> {
    if !transact(dev, 0x84, &[0x01], 50, None) {
        return None;
    }
    if !transact(dev, 0x85, &[0x01], 50, None) {
        let _ = transact(dev, 0x85, &[0x02], 50, None);
        return None;
    }
    if !transact(dev, 0x80, &SESSION_INIT, 450, None) {
        let _ = transact(dev, 0x85, &[0x02], 50, None);
        return None;
    }

    let mut seq = 2u8;
    let mut pa = |cat: u8, len: u8, payload: &[u8]| {
        let mut cmd = vec![0x50, 0x41, cat, seq, len];
        cmd.extend_from_slice(payload);
        seq = seq % 127 + 1;
        cmd
    };
    transact(dev, 0x80, &pa(CAT_WIRELESS, 1, &[GET_MODE]), 120, None);
    transact(dev, 0x80, &pa(CAT_WIRELESS, 2, &[SET_MODE, 0x01]), 120, None);
    transact(dev, 0x80, &pa(CAT_WIRELESS, 1, &[GET_MODE]), 120, None);
    let mut mv = None;
    transact(dev, 0x80, &pa(CAT_OTA, 1, &[0x00, 0x31]), 250, Some(&mut mv));
    // Close the session whatever happened above.
    let _ = transact(dev, 0x85, &[0x02], 50, None);
    mv
}

/// Sends one frame (report id 0) and collects the input reports that arrive within `wait_ms`.
/// With `mv` set, returns as soon as a battery reply is seen.
fn transact(dev: &HidDevice, cmd: u8, body: &[u8], wait_ms: u64, mut mv: Option<&mut Option<u16>>) -> bool {
    let frame = frame(cmd, body);
    let mut out = vec![0u8; FRAME + 1];
    out[1..].copy_from_slice(&frame);
    drain(dev);
    if dev.write(&out).is_err() {
        return false;
    }
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    let mut buf = [0u8; 1024];
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now()).as_millis().max(1) as i32;
        let n = match dev.read_timeout(&mut buf, left.min(250)) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(_) => continue,
        };
        if let Some(slot) = mv.as_mut()
            && let Some(found) = parse_millivolts(&buf[..n])
        {
            **slot = Some(found);
            return true;
        }
    }
    true
}

/// 64-byte frame: `<cmd> <body length> <crc lo> <crc hi> <body...>`, zero-padded. The CRC covers
/// the whole frame with the CRC field zeroed.
fn frame(cmd: u8, body: &[u8]) -> [u8; FRAME] {
    let mut f = [0u8; FRAME];
    f[0] = cmd;
    f[1] = body.len() as u8;
    let n = body.len().min(FRAME - 4);
    f[4..4 + n].copy_from_slice(&body[..n]);
    let crc = crc16(&f);
    f[2] = (crc & 0xFF) as u8;
    f[3] = (crc >> 8) as u8;
    f
}

use crate::hidutil::crc16_xmodem as crc16;

/// GET_BATTERY reply pattern `00 07 00 06 <seq> 00 <mV lo> <mV hi>`; absent or implausible
/// Li-Po voltages are rejected.
fn parse_millivolts(data: &[u8]) -> Option<u16> {
    for i in 0..data.len().saturating_sub(7) {
        if data[i] == 0x00 && data[i + 1] == 0x07 && data[i + 2] == 0x00 && data[i + 3] == 0x06 && data[i + 5] == 0x00 {
            let mv = u16::from(data[i + 6]) | u16::from(data[i + 7]) << 8;
            if (2500..=4500).contains(&mv) {
                return Some(mv);
            }
        }
    }
    None
}

/// Voltage -> % for the G435 Li-Po cell (portyporty's curve), piecewise-linear.
fn percent(mv: u16) -> u8 {
    const V: [i32; 16] = [4200, 4150, 4100, 4050, 4000, 3950, 3900, 3850, 3800, 3750, 3700, 3650, 3600, 3550, 3500, 3400];
    const P: [i32; 16] = [100, 95, 90, 85, 80, 75, 70, 62, 55, 45, 35, 25, 15, 10, 5, 0];
    let mv = i32::from(mv);
    if mv >= V[0] {
        return 100;
    }
    for i in 1..V.len() {
        if mv >= V[i] {
            let p = P[i] + ((P[i - 1] - P[i]) * (mv - V[i]) + (V[i - 1] - V[i]) / 2) / (V[i - 1] - V[i]);
            return p.clamp(0, 100) as u8;
        }
    }
    0
}

fn drain(dev: &HidDevice) {
    let mut stale = [0u8; 1024];
    while let Ok(n) = dev.read_timeout(&mut stale, 0) {
        if n == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_matches_the_standard_check_value() {
        assert_eq!(crc16(b"123456789"), 0x31C3);
    }

    #[test]
    fn frame_layout_and_crc_cover() {
        let f = frame(0x80, &[0x01, 0x02]);
        assert_eq!(f[0], 0x80);
        assert_eq!(f[1], 2);
        assert_eq!(&f[4..6], &[0x01, 0x02]);
        let mut zeroed = f;
        zeroed[2] = 0;
        zeroed[3] = 0;
        assert_eq!(crc16(&zeroed), u16::from(f[2]) | u16::from(f[3]) << 8);
    }

    #[test]
    fn pa_body_is_ascii_pa_and_category() {
        // 50 41 <category> <seq> <length field> <payload...>
        let mut cmd = vec![0x50, 0x41, CAT_OTA, 2, 1];
        cmd.extend_from_slice(&[0x00, 0x31]);
        let f = frame(0x80, &cmd);
        assert_eq!(&f[4..10], &[0x50, 0x41, 0x06, 0x02, 0x01, 0x00]);
    }

    #[test]
    fn parses_the_get_battery_pattern() {
        // 00 07 00 06 <seq> 00 <mV lo> <mV hi>; 0x0F5E = 3934 mV.
        let reply = [0x00, 0x07, 0x00, 0x06, 0x02, 0x00, 0x5E, 0x0F, 0x00, 0x00];
        assert_eq!(parse_millivolts(&reply), Some(3934));
        // Too low (not a Li-Po cell) and too high are rejected.
        assert_eq!(parse_millivolts(&[0x00, 0x07, 0x00, 0x06, 0x02, 0x00, 0x00, 0x00]), None);
        assert_eq!(parse_millivolts(&[0x00, 0x07, 0x00, 0x06, 0x02, 0x00, 0xFF, 0xFF]), None);
        assert_eq!(parse_millivolts(&[]), None);
    }

    #[test]
    fn voltage_curve_endpoints_and_midpoints() {
        assert_eq!(percent(4200), 100);
        assert_eq!(percent(4300), 100);
        assert_eq!(percent(3400), 0);
        assert_eq!(percent(2500), 0);
        // 3800 is a curve point: 55%.
        assert_eq!(percent(3800), 55);
        // Midway 3975 between 3950 (75%) and 4000 (80%).
        assert_eq!(percent(3975), 78);
        // 3934 from the captured reply above: between 3900 (70) and 3950 (75).
        assert_eq!(percent(3934), 73);
    }
}
