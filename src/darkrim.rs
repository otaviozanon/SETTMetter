//! DARKRIM SHADOW receiver (3654:6901), hardware-tested, no vendor software required.
//!
//! Interface 3, collection FF02:0002, unnumbered 64-byte reports (65 with the report-id byte).
//! The frame is
//!
//! ```text
//! FE <seq> 10 <type LE> <index> <count> <length LE> <payload…> <crc16 BE>
//! ```
//!
//! Only GET 0010 (request 01) with battery TLV selector 01 00 is sent. The reply carries TLV
//! 01: bit 7 charging, bits 0..6 the level. The CRC is CCITT (poly 0x1021, init 0xFFFF) over
//! the frame without the trailing CRC; the sequence number cycles 1..15.
//!
//! Source: the DARKRIM hub app (version 0.1.26) statically inspected by
//! Wireless-Device-Battery-Monitor (drivers/darkrim.cpp, docs/darkrim.md). The receiver first
//! sends several startup notifications and the matching GET response was measured at ~406 ms,
//! so one deadline covers them all.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice, HidResult};
use std::time::Duration;

const VID: u16 = 0x3654;
const PID: u16 = 0x6901;
const KEY: &str = "darkrim:6901";
const REPLY_TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Default)]
pub struct Darkrim {
    /// Cycle 1..15 so a late reply to the previous request is not mistaken for this one.
    seq: u8,
    /// Whether the headset ever answered; until then a silent receiver isn't listed.
    seen: bool,
}

impl Darkrim {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut state = None;
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID
                && d.product_id() == PID
                && d.interface_number() == 3
                && d.usage_page() == 0xFF02
                && d.usage() == 0x0002
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, 0) != Some(64) || probe::input_len(&desc, 0) != Some(64) {
                continue;
            }
            self.seq = self.seq % 15 + 1;
            match read(&dev, self.seq) {
                Ok(Some(s)) => {
                    state = Some(s);
                    break;
                }
                Ok(None) => {}
                Err(e) => diag::event(KEY, format!("i/o failed: {e}")),
            }
        }

        let state = match state {
            Some(s) => {
                self.seen = true;
                s
            }
            None if self.seen => State::Unreachable,
            None => return Vec::new(),
        };
        vec![Observation { id: KEY.into(), name: Some("DARKRIM SHADOW".into()), kind: Some(Kind::Headset), state }]
    }
}

fn read(dev: &HidDevice, seq: u8) -> HidResult<Option<State>> {
    drain(dev)?;
    dev.write(&request(seq))?;

    let mut buf = [0u8; 128];
    let deadline = std::time::Instant::now() + REPLY_TIMEOUT;
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        let n = match dev.read_timeout(&mut buf, left.as_millis().max(1) as i32) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(e),
        };
        if let Some(s) = reply(&buf[..n], seq) {
            return Ok(Some(s));
        }
    }
    Ok(None)
}

fn drain(dev: &HidDevice) -> HidResult<()> {
    let mut buf = [0u8; 128];
    while dev.read_timeout(&mut buf, 0)? > 0 {}
    Ok(())
}

/// `FE <seq> 10 <type LE> <index> <count> <length LE> <payload…> <crc BE>`, behind report id 0.
fn request(seq: u8) -> Vec<u8> {
    let mut frame = vec![0xFE, seq, 0x10, 0x00, 1, 0, 1, 2, 0, 1, 0];
    let crc = crc(&frame);
    frame.push((crc >> 8) as u8);
    frame.push(crc as u8);
    let mut report = vec![0u8];
    report.extend_from_slice(&frame);
    report
}

fn reply(r: &[u8], seq: u8) -> Option<State> {
    if r.len() < 11 || r[0] != 0xFE || r[1] != seq || r[2] != 0x10 || r[3] != 0 || r[4] != 2 || r[5] != 0 || r[6] > 1 {
        return None;
    }
    let len = usize::from(r[7]) | usize::from(r[8]) << 8;
    if len > 53 || len + 11 > r.len() {
        return None;
    }
    let end = 9 + len;
    if crc(&r[..end]) != (u16::from(r[end]) << 8 | u16::from(r[end + 1])) {
        return None;
    }

    let mut value = None;
    let mut i = 9;
    while i < end {
        if end - i < 2 {
            return None;
        }
        let kind = r[i];
        let size = usize::from(r[i + 1]);
        i += 2;
        if size > end - i {
            return None;
        }
        if kind == 1 {
            if size != 1 || value.is_some() || (r[i] & 0x7F) > 100 {
                return None;
            }
            value = Some(r[i]);
        }
        i += size;
    }
    let value = value?;
    Some(State::Battery { percent: value & 0x7F, charging: value & 0x80 != 0 })
}

fn crc(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for byte in data {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply_frame(seq: u8, tlvs: &[u8]) -> Vec<u8> {
        let mut r = vec![0xFE, seq, 0x10, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00];
        r[7] = tlvs.len() as u8;
        r.extend_from_slice(tlvs);
        let crc = crc(&r);
        r.push((crc >> 8) as u8);
        r.push(crc as u8);
        r
    }

    #[test]
    fn crc_matches_the_ccitt_check_value() {
        assert_eq!(crc(b"123456789"), 0x29B1);
    }

    #[test]
    fn parses_the_battery_tlv() {
        // TLV 01, one byte: bit 7 charging, bits 0..6 the level.
        assert_eq!(reply(&reply_frame(3, &[0x01, 0x01, 87]), 3), Some(State::Battery { percent: 87, charging: false }));
        assert_eq!(reply(&reply_frame(3, &[0x01, 0x01, 0x80 | 87]), 3), Some(State::Battery { percent: 87, charging: true }));
        // Another TLV first, then the battery one.
        assert_eq!(
            reply(&reply_frame(3, &[0x02, 0x02, 0xAA, 0xBB, 0x01, 0x01, 42]), 3),
            Some(State::Battery { percent: 42, charging: false })
        );
    }

    #[test]
    fn rejects_wrong_sequence_crc_and_level() {
        assert_eq!(reply(&reply_frame(3, &[0x01, 0x01, 87]), 4), None); // another request's reply
        let mut bad = reply_frame(3, &[0x01, 0x01, 87]);
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        assert_eq!(reply(&bad, 3), None);
        let mut frame = reply_frame(3, &[0x01, 0x01, 0x80 | 101]);
        assert_eq!(reply(&frame, 3), None);
        // A battery TLV of the wrong width is not a level.
        frame = reply_frame(3, &[0x01, 0x02, 42, 0]);
        assert_eq!(reply(&frame, 3), None);
        assert_eq!(reply(&[], 3), None);
    }

    #[test]
    fn request_is_well_formed() {
        let r = request(1);
        assert_eq!(r[0], 0x00); // report id
        assert_eq!(&r[1..4], &[0xFE, 0x01, 0x10]);
        let end = r.len() - 2;
        assert_eq!(crc(&r[1..end]), u16::from(r[end]) << 8 | u16::from(r[end + 1]));
    }
}
