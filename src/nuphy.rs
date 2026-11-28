//! NuPhy WH80 over USB (19F5:A011), hardware-tested.
//!
//! Source: NuPhy's web driver (NuPhyIO SDK). getBatteryState(0) is a GetFunc (05) of one byte at
//! offset 0020; only that GET is sent. Unnumbered 64-byte reports on usage 0001:0000, interface 1:
//!
//!   `55 05 00 21 01 20 00 00 …` -> `AA 05 00 <checksum> 01 20 00 00 <value> …`
//!
//! The checksum is the low byte of the sum of bytes 4..63. Value: bit 7 charging, bits 0-6
//! percent (the WH80 reports in 10 % steps). Bluetooth and receiver modes are untested.
//!
//! Protocol from Wireless-Device-Battery-Monitor (drivers/nuphy.cpp).

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::drain as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x19F5;
const PID: u16 = 0xA011;
const KEY: &str = "nuphy:wh80";
const WAIT: Duration = Duration::from_millis(400);
const LEN: usize = 64;

#[derive(Default)]
pub struct Nuphy {
    seen: bool,
}

impl Nuphy {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == PID && d.interface_number() == 1 && d.usage_page() == 0x0001 && d.usage() == 0x0000
        }) {
            let Some(dev) = api.open_path(info.path()).ok() else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if caps(probe::input_len(&desc, 0)) != 65 || caps(probe::output_len(&desc, 0)) != 65 {
                continue;
            }
            match query(&dev) {
                Ok(Some((percent, charging))) => {
                    self.seen = true;
                    return vec![observation(State::Battery { percent, charging })];
                }
                Ok(None) => {}
                Err(e) => diag::event(KEY, format!("i/o failed: {e}")),
            }
        }
        if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() }
    }
}

fn observation(state: State) -> Observation {
    Observation { id: KEY.into(), name: Some("NuPhy WH80".into()), kind: Some(Kind::Keyboard), state }
}

/// Windows' report length: hidapi hides the report-id byte of an unnumbered report.
fn caps(len: Option<usize>) -> usize {
    len.map_or(0, |l| l + 1)
}

fn query(dev: &HidDevice) -> hidapi::HidResult<Option<(u8, bool)>> {
    flush(dev);
    dev.write(&[0x00, 0x55, 0x05, 0x00, 0x21, 0x01, 0x20, 0x00, 0x00])?;

    let deadline = Instant::now() + WAIT;
    let mut buf = [0u8; LEN];
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

/// A valid reply: `AA 05 00 <sum> 01 20 00 00 <value>` with the sum over bytes 4..63.
fn parse(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() != LEN || r[0] != 0xAA || r[1] != 0x05 || r[2] != 0 || r[4] != 1 || r[5] != 0x20 || r[6] != 0 || r[7] != 0
    {
        return None;
    }
    let sum = r[4..].iter().fold(0u8, |s, b| s.wrapping_add(*b));
    if sum != r[3] {
        return None;
    }
    let level = r[8] & 0x7F;
    if level > 100 {
        return None;
    }
    Some((level, r[8] & 0x80 != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(value: u8) -> [u8; LEN] {
        let mut r = [0u8; LEN];
        r[0] = 0xAA;
        r[1] = 0x05;
        r[2] = 0x00;
        r[4] = 0x01;
        r[5] = 0x20;
        r[8] = value;
        let sum = r[4..].iter().fold(0u8, |s, b| s.wrapping_add(*b));
        r[3] = sum;
        r
    }

    #[test]
    fn parses_a_wh80_battery_reply() {
        assert_eq!(parse(&reply(50)), Some((50, false)));
        assert_eq!(parse(&reply(0x80 | 50)), Some((50, true)));
        assert_eq!(parse(&reply(0)), Some((0, false))); // the WH80 reports in 10 % steps
        assert_eq!(parse(&reply(0x7F)), None); // 127 is not a level
    }

    #[test]
    fn checksum_is_the_low_byte_of_bytes_4_to_63() {
        let good = reply(50);
        assert_eq!(good[3], 0x01u8.wrapping_add(0x20).wrapping_add(50));

        let mut bad = good;
        bad[3] ^= 0xFF;
        assert_eq!(parse(&bad), None);

        let mut short = good;
        short[63] = 0x77; // checksum no longer covers the frame
        assert_eq!(parse(&short), None);

        let mut wrong = good;
        wrong[5] = 0x21;
        assert_eq!(parse(&wrong), None);
    }
}
