//! ATK / VXE NearLink mice and the ATK Zero, plus the Holtek-based "8K Nano Dongle".
//!
//! BITMOUSE (373B:1216 Nano dongle, 124C NK cable, 1154/124F/1155 ATK Zero), on vendor
//! collection FF05:0001:
//!   Output report 08, 64 bytes: `[08][checksum][72][02][00][target][cmd][01][00 …]`,
//!   checksum = sum(bytes 2..63) & 0xFF, target 01 = the mouse behind the dongle (00 = the Zero
//!   cable). The Zero instead fixes the checksum byte at 0x7D.
//!   Reply: `[08][72][status][seq][00][cmd][01][value]…`; status FF = error.
//!   cmd 07 = battery level, cmd 0F = charging flag (NearLink); cmd 08 = charging on the Zero
//!   cable. No reply = the mouse is asleep or switched off.
//!
//! 8K Nano (373B:1178) is a Holtek receiver on FF10:0001 (65-byte input/output, 9-byte feature
//! report, no report ids): feature report 0 `[82][00 x7]` (GetConfigData) is answered as an input
//! report `[82][rate][dpi stage][light mode][brightness][speed][color][reserved][battery][link]…`;
//! battery bit 7 = charging, bits 0-6 = percent, link 0 = no mouse paired or awake.
//!
//! Protocol and ids from Wireless-Device-Battery-Monitor (drivers/atk_bitmouse.cpp), which
//! credits HeyOkay/HaloBattery, qwr79/mouse-battery, Fan4Metal/mouse_tray and the ATK HUB
//! JavaScript. Only the battery/link reads are sent.

use crate::device::{Kind, Observation, State};
use crate::hidutil::drain as flush;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const VID: u16 = 0x373B;
const NANO_DONGLE: u16 = 0x1216;
const NK_CABLE: u16 = 0x124C;
const NANO_8K: u16 = 0x1178;
const ZERO_CABLE: u16 = 0x1154;
const ZERO_RECEIVERS: [u16; 2] = [0x124F, 0x1155];
/// The Zero's frames carry this fixed checksum byte instead of the sum.
const ZERO_SUM: u8 = 0x7D;

#[derive(Clone, Copy)]
enum Link {
    NearLink,
    ZeroCable,
    ZeroReceiver,
}

#[derive(Default)]
pub struct AtkBitmouse {
    /// Last name each key reported, so a sleeping mouse keeps its model.
    seen: HashMap<String, String>,
}

impl AtkBitmouse {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        self.zero(api, &mut out);
        self.nano8k(api, &mut out);
        self.nk(api, &mut out);

        let ids: HashSet<String> = out.iter().map(|o| o.id.clone()).collect();
        for (key, name) in &self.seen {
            if !ids.contains(key) {
                out.push(observation(key, name, State::Unreachable));
            }
        }
        out
    }

    fn zero(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for pid in [ZERO_CABLE, ZERO_RECEIVERS[0], ZERO_RECEIVERS[1]] {
            let wired = pid == ZERO_CABLE;
            let link = if wired { Link::ZeroCable } else { Link::ZeroReceiver };
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0xFF05 && d.usage() == 0x0001
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if caps(probe::output_len(&desc, 0x08), 0x08) != 64 || caps(probe::input_len(&desc, 0x08), 0x08) != 64
                {
                    continue;
                }
                let Some(level) = query(&dev, 0x07, link) else { continue };
                if level > 100 {
                    continue;
                }
                let charging = wired && query(&dev, 0x08, link) == Some(0x02);
                self.push(out, "atk:1154", "ATK Zero", State::Battery { percent: level, charging });
                return;
            }
        }
    }

    fn nano8k(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == NANO_8K && d.usage_page() == 0xFF10 && d.usage() == 0x0001
        }) {
            let Some(dev) = api.open_path(info.path()).ok() else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if caps(probe::feature_len(&desc, 0), 0) < 9 || caps(probe::input_len(&desc, 0), 0) < 11 {
                continue;
            }
            let Some(config) = query_config(&dev) else { continue };
            let Some((level, charging)) = nano_config(&config) else { continue };
            self.push(out, "atk:1178", "ATK 8K Nano mouse", State::Battery { percent: level, charging });
            return;
        }
    }

    fn nk(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        // The cable is tried first so an idle dongle cannot hand back a stale reading while the
        // mouse is wired; both report under the dongle's key.
        let mut wired = false;
        for pid in [NK_CABLE, NANO_DONGLE] {
            if wired {
                break;
            }
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == 0xFF05 && d.usage() == 0x0001
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if caps(probe::output_len(&desc, 0x08), 0x08) < 64 || caps(probe::input_len(&desc, 0x08), 0x08) < 8 {
                    continue;
                }
                let Some(level) = query(&dev, 0x07, Link::NearLink) else { continue };
                if !(1..=100).contains(&level) {
                    continue;
                }
                let cable = pid == NK_CABLE;
                let name = if cable {
                    let product = info.product_string().unwrap_or("").trim();
                    if product.is_empty() { "ATK / VXE NK mouse".to_string() } else { product.to_string() }
                } else {
                    "ATK / VXE NK mouse".to_string()
                };
                let charging = cable || query(&dev, 0x0F, Link::NearLink) == Some(1);
                self.push(out, "atk:1216", &name, State::Battery { percent: level, charging });
                if cable {
                    wired = true;
                }
                break;
            }
        }
    }

    fn push(&mut self, out: &mut Vec<Observation>, key: &str, name: &str, state: State) {
        self.seen.insert(key.into(), name.into());
        out.push(observation(key, name, state));
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

/// Windows' report length: hidapi hides the report-id byte of an unnumbered report.
fn caps(len: Option<usize>, id: u8) -> usize {
    len.map_or(0, |l| l + usize::from(id == 0))
}

fn request(cmd: u8, link: Link) -> [u8; 64] {
    let zero = !matches!(link, Link::NearLink);
    let mut req = [0u8; 64];
    req[0] = 0x08;
    req[2] = 0x72;
    req[3] = 0x02;
    req[5] = if matches!(link, Link::ZeroCable) { 0x00 } else { 0x01 };
    req[6] = cmd;
    req[7] = 0x01;
    let sum: u32 = req[2..].iter().map(|b| u32::from(*b)).sum();
    req[1] = if zero { ZERO_SUM } else { sum as u8 };
    req
}

fn query(dev: &HidDevice, cmd: u8, link: Link) -> Option<u8> {
    let zero = !matches!(link, Link::NearLink);
    let req = request(cmd, link);
    flush(dev);
    dev.write(&req).ok()?;

    // The NearLink dongle keeps the reply window a little longer.
    let wait = if zero { Duration::from_millis(500) } else { Duration::from_millis(700) };
    let deadline = Instant::now() + wait;
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(0) => return None,
            Ok(n) => match reply(&buf[..n], cmd, zero) {
                Answer::Value(v) => return Some(v),
                Answer::Error => return None,
                Answer::Skip => {}
            },
            Err(_) => return None,
        }
    }
    None
}

enum Answer {
    Value(u8),
    /// The dongle answered for this command with an error.
    Error,
    /// A frame for another request.
    Skip,
}

fn reply(r: &[u8], cmd: u8, zero: bool) -> Answer {
    if r.len() < 8 || r[0] != 0x08 || r[1] != 0x72 {
        return Answer::Skip;
    }
    if r[5] != cmd || (zero && r[6] != 0x01) {
        return Answer::Skip;
    }
    // Only status 00 is fresh on the Zero; FF is an error on NearLink.
    let error = if zero { r[2] != 0x00 } else { r[2] == 0xFF };
    if error {
        return Answer::Error;
    }
    Answer::Value(r[7])
}

fn query_config(dev: &HidDevice) -> Option<Vec<u8>> {
    flush(dev);
    dev.send_feature_report(&[0x00, 0x82]).ok()?;

    let deadline = Instant::now() + Duration::from_millis(700);
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(0) => return None,
            Ok(n) if n >= 10 => {
                // Other input reports (e.g. 0x12 DPI / rate notifications) are skipped.
                if buf[0] == 0x82 {
                    return Some(buf[..n].to_vec());
                }
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

/// `[8]` battery: bit 7 charging, bits 0-6 percent; `[9]` link, 0 = no mouse paired or awake.
fn nano_config(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 10 || r[0] != 0x82 || r[9] == 0 {
        return None;
    }
    let level = r[8] & 0x7F;
    if !(1..=100).contains(&level) {
        return None;
    }
    Some((level, r[8] & 0x80 != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_checksum_is_the_sum_of_the_frame() {
        let req = request(0x07, Link::NearLink);
        let sum: u32 = req[2..].iter().map(|b| u32::from(*b)).sum();
        assert_eq!(u32::from(req[1]), sum & 0xFF);
        assert_eq!(req[1], 0x7D); // 72 + 02 + 01 + 07 + 01

        let charging = request(0x0F, Link::NearLink);
        assert_eq!(charging[1], 0x85);

        let zero = request(0x07, Link::ZeroCable);
        assert_eq!(zero[1], ZERO_SUM);
        assert_eq!(zero[5], 0x00);
    }

    #[test]
    fn replies_are_matched_to_their_command() {
        let good = [0x08, 0x72, 0x00, 0x00, 0x00, 0x07, 0x01, 0x57];
        assert!(matches!(reply(&good, 0x07, false), Answer::Value(0x57)));

        assert!(matches!(reply(&good, 0x0F, false), Answer::Skip)); // another request
        let mut error = good;
        error[2] = 0xFF;
        assert!(matches!(reply(&error, 0x07, false), Answer::Error));

        // The Zero requires the echoed length byte and status 00.
        assert!(matches!(reply(&good, 0x07, true), Answer::Value(0x57)));
        let mut wrong = good;
        wrong[6] = 0x00;
        assert!(matches!(reply(&wrong, 0x07, true), Answer::Skip));
        let mut status = good;
        status[2] = 0x01;
        assert!(matches!(reply(&status, 0x07, true), Answer::Error));
    }

    #[test]
    fn nano8k_config_gives_level_and_link() {
        let mut r = [0u8; 16];
        r[0] = 0x82;
        r[8] = 0x80 | 87;
        r[9] = 1;
        assert_eq!(nano_config(&r), Some((87, true)));

        r[8] = 55;
        assert_eq!(nano_config(&r), Some((55, false)));

        r[9] = 0; // no mouse paired or awake
        assert_eq!(nano_config(&r), None);

        r[9] = 1;
        r[8] = 0; // 0 % is not a reading
        assert_eq!(nano_config(&r), None);

        r[8] = 0x7F; // 127 is not a level
        assert_eq!(nano_config(&r), None);

        r[8] = 50;
        r[0] = 0x12; // a DPI/rate notification
        assert_eq!(nano_config(&r), None);
    }
}
