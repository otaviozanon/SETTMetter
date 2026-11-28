//! Samsung Galaxy Buds (Buds, Buds+, Live, Pro, Buds2, Buds2 Pro, FE, Core, Buds3 ...).
//!
//! Windows shows no battery level for them, so upstream asks over Bluetooth RFCOMM: the earbuds
//! accept a connection on Samsung's SPP service and push a status frame as soon as it opens.
//! Nothing is sent to them.
//!   Buds2 and later:  {2e73a4ad-332d-41fc-90e2-16bef06523f2}
//!   Buds+, Live, Pro: standard SPP {00001101-0000-1000-8000-00805f9b34fb}
//!   original Buds:    {00001102-0000-1000-8000-00805f9b34fd}, legacy framing
//!
//! RFCOMM needs either winsock's AF_BTH (Cargo feature `Win32_Networking_WinSock`) or WinRT
//! `RfcommDeviceService` (`Devices_Bluetooth_Rfcomm`); neither is enabled here, so `scan` reports
//! nothing. The frame decoder is ported and tested.
//!
//! Frame (Buds+ and later):  FD [size lo][size hi | flags] [id] [payload...] [crc lo][crc hi] DD
//! Frame (original Buds):    FE [type] [size] [id] [payload...] [crc lo][crc hi] EE
//!   size = id + payload + crc (low 10 bits of the header); header bit 0x2000 = firmware fragment
//!   crc  = CRC-16/XMODEM (poly 0x1021, init 0) over id + payload, little-endian
//!   0x60 payload[1] left %, [2] right %, [6] case %; 0x61 payload[2], [3] and [7].
//!   0 means that part is not connected; Buds+ report 101 for an unknown case.
//! From GalaxyBudsClient, as in upstream galaxy_buds_protocol.h.
use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::crc16_xmodem;
use crate::soundcore::rfcomm::Connection;
use std::time::{Duration, Instant};
use windows_sys::core::GUID;

/// Samsung's SPP service for Buds2 and later, standard SPP, and the original Buds' service.
pub const SPP_NEW: u128 = 0x2e73a4ad_332d_41fc_90e2_16bef06523f2;
pub const SPP_STANDARD: u128 = 0x00001101_0000_1000_8000_0080_5f9b34fb;
pub const SPP_LEGACY: u128 = 0x00001102_0000_1000_8000_0080_5f9b34fd;

pub const STATUS_UPDATED: u8 = 0x60;
pub const EXTENDED_STATUS_UPDATED: u8 = 0x61;

/// Only paired, connected devices whose name contains "Buds" are tried.
pub fn looks_like_buds(name: &str) -> bool {
    name.to_ascii_lowercase().contains("buds")
}

#[derive(Default)]
pub struct GalaxyBuds;

impl GalaxyBuds {
    pub fn scan(&mut self) -> Vec<Observation> {
        let mut out = Vec::new();
        for node in crate::soundcore::bt_nodes() {
            let Some(addr) = node.id_address else { continue };
            if !node.connected || !looks_like_buds(&node.name) {
                continue;
            }
            let found = [
                (SPP_NEW, false),
                (SPP_STANDARD, false),
                (SPP_LEGACY, true),
            ]
            .into_iter()
            .find_map(|(service, legacy)| read_status(addr, service, legacy));
            let Some(status) = found else { continue };
            let percent = match (status.left, status.right) {
                (Some(l), Some(r)) => Some(l.min(r)),
                (left, right) => left.or(right),
            };
            let Some(percent) = percent else { continue };
            diag::event(&format!("galaxybuds:{addr:012x}"), format!("L {:?}, R {:?}, case {:?}", status.left, status.right, status.case));
            out.push(Observation {
                id: format!("galaxybuds:{addr:012x}"),
                name: Some(node.name),
                kind: Some(Kind::Headset),
                state: State::Battery { percent, charging: false },
            });
        }
        out
    }
}

fn read_status(addr: u64, service: u128, legacy: bool) -> Option<Status> {
    let conn = Connection::open(addr, GUID::from_u128(service))?;
    let deadline = Instant::now() + Duration::from_millis(3000);
    let mut buf = Vec::new();
    while buf.len() < 8192 {
        let left = deadline.checked_duration_since(Instant::now())?;
        if !conn.wait_readable(left) {
            break;
        }
        let mut chunk = [0u8; 512];
        let Some(n) = conn.recv(&mut chunk) else { break };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(status) = find_status(&buf, legacy) {
            return Some(status);
        }
    }
    None
}

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Status {
    pub left: Option<u8>,
    pub right: Option<u8>,
    pub case: Option<u8>,
}



fn level(v: u8) -> Option<u8> {
    (1..=100).contains(&v).then_some(v)
}

/// Scans a received byte stream for status frames, keeping the levels of the last valid one.
pub fn find_status(data: &[u8], legacy: bool) -> Option<Status> {
    let (som, eom) = if legacy { (0xFE, 0xEE) } else { (0xFD, 0xDD) };
    let mut out = None;
    let mut i = 0;
    while i + 6 <= data.len() {
        if data[i] != som {
            i += 1;
            continue;
        }
        let len = if legacy {
            data[i + 2] as usize
        } else {
            let header = usize::from(data[i + 1]) | usize::from(data[i + 2]) << 8;
            if header & 0x2000 != 0 {
                i += 1; // firmware fragments carry no status
                continue;
            }
            header & 0x3FF
        };
        if len < 3 || i + 3 + len >= data.len() || data[i + 3 + len] != eom {
            i += 1;
            continue;
        }
        let body = &data[i + 3..i + 3 + len]; // id, payload, crc
        let crc = u16::from_le_bytes([body[len - 2], body[len - 1]]);
        if crc16_xmodem(&body[..len - 2]) != crc {
            i += 1;
            continue;
        }
        let p = &body[1..];
        let plen = len - 3;
        let status = match body[0] {
            EXTENDED_STATUS_UPDATED if plen >= 4 => Status {
                left: level(p[2]),
                right: level(p[3]),
                case: if !legacy && plen >= 8 { level(p[7]) } else { None },
            },
            STATUS_UPDATED if plen >= 3 => Status {
                left: level(p[1]),
                right: level(p[2]),
                case: if !legacy && plen >= 7 { level(p[6]) } else { None },
            },
            _ => {
                i += 1;
                continue;
            }
        };
        out = Some(status);
        i += 3 + len + 1; // past this frame's end marker
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_reference_check_value() {
        assert_eq!(crc16_xmodem(b"123456789"), 0x31C3);
    }

    #[test]
    fn parses_status_updated_frame() {
        // FD 0A 00 | 60 00 50 46 00 00 00 37 75 67 | DD: left 80, right 70, case 55.
        let frame = [0xFD, 0x0A, 0x00, 0x60, 0x00, 0x50, 0x46, 0x00, 0x00, 0x00, 0x37, 0x75, 0x67, 0xDD];
        let st = find_status(&frame, false).unwrap();
        assert_eq!((st.left, st.right, st.case), (Some(80), Some(70), Some(55)));
    }

    #[test]
    fn parses_extended_status_updated_frame() {
        // FD 0B 00 | 61 00 00 2C 58 00 00 00 21 B1 0E | DD: left 44, right 88, case 33.
        let frame = [0xFD, 0x0B, 0x00, 0x61, 0x00, 0x00, 0x2C, 0x58, 0x00, 0x00, 0x00, 0x21, 0xB1, 0x0E, 0xDD];
        let st = find_status(&frame, false).unwrap();
        assert_eq!((st.left, st.right, st.case), (Some(44), Some(88), Some(33)));
    }

    #[test]
    fn legacy_framing_has_no_case_level() {
        // FE 00 0A | 60 00 50 46 00 00 00 37 75 67 | EE
        let frame = [0xFE, 0x00, 0x0A, 0x60, 0x00, 0x50, 0x46, 0x00, 0x00, 0x00, 0x37, 0x75, 0x67, 0xEE];
        let st = find_status(&frame, true).unwrap();
        assert_eq!((st.left, st.right, st.case), (Some(80), Some(70), None));
        // Read with the modern framing: not a frame at all.
        assert!(find_status(&frame, false).is_none());
    }

    #[test]
    fn rejects_bad_crc_and_zero_levels() {
        let mut frame = [0xFD, 0x0A, 0x00, 0x60, 0x00, 0x50, 0x46, 0x00, 0x00, 0x00, 0x37, 0x75, 0x67, 0xDD];
        frame[10] ^= 0xFF;
        assert!(find_status(&frame, false).is_none());

        // Level 0 = that part is not connected.
        let mut frame = [0xFD, 0x0A, 0x00, 0x60, 0x00, 0x50, 0x46, 0x00, 0x00, 0x00, 0x37, 0x75, 0x67, 0xDD];
        frame[5] = 0x00;
        let crc = crc16_xmodem(&frame[3..11]).to_le_bytes();
        frame[11..13].copy_from_slice(&crc);
        let st = find_status(&frame, false).unwrap();
        assert_eq!((st.left, st.right, st.case), (None, Some(70), Some(55)));
    }

    #[test]
    fn finds_a_frame_behind_garbage() {
        let frame = [0xFD, 0x0A, 0x00, 0x60, 0x00, 0x50, 0x46, 0x00, 0x00, 0x00, 0x37, 0x75, 0x67, 0xDD];
        let mut data = vec![0x00, 0x11, 0x22, 0xFD];
        data.extend(frame);
        assert_eq!(find_status(&data, false).map(|s| s.left), Some(Some(80)));
    }

    #[test]
    fn name_matching() {
        assert!(looks_like_buds("Galaxy Buds2 Pro"));
        assert!(looks_like_buds("buds live"));
        assert!(!looks_like_buds("Samsung TV"));
    }
}
