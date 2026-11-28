//! PlayStation PULSE Explore earbuds on the PlayStation Link USB adapter (054C:0ECC).
//!
//! Read-only feature reports on the vendor collection FF01:0020 (64-byte feature reports,
//! interface 3), the same ones the PlayStation Link PC app polls:
//!
//! - `82`: link state, byte 1 = 01 while a device is connected.
//! - `A0`: name of the connected device, ASCII from byte 1.
//! - `B0`: device status, fails while nothing is connected. Byte 55 holds the earbud levels as
//!   two nibbles on a 0-15 scale: low nibble left, high nibble right.
//!
//! Byte 55 was checked on one pair: AE = L 93% R 67%, FF = L 100% R 100%, matching the app.
//! Charging, the case level and buds-in-case readings are not decoded. The PULSE Elite shares
//! the adapter but its status layout is unverified, so only "PULSE Explore" is reported.
//!
//! Decoded from USB captures of the PlayStation Link app and ported from
//! Wireless-Device-Battery-Monitor (drivers/playstation_link.cpp).

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::{HidApi, HidDevice};

const VID: u16 = 0x054C;
const PID: u16 = 0x0ECC;
const KEY: &str = "pslink:0ecc";
const NAME: &str = "PULSE Explore";
const LINK_REPORT: u8 = 0x82;
const NAME_REPORT: u8 = 0xA0;
const STATUS_REPORT: u8 = 0xB0;
const LEVELS_BYTE: usize = 55;

#[derive(Default)]
pub struct PlayStationLink {}

impl PlayStationLink {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == 0xFF01 && d.usage() == 0x0020
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::feature_len(&desc, LINK_REPORT).is_none_or(|n| n < 64) {
                continue;
            }
            let Some(link) = feature(&dev, LINK_REPORT) else { continue };
            if link.len() < 2 {
                continue;
            }
            let connected = link[1] != 0;
            let device_name = connected.then(|| feature(&dev, NAME_REPORT)).flatten().map(|r| ascii_name(&r));
            let status = connected.then(|| feature(&dev, STATUS_REPORT)).flatten();
            let (Some(device_name), Some(status)) = (device_name, status) else {
                // Adapter present but the earbuds are off or in the case.
                return vec![observation(State::Off)];
            };
            if status.len() <= LEVELS_BYTE || status[0] != STATUS_REPORT {
                return vec![observation(State::Off)];
            }
            if device_name != NAME {
                return Vec::new(); // another PlayStation Link device: layout unknown
            }
            let levels = status[LEVELS_BYTE];
            let left = nibble_percent(levels & 0x0F);
            let right = nibble_percent(levels >> 4);
            return vec![observation(State::Battery { percent: left.min(right), charging: false })];
        }
        Vec::new()
    }
}

fn observation(state: State) -> Observation {
    Observation { id: KEY.into(), name: Some(NAME.into()), kind: Some(Kind::Headset), state }
}

/// One GetFeatureReport; the returned buffer keeps the report id in byte 0.
fn feature(dev: &HidDevice, id: u8) -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    buf[0] = id;
    dev.get_feature_report(&mut buf).ok().map(|n| buf[..n].to_vec())
}

/// ASCII device name from byte 1 up to the first non-printable byte.
fn ascii_name(r: &[u8]) -> String {
    r[1..].iter().take_while(|b| (0x20..0x7F).contains(*b)).map(|b| *b as char).collect()
}

/// 0-15 -> percent, rounded: 14 -> 93, 10 -> 67.
fn nibble_percent(n: u8) -> u8 {
    ((u16::from(n) * 100 + 7) / 15) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nibble_scale_matches_the_capture() {
        assert_eq!(nibble_percent(0), 0);
        assert_eq!(nibble_percent(15), 100);
        assert_eq!(nibble_percent(14), 93); // low nibble E
        assert_eq!(nibble_percent(10), 67); // high nibble A
    }

    #[test]
    fn levels_byte_is_split_low_left_high_right() {
        let mut status = vec![0u8; 64];
        status[0] = STATUS_REPORT;
        status[LEVELS_BYTE] = 0xAE; // L 93, R 67
        let levels = status[LEVELS_BYTE];
        assert_eq!((nibble_percent(levels & 0x0F), nibble_percent(levels >> 4)), (93, 67));
        assert_eq!(nibble_percent(levels & 0x0F).min(nibble_percent(levels >> 4)), 67);
        status[LEVELS_BYTE] = 0xFF;
        assert_eq!(nibble_percent(0xFF & 0x0F), 100);
    }

    #[test]
    fn name_stops_at_the_first_non_printable_byte() {
        let mut r = vec![0u8; 64];
        r[0] = NAME_REPORT;
        r[1..14].copy_from_slice(b"PULSE Explore");
        assert_eq!(ascii_name(&r), NAME);
        let mut other = r.clone();
        other[1] = b'X';
        assert_ne!(ascii_name(&other), NAME);
    }
}
