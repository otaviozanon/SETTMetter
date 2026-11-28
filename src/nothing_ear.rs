//! Nothing / CMF wireless earbuds (Ear (3a), Ear (a), Ear (2), Ear, CMF Buds ...).
//!
//! Windows exposes no battery level for them over Classic Bluetooth, so upstream queries the
//! NT Link RFCOMM service ({aeac4a03-dff5-498f-843a-34487cf133eb}) with the read-only 0xC007
//! battery command. Only paired devices that Windows reports as connected and whose name matches
//! Nothing/CMF earbuds are tried; if another app holds the channel they are skipped.
//!
//! RFCOMM needs winsock's AF_BTH (Cargo feature `Win32_Networking_WinSock`) or WinRT
//! `RfcommDeviceService` (`Devices_Bluetooth_Rfcomm`); neither is enabled here, so `scan` reports
//! nothing. The frame decoder is ported and tested.
//!
//! Frame:  55 60 01 [cmd lo][cmd hi] [payload len] 00 [seq] [payload...] [crc lo][crc hi]
//!   crc   = CRC-16/MODBUS (init 0xFFFF, reflected poly 0xA001) over bytes [0 .. 8+len-1], LE
//!   query = cmd 0xC007, payload len 0 (read-only battery request)
//!   reply = cmd 0xC007 (or 0xE001 push): payload[0] = device count N, then N pairs (id, status)
//!           id 0x02 left, 0x03 right, 0x04 case; status bit 7 charging, bits 0..6 percent.
//! From the reverse-engineered Nothing/CMF protocol, as in upstream nothing_ear_protocol.h.
use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::crc16_modbus;
use crate::soundcore::rfcomm::Connection;
use std::time::{Duration, Instant};
use windows_sys::core::GUID;

/// NT Link service the battery query goes to.
pub const NT_LINK: u128 = 0xaeac4a03_dff5_498f_843a_34487cf133eb;
pub const CMD_BATTERY: u16 = 0xC007;
pub const CMD_BATTERY_PUSH: u16 = 0xE001;

pub fn looks_like_nothing_ear(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    (n.contains("nothing") && (n.contains("ear") || n.contains("buds")))
        || n.contains("ear (")
        || n.contains("ear 3a")
        || n.contains("cmf buds")
}

#[derive(Default)]
pub struct NothingEar;

impl NothingEar {
    pub fn scan(&mut self) -> Vec<Observation> {
        let mut out = Vec::new();
        for node in crate::soundcore::bt_nodes() {
            let Some(addr) = node.id_address else { continue };
            if !node.connected || !looks_like_nothing_ear(&node.name) {
                continue;
            }
            let Some(status) = read_status(addr) else { continue };
            let percent = match (status.left, status.right) {
                (Some(l), Some(r)) => Some(l.min(r)),
                (left, right) => left.or(right),
            };
            let Some(percent) = percent else { continue };
            diag::event(
                &format!("nothingear:{addr:012x}"),
                format!("L {:?}, R {:?}, case {:?}", status.left, status.right, status.case),
            );
            out.push(Observation {
                id: format!("nothingear:{addr:012x}"),
                name: Some(node.name),
                kind: Some(Kind::Headset),
                state: State::Battery { percent, charging: status.left_charging || status.right_charging },
            });
        }
        out
    }
}

fn read_status(addr: u64) -> Option<Status> {
    let conn = Connection::open(addr, GUID::from_u128(NT_LINK))?;
    conn.send_all(&build_battery_query(1)).then_some(())?;
    let deadline = Instant::now() + Duration::from_millis(2000);
    let mut buf = Vec::new();
    while buf.len() < 4096 {
        let left = deadline.checked_duration_since(Instant::now())?;
        if !conn.wait_readable(left) {
            break;
        }
        let mut chunk = [0u8; 512];
        let Some(n) = conn.recv(&mut chunk) else { break };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(status) = find_status(&buf) {
            return Some(status);
        }
    }
    None
}

fn build_battery_query(seq: u8) -> [u8; 10] {
    let mut f = [0x55, 0x60, 0x01, 0x07, 0xC0, 0x00, 0x00, seq, 0x00, 0x00];
    let crc = crc16_modbus(&f[..8]);
    f[8] = crc as u8;
    f[9] = (crc >> 8) as u8;
    f
}

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Status {
    pub left: Option<u8>,
    pub right: Option<u8>,
    pub case: Option<u8>,
    pub left_charging: bool,
    pub right_charging: bool,
    pub case_charging: bool,
}



/// Scans a received byte stream for a battery reply, keeping the levels of the last valid one.
pub fn find_status(data: &[u8]) -> Option<Status> {
    let mut out = None;
    let mut i = 0;
    while i + 10 <= data.len() {
        if data[i] != 0x55 || data[i + 1] != 0x60 || data[i + 2] != 0x01 {
            i += 1;
            continue;
        }
        let plen = data[i + 5] as usize;
        let total = 8 + plen + 2;
        if i + total > data.len() {
            i += 1;
            continue;
        }
        let crc = u16::from_le_bytes([data[i + 8 + plen], data[i + 9 + plen]]);
        if crc16_modbus(&data[i..i + 8 + plen]) != crc {
            i += 1;
            continue;
        }
        let cmd = u16::from_le_bytes([data[i + 3], data[i + 4]]);
        if cmd != CMD_BATTERY && cmd != CMD_BATTERY_PUSH {
            i += total;
            continue;
        }
        let p = &data[i + 8..i + 8 + plen];
        if plen < 1 {
            i += 1;
            continue;
        }
        let count = p[0] as usize;
        if 1 + count * 2 > plen {
            i += 1;
            continue;
        }
        let mut s = Status::default();
        let mut any = false;
        for k in 0..count {
            let (id, st) = (p[1 + k * 2], p[2 + k * 2]);
            let percent = st & 0x7F;
            if percent > 100 {
                continue;
            }
            let charging = st & 0x80 != 0;
            match id {
                0x02 => {
                    s.left = Some(percent);
                    s.left_charging = charging;
                    any = true;
                }
                0x03 => {
                    s.right = Some(percent);
                    s.right_charging = charging;
                    any = true;
                }
                0x04 => {
                    s.case = Some(percent);
                    s.case_charging = charging;
                    any = true;
                }
                _ => {}
            }
        }
        if any {
            out = Some(s);
        }
        i += total;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_reference_check_value() {
        assert_eq!(crc16_modbus(b"123456789"), 0x4B37);
    }

    #[test]
    fn parses_a_battery_reply() {
        // 55 60 01 07 C0 07 00 01 | count 3: left 85, right 75, case 100 charging | 82 88
        let frame = [0x55, 0x60, 0x01, 0x07, 0xC0, 0x07, 0x00, 0x01, 0x03, 0x02, 0x55, 0x03, 0x4B, 0x04, 0xE4, 0x82, 0x88];
        let st = find_status(&frame).unwrap();
        assert_eq!((st.left, st.right, st.case), (Some(85), Some(75), Some(100)));
        assert!(!st.left_charging && !st.right_charging && st.case_charging);
    }

    #[test]
    fn parses_a_push_frame_with_two_components() {
        // 55 60 01 01 E0 07 00 02 | count 2: left 99, right 75, case 100 charging | A9 51
        let frame = [0x55, 0x60, 0x01, 0x01, 0xE0, 0x07, 0x00, 0x02, 0x03, 0x02, 0x63, 0x03, 0x4B, 0x04, 0xE4, 0xA9, 0x51];
        let st = find_status(&frame).unwrap();
        assert_eq!((st.left, st.right, st.case), (Some(99), Some(75), Some(100)));
    }

    #[test]
    fn rejects_bad_crc_count_and_foreign_commands() {
        let frame = [0x55, 0x60, 0x01, 0x07, 0xC0, 0x07, 0x00, 0x01, 0x03, 0x02, 0x55, 0x03, 0x4B, 0x04, 0xE4, 0x82, 0x88];

        let mut bad = frame;
        bad[10] ^= 0xFF;
        assert!(find_status(&bad).is_none());

        // Count claims more pairs than the payload holds.
        let mut bad = frame;
        bad[8] = 0x09;
        let crc = crc16_modbus(&bad[..15]).to_le_bytes();
        bad[15..17].copy_from_slice(&crc);
        assert!(find_status(&bad).is_none());

        // Some other read-only command: skipped, no status.
        let mut other = frame;
        other[3..5].copy_from_slice(&0xC006u16.to_le_bytes());
        let crc = crc16_modbus(&other[..15]).to_le_bytes();
        other[15..17].copy_from_slice(&crc);
        assert!(find_status(&other).is_none());
    }

    #[test]
    fn ignores_out_of_range_percentages() {
        // count 1, id left, 0x7F percent = 127 (unknown).
        let mut frame = Vec::from([0x55, 0x60, 0x01, 0x07, 0xC0, 0x03, 0x00, 0x01, 0x01, 0x02, 0x7F, 0x00, 0x00]);
        let crc = crc16_modbus(&frame[..11]);
        frame[11..13].copy_from_slice(&crc.to_le_bytes());
        assert!(find_status(&frame).is_none());
    }

    #[test]
    fn finds_a_reply_behind_garbage() {
        let frame = [0x55, 0x60, 0x01, 0x07, 0xC0, 0x07, 0x00, 0x01, 0x03, 0x02, 0x55, 0x03, 0x4B, 0x04, 0xE4, 0x82, 0x88];
        let mut data = vec![0xAA, 0xBB, 0x55];
        data.extend(frame);
        assert_eq!(find_status(&data).map(|s| s.right), Some(Some(75)));
    }

    #[test]
    fn name_matching() {
        assert!(looks_like_nothing_ear("Nothing Ear (a)"));
        assert!(looks_like_nothing_ear("CMF Buds Pro 2"));
        assert!(looks_like_nothing_ear("Ear 3a"));
        assert!(!looks_like_nothing_ear("Nothing Phone (2)"));
    }
}
