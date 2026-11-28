//! MAONO PD200W battery over its 2.4 GHz receiver (352F:040E).
//!
//! Interface 3, usage page/usage 000C:0001, 64-byte input and output reports, no feature
//! reports. Report 0xC4 carries a register GET (`04`, never the settings SET `03`):
//!
//! ```text
//! C4 09 00 00 04 42 00 ED FE
//! ```
//!
//! Field 1 is the length, 4 the GET opcode, 5-6 the little-endian register (`0x0042` is the
//! battery), 7-8 the two's-complement checksum of the first seven bytes. The reply repeats
//! the header and answers one or more little-endian (register, value) pairs, checksum last.
//!
//! Register and framing were tested on PD200W hardware by the upstream owner; the transport is
//! documented for the sibling 352F:0411. Charging and link state are not established and are
//! never inferred from the level. The receiver stays enumerated when the microphone is off.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice, HidResult};
use std::time::{Duration, Instant};

const VID: u16 = 0x352F;
const PID: u16 = 0x040E;
const KEY: &str = "maono:040e";
const INTERFACE: i32 = 3;
const REPORT_ID: u8 = 0xC4;
const GET: u8 = 0x04;
/// Battery register, inherited from the PD100W and tested on the PD200W.
const REG_BATTERY: u16 = 0x0042;
const REPORT_LEN: usize = 64;
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Maono {
    /// Whether the microphone ever answered; until then a silent receiver isn't listed.
    seen: bool,
}

impl Maono {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let infos: Vec<_> = api
            .device_list()
            .filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == PID
                    && d.interface_number() == INTERFACE
                    && d.usage_page() == 0x000C
                    && d.usage() == 0x0001
            })
            .collect();
        if infos.is_empty() {
            return Vec::new();
        }

        let mut state = None;
        for info in infos {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, REPORT_ID) != Some(REPORT_LEN) || probe::input_len(&desc, REPORT_ID) != Some(REPORT_LEN) {
                continue;
            }
            match read(&dev) {
                Ok(Some(percent)) => {
                    state = Some(State::Battery { percent, charging: false });
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
        // Upstream calls it a microphone; the app only knows the common kinds.
        vec![Observation { id: KEY.into(), name: Some("MAONO PD200W".into()), kind: Some(Kind::Other), state }]
    }
}

fn request() -> [u8; REPORT_LEN] {
    let mut req = [0u8; REPORT_LEN];
    req[0] = REPORT_ID;
    req[1] = 9;
    req[4] = GET;
    req[5] = (REG_BATTERY & 0xFF) as u8;
    req[6] = (REG_BATTERY >> 8) as u8;
    let checksum = checksum(&req[..7]);
    req[7] = (checksum & 0xFF) as u8;
    req[8] = (checksum >> 8) as u8;
    req
}

fn read(dev: &HidDevice) -> HidResult<Option<u8>> {
    drain(dev)?;
    dev.write(&request())?;

    let mut buf = [0u8; REPORT_LEN];
    let deadline = Instant::now() + REPLY_TIMEOUT;
    for _ in 0..128 {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
        let n = match dev.read_timeout(&mut buf, left.as_millis().max(1) as i32) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(e),
        };
        if let Some(percent) = parse(&buf[..n]) {
            return Ok(Some(percent));
        }
    }
    Ok(None)
}

fn drain(dev: &HidDevice) -> HidResult<()> {
    let mut buf = [0u8; REPORT_LEN];
    while dev.read_timeout(&mut buf, 0)? > 0 {}
    Ok(())
}

/// Two's complement of the byte sum, low 16 bits.
fn checksum(data: &[u8]) -> u16 {
    let sum = data.iter().fold(0u32, |sum, b| sum + u32::from(*b));
    (0x10000u32.wrapping_sub(sum & 0xFFFF) & 0xFFFF) as u16
}

fn read_le16(r: &[u8], at: usize) -> Option<u16> {
    Some(u16::from(*r.get(at)?) | u16::from(*r.get(at + 1)?) << 8)
}

fn parse(r: &[u8]) -> Option<u8> {
    if r.len() < 11 || r[0] != REPORT_ID {
        return None;
    }
    let length = usize::from(r[1]);
    if length < 11 || length > r.len() || r[2] != 0 || r[3] != 0 || r[4] != GET {
        return None;
    }
    let end = length - 2;
    if (end - 5) % 4 != 0 || read_le16(r, end)? != checksum(&r[..end]) {
        return None;
    }
    for offset in (5..end).step_by(4) {
        if read_le16(r, offset)? != REG_BATTERY {
            continue;
        }
        let value = read_le16(r, offset + 2)?;
        if value > 100 {
            return None;
        }
        return Some(value as u8);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_documented_request() {
        // C4 09 00 00 04 42 00 ED FE, from the protocol notes.
        assert_eq!(&request()[..9], &[0xC4, 0x09, 0x00, 0x00, 0x04, 0x42, 0x00, 0xED, 0xFE]);
    }

    fn reply(pairs: &[(u16, u16)]) -> Vec<u8> {
        let length = 5 + pairs.len() * 4 + 2;
        let mut r = vec![0u8; length];
        r[..5].copy_from_slice(&[REPORT_ID, length as u8, 0, 0, GET]);
        for (i, (reg, value)) in pairs.iter().enumerate() {
            r[5 + i * 4..7 + i * 4].copy_from_slice(&reg.to_le_bytes());
            r[7 + i * 4..9 + i * 4].copy_from_slice(&value.to_le_bytes());
        }
        let checksum = checksum(&r[..length - 2]);
        r[length - 2..].copy_from_slice(&checksum.to_le_bytes());
        r
    }

    #[test]
    fn parses_the_battery_register() {
        assert_eq!(parse(&reply(&[(REG_BATTERY, 87)])), Some(87));
        // Another register first; the battery pair is found after it.
        assert_eq!(parse(&reply(&[(0x0040, 1), (REG_BATTERY, 42)])), Some(42));
        assert_eq!(parse(&reply(&[(0x0040, 1)])), None);
    }

    #[test]
    fn rejects_bad_frames() {
        let mut r = reply(&[(REG_BATTERY, 87)]);
        let last = r.len() - 1;
        r[last] ^= 0xFF; // checksum
        assert_eq!(parse(&r), None);
        assert_eq!(parse(&reply(&[(REG_BATTERY, 101)])), None);
        let mut short = reply(&[(REG_BATTERY, 87)]);
        short[1] = 200; // length beyond the report
        assert_eq!(parse(&short), None);
        let mut opcode = reply(&[(REG_BATTERY, 87)]);
        opcode[4] = 0x03; // a SET, not a GET answer
        assert_eq!(parse(&opcode), None);
    }

    #[test]
    fn checksum_is_twos_complement() {
        assert_eq!(checksum(&[0xC4, 0x09, 0, 0, 0x04, 0x42, 0]), 0xFEED);
        assert_eq!(checksum(&[0x01]), 0xFFFF);
    }
}
