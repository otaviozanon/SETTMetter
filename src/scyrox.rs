//! Scyrox V12M on its 2.4 GHz receiver 395A:0006.
//!
//! Only this exact id is handled: the V6S / V8S on the same vendor id answer a different status
//! command, and the older V6 / V8 sit on Compx's 0x3554.
//!
//! Read-only profile read from Scyrox's S-Center 2.0 web driver, on the vendor collection
//! FF00:0002 (64-byte input and output report 0x08):
//!   Output report 0x08: `08 01 00 … <checksum>`, checksum = low byte of 0x08 + 0x01 = 0x09.
//!   The receiver answers with a stream of 64-byte input reports 0x08 (one 08 01 frame per
//!   profile plus axis frames 23 / 25, ended by an 08 01 frame with [3] = 0). The active
//!   profile's frame has [4] = the active profile = [5], [7] = DPI stage count 1..5, [62] = 0
//!   (1 = error) and the battery percent at [20], after five 16-bit DPI values and the polling
//!   rate. The protocol has no charging field, so charging is not shown.
//!
//! From late-autumn-gif/mousedock (MIT, ReferenceBatteryProtocol.cs / ScyroxV12Dpi.cs) and
//! MoFeng2223/BrimDeck; nothing here writes settings: command 0x01 only reads the profiles.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x395A;
const V12M_RECEIVER: u16 = 0x0006;
const REPORT_ID: u8 = 0x08;
const READ_PROFILES: u8 = 0x01;
const LEN: usize = 64;
const WAIT: Duration = Duration::from_millis(1500);

#[derive(Default)]
pub struct Scyrox {
    /// Answered once, so a sleeping mouse keeps its row instead of disappearing.
    seen: bool,
}

impl Scyrox {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let key = format!("scyrox:{V12M_RECEIVER:04x}");
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == V12M_RECEIVER && d.usage_page() == 0xFF00 && d.usage() == 0x0002
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, REPORT_ID) != Some(LEN) || probe::input_len(&desc, REPORT_ID) != Some(LEN) {
                continue;
            }
            match read_battery(&dev) {
                // No valid reply: the mouse is asleep or off.
                Ok(None) => {}
                Ok(Some(percent)) => {
                    self.seen = true;
                    diag::event(&key, format!("battery {percent}% via profiles"));
                    return vec![observation(&key, State::Battery { percent, charging: false })];
                }
                Err(e) => diag::event(&key, format!("i/o failed: {e}")),
            }
        }
        if self.seen {
            return vec![observation(&key, State::Unreachable)];
        }
        Vec::new()
    }
}

fn observation(key: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some("Scyrox V12M".into()), kind: Some(Kind::Mouse), state }
}

fn read_battery(dev: &HidDevice) -> hidapi::HidResult<Option<u8>> {
    let mut req = [0u8; LEN];
    req[0] = REPORT_ID;
    req[1] = READ_PROFILES;
    req[LEN - 1] = 0x09; // low byte of 08 + 01
    let mut buf = [0u8; LEN];
    // A frame queued before the request isn't part of its answer.
    drain(dev, &mut buf)?;
    dev.write(&req)?;

    let deadline = Instant::now() + WAIT;
    let mut percent = None;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32)? {
            0 => break,
            n => match frame(&buf[..n]) {
                Some(Frame::End) => break,
                Some(Frame::Percent(p)) => {
                    percent = Some(p);
                    break;
                }
                None => {} // axis frames and other replies
            },
        }
    }
    drain(dev, &mut buf)?; // drop the rest of the profile stream
    Ok(percent)
}

fn drain(dev: &HidDevice, buf: &mut [u8]) -> hidapi::HidResult<()> {
    while dev.read_timeout(buf, 0)? != 0 {}
    Ok(())
}

enum Frame {
    /// The stream ended (or the request came echoed back).
    End,
    Percent(u8),
}

/// A profile frame; only the active profile's frame carries the battery.
fn frame(r: &[u8]) -> Option<Frame> {
    if r.len() != LEN || r[0] != REPORT_ID || r[1] != READ_PROFILES {
        return None;
    }
    if r[3] == 0 {
        return Some(Frame::End);
    }
    if r[62] == 1 || r[4] > 3 || r[4] != r[5] || !(1..=5).contains(&r[7]) {
        return None; // not the active profile
    }
    if r[20] > 100 {
        return Some(Frame::End);
    }
    Some(Frame::Percent(r[20]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active(percent: u8) -> Vec<u8> {
        let mut r = vec![0u8; LEN];
        r[0] = REPORT_ID;
        r[1] = READ_PROFILES;
        r[3] = 0x01;
        r[4] = 2; // active profile
        r[5] = 2; // profile in this frame
        r[7] = 3; // DPI stages
        r[20] = percent;
        r
    }

    #[test]
    fn reads_the_active_profile_battery() {
        assert!(matches!(frame(&active(77)), Some(Frame::Percent(77))));
        assert!(matches!(frame(&active(0)), Some(Frame::Percent(0))));
    }

    #[test]
    fn stops_at_end_and_rejects_other_frames() {
        let mut end = active(77);
        end[3] = 0;
        assert!(matches!(frame(&end), Some(Frame::End)));

        let mut axis = active(77);
        axis[1] = 0x23;
        assert!(matches!(frame(&axis), None));

        let mut inactive = active(77);
        inactive[4] = 1;
        assert!(matches!(frame(&inactive), None));

        let mut error = active(77);
        error[62] = 1;
        assert!(matches!(frame(&error), None));

        let mut high = active(77);
        high[20] = 101;
        assert!(matches!(frame(&high), Some(Frame::End)));

        assert!(matches!(frame(&active(77)[..63]), None)); // short
    }
}
