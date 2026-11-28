//! SONiX keyboards on their 2.4 GHz dongle: AJAZZ AK820 MAX/AK980, AULA F108 Pro, EPOMAKER
//! P75/Galaxy100 and many OEM boards.
//!
//! The dongle exposes a 32-byte vendor report on FF60:0061 (unnumbered, report id 0):
//! request `20 01 00 … <sum>` with the sum of the 31 bytes before it, reply `20 01 00 <level>`
//! with the level in percent. The echoed command is the only reply check; the checksum is not
//! verified, as the AK820 tools do. The keyboard only answers in 2.4 GHz mode.
//!
//! Table and protocol from Wireless-Device-Battery-Monitor (drivers/sonix.cpp), itself from
//! sonix-keyboard-studio, ak820-rs and ak820-studio. Read-only.
//!
//! The Lofree Flow 2 (388D:0002) needs a custom firmware image to answer this query at all and
//! has its own signed reply; it is not covered here. MCHOSE K87 receivers share 05AC:024F but
//! expose a different collection (handled by aula.cpp upstream) and are skipped by the FF60
//! match.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::drain_ok as flush;
use crate::hidutil::sum as checksum;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const PAYLOAD: usize = 32;
const REPORT_LEN: usize = PAYLOAD + 1;
const REPLY_TIMEOUT: Duration = Duration::from_millis(500);

struct Dongle {
    vid: u16,
    pid: u16,
    /// None: match the vendor collection on any interface.
    iface: Option<i32>,
}

const DONGLES: &[Dongle] = &[
    Dongle { vid: 0x05AC, pid: 0x024F, iface: Some(3) },
    Dongle { vid: 0x0C45, pid: 0xFEFE, iface: Some(3) },
    Dongle { vid: 0x0C45, pid: 0xFDFD, iface: Some(3) },
    Dongle { vid: 0x342D, pid: 0xE479, iface: None }, // EPOMAKER P75
    Dongle { vid: 0x342D, pid: 0xE411, iface: Some(1) }, // RSQ_20055_2.4G
    Dongle { vid: 0x342D, pid: 0xE489, iface: Some(2) }, // EPOMAKER Galaxy100
    Dongle { vid: 0x342D, pid: 0xE485, iface: None }, // RK Royal Kludge R75
    Dongle { vid: 0x342D, pid: 0xE4BC, iface: None }, // Redragon Arlokks K742B
    Dongle { vid: 0x342D, pid: 0xE4C2, iface: None }, // M1 V5 VIA
    Dongle { vid: 0x342D, pid: 0xE407, iface: None }, // Lucky65V2
    Dongle { vid: 0x388D, pid: 0x0003, iface: None }, // Lofree Flow2
    Dongle { vid: 0x0DB0, pid: 0x0CA0, iface: None }, // MSI FORGE TKL W FRIEREN EDITION
    Dongle { vid: 0x36B0, pid: 0x30BD, iface: None }, // Essential75
    Dongle { vid: 0x342D, pid: 0xE51A, iface: None }, // NUT65
    Dongle { vid: 0x342D, pid: 0xE487, iface: None }, // RK 2.4G Dongle
    Dongle { vid: 0x342D, pid: 0xE537, iface: None }, // WEIKAV MAX102
    Dongle { vid: 0x19F5, pid: 0x3245, iface: None }, // NuPhy Air75V2
    Dongle { vid: 0x362D, pid: 0xD028, iface: None }, // SKN Qinglong
    Dongle { vid: 0x362D, pid: 0xD030, iface: None }, // Lemokey Link
    Dongle { vid: 0x362D, pid: 0xD101, iface: None }, // Wireless Link
    Dongle { vid: 0x342D, pid: 0xE49F, iface: None }, // Spade65-QMK-2.4GHz
    Dongle { vid: 0x0DB0, pid: 0x8A8F, iface: None }, // MSI FORGE GK600 TKL WIRELESS
    Dongle { vid: 0x373B, pid: 0x111F, iface: None }, // ATK A87Pro
    Dongle { vid: 0x55D4, pid: 0x0664, iface: None }, // W-SOFLE keyboard
    Dongle { vid: 0xFFFE, pid: 0x003D, iface: None }, // CM81 Zeno
    Dongle { vid: 0xFFFE, pid: 0x0047, iface: None }, // Vortex NS75
    Dongle { vid: 0x373B, pid: 0x1053, iface: None }, // MAD60
    Dongle { vid: 0x6D66, pid: 0x8888, iface: None }, // Zoom 75
    Dongle { vid: 0x373B, pid: 0x1151, iface: None }, // ATK A98 Pro
    Dongle { vid: 0x373B, pid: 0x1193, iface: None }, // REKT Hiro 60
    Dongle { vid: 0x373B, pid: 0x11FF, iface: None }, // ATK YOGO 75 PRO
    Dongle { vid: 0x6D66, pid: 0x8572, iface: None }, // Meletrix Zoom64
    Dongle { vid: 0x36B0, pid: 0x3122, iface: None }, // CIDOO QK104PRO
    Dongle { vid: 0x342D, pid: 0xE4C5, iface: None }, // M1 V5 VIA 2.4G KB
    Dongle { vid: 0x342D, pid: 0xE471, iface: None }, // EPOMAKER TIDE 75
    Dongle { vid: 0x19F5, pid: 0x32F5, iface: None }, // NuPhy Halo75 V2
    Dongle { vid: 0x36B0, pid: 0x3035, iface: None }, // CIDOO QK61
    Dongle { vid: 0x36B0, pid: 0x3005, iface: None }, // Linky87
    Dongle { vid: 0x342D, pid: 0xEA80, iface: None }, // IO Custom 68 2.4G
    Dongle { vid: 0x36B0, pid: 0x2003, iface: None }, // wireless 2.4G Dongle
    Dongle { vid: 0x362D, pid: 0xD107, iface: None }, // JZM5
];

#[derive(Default)]
pub struct Sonix;

impl Sonix {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        let mut done = HashSet::new();
        for m in DONGLES {
            if !done.insert((m.vid, m.pid)) {
                continue;
            }
            let mut found: Option<(&str, Option<u8>)> = None;
            for info in api.device_list().filter(|d| {
                d.vendor_id() == m.vid
                    && d.product_id() == m.pid
                    && m.iface.is_none_or(|iface| d.interface_number() == iface)
                    && d.usage_page() == 0xFF60
                    && d.usage() == 0x0061
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                // Unnumbered: 32 payload bytes, the report id 0 is implicit.
                if probe::output_len(&desc, 0) != Some(PAYLOAD) || probe::input_len(&desc, 0) != Some(PAYLOAD) {
                    continue;
                }
                let key = format!("sonix:{:04x}:{:04x}", m.vid, m.pid);
                // The first request after idle can be lost while the link wakes up.
                let mut level = None;
                for attempt in 0..2 {
                    match query_level(&dev) {
                        Ok(v) => {
                            level = v;
                            if v.is_some() {
                                break;
                            }
                        }
                        Err(e) => {
                            diag::event(&key, format!("i/o failed: {e}"));
                            level = None;
                            if attempt == 0 {
                                continue;
                            }
                            break;
                        }
                    }
                }
                found = Some((info.product_string().unwrap_or(""), level));
                if level.is_some() {
                    break;
                }
            }

            let Some((product, level)) = found else { continue };
            let key = format!("sonix:{:04x}:{:04x}", m.vid, m.pid);
            let state = match level {
                Some(0) => State::Off, // off, out of range or in another mode
                Some(level @ 1..=100) => State::Battery { percent: level, charging: false },
                _ => State::Unreachable,
            };
            diag::event(&key, format!("battery via report 20 01: {state:?}"));
            out.push(Observation {
                id: key,
                name: Some(keyboard_name(product)),
                kind: Some(Kind::Keyboard),
                state,
            });
        }
        out
    }
}

/// "AJAZZ AK820 MAX 2.4G" -> "AJAZZ AK820 MAX"; an empty product gets a generic name.
fn keyboard_name(product: &str) -> String {
    let name = product.trim_end().strip_suffix(" 2.4G").unwrap_or(product.trim_end());
    if name.is_empty() { "Wireless keyboard".into() } else { name.into() }
}

fn query_level(dev: &HidDevice) -> hidapi::HidResult<Option<u8>> {
    let mut req = [0u8; REPORT_LEN];
    req[1] = 0x20;
    req[2] = 0x01;
    req[PAYLOAD] = checksum(&req[1..PAYLOAD]);

    flush(dev)?;
    dev.write(&req)?;

    let deadline = Instant::now() + REPLY_TIMEOUT;
    let mut buf = [0u8; REPORT_LEN];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let ms = (left.as_millis() as i32).clamp(1, 100);
        match dev.read_timeout(&mut buf, ms) {
            Ok(0) => {}
            Ok(n) => {
                if let Some(level) = parse(&buf[..n]) {
                    return Ok(Some(level));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// The reply echoes the first three payload bytes; the level follows.
fn parse(r: &[u8]) -> Option<u8> {
    if r.len() < 4 || r[0] != 0x20 || r[1] != 0x01 || r[2] != 0x00 {
        return None;
    }
    Some(r[3])
}

/// Sum of the request bytes before the checksum byte.


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_checksum_of_the_20_01_query() {
        let mut req = [0u8; REPORT_LEN];
        req[1] = 0x20;
        req[2] = 0x01;
        req[PAYLOAD] = checksum(&req[1..PAYLOAD]);
        // 0x20 + 0x01
        assert_eq!(req[PAYLOAD], 0x21);
    }

    #[test]
    fn parses_the_echoed_level() {
        // VGN receiver read 93% this way (myfinder-device-battery).
        assert_eq!(parse(&[0x20, 0x01, 0x00, 0x5D]), Some(93));
        assert_eq!(parse(&[0x20, 0x01, 0x00, 0x00]), Some(0));
        assert_eq!(parse(&[0x20, 0x02, 0x00, 0x5D]), None);
        assert_eq!(parse(&[0x20, 0x01, 0x00]), None);
        assert_eq!(parse(&[]), None);
    }

    #[test]
    fn names_drop_the_link_suffix() {
        assert_eq!(keyboard_name("AJAZZ AK820 MAX 2.4G"), "AJAZZ AK820 MAX");
        assert_eq!(keyboard_name("EPOMAKER P75"), "EPOMAKER P75");
        assert_eq!(keyboard_name("  "), "Wireless keyboard");
    }
}
