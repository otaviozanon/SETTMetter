//! Google Fast Pair battery broadcasts: the left, right and case split of earbuds (Jabra Elite,
//! Pixel Buds, many Sony / JBL / Bose models ...) that Windows' single Battery Service level
//! cannot show.
//!
//! Service data of 16-bit UUID 0xFE2C, after the UUID: `[0]` version/flags (0x00), then fields
#![allow(dead_code)] // stream parser kept for the Fast Pair transport
//! whose header byte is `0bLLLLTTTT` (L = length, T = type):
//!   T 0 / 2  account key filter (shown / hidden UI), T 1 salt
//!   T 3 / 4  battery (shown / hidden UI): one byte per component, in order left, right, case;
//!            bit 7 = charging, bits 0-6 = level 0-100, 0x7F = unknown
//!   T 5      remaining battery time
//! Discoverable-mode advertisements carry only the 3-byte model id (no version byte) and are ignored.
//!
//! Messages are matched to the paired device by its public Bluetooth address, so a device
//! broadcasting from a rotating random address is not matched. Like upstream (which folds this
//! into the Bluetooth entry), `scan` reuses the Bluetooth source's `bt:` id, so the tracker merges
//! the split into that row instead of adding a second one.
//!
//! Watching advertisements happens in the shared watcher (`ble_adverts`); this module only decodes
//! what it cached. Layout from the Fast Pair specification, as in upstream fastpair.cpp.
use crate::ble_adverts;
use crate::device::{Observation, State};

/// Shown level: the lower discharging bud, else the lower charging one. `None` when neither bud
/// reports (the case alone is not shown, same as upstream, so Windows' own level survives).
pub fn battery_level(b: &Battery) -> Option<(u8, bool)> {
    let (mut discharging, mut charging) = (None::<u8>, None::<u8>);
    for (percent, chg) in [(b.left, b.left_charging), (b.right, b.right_charging)] {
        if let Some(p) = percent {
            let slot = if chg { &mut charging } else { &mut discharging };
            *slot = Some(match *slot {
                Some(v) => v.min(p),
                None => p,
            });
        }
    }
    if let Some(p) = discharging {
        Some((p, false))
    } else {
        charging.map(|p| (p, true))
    }
}

#[derive(Default)]
pub struct FastPair;

impl FastPair {
    pub fn scan(&mut self) -> Vec<Observation> {
        ble_adverts::ensure();
        ble_adverts::fast_pair()
            .into_iter()
            .filter_map(|a| {
                let (percent, charging) = battery_level(&a.battery)?;
                Some(Observation {
                    // Same id as the Bluetooth source, so this only refines that row.
                    id: format!("bt:{:012x}", a.addr),
                    name: None,
                    kind: None,
                    state: State::Battery { percent: percent.min(100), charging },
                })
            })
            .collect()
    }
}

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Battery {
    pub left: Option<u8>,
    pub right: Option<u8>,
    pub case: Option<u8>,
    pub left_charging: bool,
    pub right_charging: bool,
    pub case_charging: bool,
}

fn component(b: u8) -> (Option<u8>, bool) {
    let level = b & 0x7F;
    ((level <= 100).then_some(level), level <= 100 && b & 0x80 != 0)
}

/// Parses the service data after the 0xFE2C UUID.
pub fn parse(data: &[u8]) -> Option<Battery> {
    if data.len() < 2 || data[0] != 0x00 {
        return None;
    }
    let mut out = Battery::default();
    let mut i = 1;
    while i < data.len() {
        let len = (data[i] >> 4) as usize;
        let kind = data[i] & 0x0F;
        if i + 1 + len > data.len() {
            return None;
        }
        if (kind == 3 || kind == 4) && len >= 1 {
            (out.left, out.left_charging) = component(data[i + 1]);
            if len >= 2 {
                (out.right, out.right_charging) = component(data[i + 2]);
            }
            if len >= 3 {
                (out.case, out.case_charging) = component(data[i + 3]);
            }
            return (out.left.is_some() || out.right.is_some() || out.case.is_some()).then_some(out);
        }
        i += 1 + len;
    }
    None
}

// ---- Fast Pair Message Stream (RFCOMM {df21fe2c-2515-4fdb-8886-f12c4d67927c}) ----

pub const GROUP_DEVICE_INFO: u8 = 0x03;
pub const CODE_MODEL_ID: u8 = 0x01;
pub const CODE_BLE_ADDRESS: u8 = 0x02;

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct StreamInfo {
    pub has_model_id: bool,
    pub model_id: u32,
    pub has_le_address: bool,
    pub le_address: u64,
}

/// Reads every complete message in `data`, recording the ones above (later ones win). Returns the
/// bytes consumed by complete messages; a trailing partial message is left for the next call.
/// Each message is `[group][code][length, 2 bytes big-endian][data...]`.
pub fn parse_stream(data: &[u8], info: &mut StreamInfo) -> usize {
    let mut pos = 0;
    while data.len() - pos >= 4 {
        let len = usize::from(data[pos + 2]) << 8 | usize::from(data[pos + 3]);
        if data.len() - pos - 4 < len {
            break; // incomplete: wait for more bytes
        }
        let p = &data[pos + 4..pos + 4 + len];
        if data[pos] == GROUP_DEVICE_INFO && data[pos + 1] == CODE_MODEL_ID && len == 3 {
            info.model_id = u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2]);
            info.has_model_id = true;
        } else if data[pos] == GROUP_DEVICE_INFO && data[pos + 1] == CODE_BLE_ADDRESS && len == 6 {
            let mut addr = 0u64;
            for &b in p {
                addr = addr << 8 | u64::from(b);
            }
            info.le_address = addr;
            info.has_le_address = addr != 0;
        }
        pos += 4 + len;
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_component_battery_notification() {
        // Version 0, header 0x33 (length 3, type 3): left 80, right 75 charging, case unknown.
        let b = parse(&[0x00, 0x33, 0x50, 0xCB, 0x7F]).unwrap();
        assert_eq!((b.left, b.right, b.case), (Some(80), Some(75), None));
        assert!(!b.left_charging && b.right_charging);
        assert_eq!(battery_level(&b), Some((80, false)));
    }

    #[test]
    fn parses_hidden_ui_type_and_short_payloads() {
        // Type 4 (hidden UI), only the left bud reported.
        let b = parse(&[0x00, 0x14, 0x64]).unwrap();
        assert_eq!((b.left, b.right, b.case), (Some(100), None, None));
        assert_eq!(battery_level(&b), Some((100, false)));
    }

    #[test]
    fn skips_other_fields_before_the_battery() {
        // Salt (type 1, length 2) then battery (type 3, length 2).
        let b = parse(&[0x00, 0x21, 0xAA, 0xBB, 0x23, 0x4B, 0xD2]).unwrap();
        assert_eq!((b.left, b.right), (Some(75), Some(82)));
        assert!(b.right_charging);
    }

    #[test]
    fn rejects_discoverable_mode_truncated_and_unknown_levels() {
        // Discoverable mode: 3-byte model id, no version byte.
        assert!(parse(&[0x00, 0x1A, 0x0B]).is_none());
        // Field length runs past the data.
        assert!(parse(&[0x00, 0x33, 0x50]).is_none());
        // No battery field at all: salt only.
        assert!(parse(&[0x00, 0x11, 0xAA]).is_none());
        // Every component 0x7F = unknown.
        assert!(parse(&[0x00, 0x33, 0x7F, 0x7F, 0x7F]).is_none());
    }

    #[test]
    fn charging_only_buds_report_charging() {
        let b = parse(&[0x00, 0x23, 0xC8, 0xE1]).unwrap();
        assert_eq!((b.left, b.right), (Some(72), Some(97)));
        assert_eq!(battery_level(&b), Some((72, true)));
        // Only the case is reported: no shown level, Windows keeps its own.
        let case_only = Battery { case: Some(60), ..Default::default() };
        assert_eq!(battery_level(&case_only), None);
    }

    #[test]
    fn stream_parses_model_id_and_ble_address() {
        let mut info = StreamInfo::default();
        let data = [0x03, 0x01, 0x00, 0x03, 0x00, 0x00, 0x1A, 0x03, 0x02, 0x00, 0x06, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        assert_eq!(parse_stream(&data, &mut info), data.len());
        assert!(info.has_model_id);
        assert_eq!(info.model_id, 0x1A);
        assert!(info.has_le_address);
        assert_eq!(info.le_address, 0xAABBCCDDEEFF);
    }

    #[test]
    fn stream_keeps_a_partial_trailing_message() {
        let mut info = StreamInfo::default();
        let data = [0x03, 0x02, 0x00, 0x06, 0xAA, 0xBB]; // address, half received
        assert_eq!(parse_stream(&data, &mut info), 0);
        assert!(!info.has_le_address);
        // Same buffer with the rest appended.
        let full = [0x03, 0x02, 0x00, 0x06, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        assert_eq!(parse_stream(&full, &mut info), full.len());
        assert_eq!(info.le_address, 0xAABBCCDDEEFF);
    }
}
