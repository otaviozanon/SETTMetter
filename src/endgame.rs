//! Endgame Gear wireless mice: the shared 4K dongle (OP1w 4k, XM2w 4k v1/v2) and the XM2we on
//! its older 2.4 GHz receiver.
//!
//! 4K dongle 3367:1970, protocol from johanneszab/vole (re/PROTOCOL.md, verified on OP1w 4k v1
//! and v2) and Janlhin/Endgame_Gear_Web_Configurator: the config channel is vendor collection
//! FF01:0002 with 64-byte feature report 0xA1, `A1 <cmd> <target> <len> 00 …`, payload from byte
//! 16. The reply is read after ~150 ms with status in byte 1: 0x01 ok, 0x03 busy (read again),
//! 0x08 / 0x11 mouse asleep or out of range. Command 0xB4 answers the battery percent in byte 16;
//! 0x0E the mouse's own info (bytes 16-17 = 67 33, its pid in 18-19 little endian); 0x0F is a
//! probe sent only after a failed command. No charging flag is decoded for this family.
//!
//! XM2we 3367:1961, from HaloBattery (MIT) and okms/xm2we-linux (PROTOCOL.md): a 17-byte feature
//! report 0x08 on FF02:0002, `08 <op> 00 … <checksum>` with all 17 bytes summing to 0x55. The
//! receiver answers on input report 0x09 of FF01:0000: `09 <op> 00 00 00 <size> <payload>
//! <checksum>`. Op 0x04 is the battery ([percent][charging]), op 0x03 the radio link (1 = linked).
//!
//! Only these read-only status queries are sent; no configuration, reset or re-pair commands.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::checksum55 as checksum;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::time::{Duration, Instant};

const VID: u16 = 0x3367;
const DONGLE_4K: u16 = 0x1970;
const RECEIVER_WE: u16 = 0x1961;

const REPORT_4K: u8 = 0xA1;
const BATTERY_4K: u8 = 0xB4;
const INFO_4K: u8 = 0x0E;
const PROBE_4K: u8 = 0x0F;
const LEN_4K: usize = 64;
const BUSY: u8 = 0x03;

const REPORT_WE: u8 = 0x08;
const REPLY_WE: u8 = 0x09;
const BATTERY_WE: u8 = 0x04;
const LINK_WE: u8 = 0x03;
const LEN_WE: usize = 17;
const REPLY_TIMEOUT: Duration = Duration::from_millis(400);

#[derive(Default)]
pub struct Endgame {
    /// The 4K mouse only identifies itself while awake; keep the last name per key.
    names: HashMap<String, String>,
    seen: HashSet<String>,
}

impl Endgame {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        self.scan_4k(api, &mut out);
        self.scan_we(api, &mut out);
        out
    }

    fn scan_4k(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        let key = format!("endgame:{DONGLE_4K:04x}");
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == DONGLE_4K && d.usage_page() == 0xFF01 && d.usage() == 0x0002
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::feature_len(&desc, REPORT_4K).is_none_or(|n| n < LEN_4K) {
                continue;
            }
            let Some(mut r) = command(&dev, BATTERY_4K, 0x00) else { continue };
            if r[1] != 0x01 {
                // After a failed command the dongle stays silent until a probe gets through.
                if command(&dev, PROBE_4K, PROBE_4K).is_none_or(|p| p[1] != 0x01) {
                    let _ = command(&dev, PROBE_4K, 0x01);
                }
                let Some(retry) = command(&dev, BATTERY_4K, 0x00) else { continue };
                r = retry;
            }
            let awake = r[1] == 0x01 && r[16] <= 100;
            let asleep = r[1] == 0x08 || r[1] == 0x11;
            if !awake && !asleep {
                if self.seen.contains(&key) {
                    out.push(observation(&key, self.name(&key), State::Unreachable));
                }
                continue;
            }
            let state = if awake {
                if let Some(info) = command(&dev, INFO_4K, 0x00) {
                    if let Some(pid) = mouse_pid(&info) {
                        if let Some(name) = mouse_name(pid) {
                            self.names.insert(key.clone(), name.into());
                        }
                    }
                }
                State::Battery { percent: r[16], charging: false }
            } else {
                State::Unreachable
            };
            self.seen.insert(key.clone());
            out.push(observation(&key, self.name(&key), state));
            // One dongle, one key: don't list the same receiver twice.
            break;
        }
    }

    fn scan_we(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        let key = format!("endgame:{RECEIVER_WE:04x}");
        let feature = |d: &hidapi::DeviceInfo| {
            d.vendor_id() == VID && d.product_id() == RECEIVER_WE && d.usage_page() == 0xFF02 && d.usage() == 0x0002
        };
        let inputs: Vec<_> = api
            .device_list()
            .filter(|d| {
                d.vendor_id() == VID && d.product_id() == RECEIVER_WE && d.usage_page() == 0xFF01 && d.usage() == 0x0000
            })
            .collect();
        for f in api.device_list().filter(|d| feature(d)) {
            // A receiver has two collections, told apart only by the "&colNN" instance part.
            let Some(input) = inputs.iter().find(|i| receiver_key(i.path()) == receiver_key(f.path())) else {
                continue; // without the reply collection nothing is sent
            };
            let Ok(feat) = api.open_path(f.path()) else { continue };
            let Ok(reader) = api.open_path(input.path()) else { continue };
            let (Some(fdesc), Some(idesc)) = (probe::descriptor(&feat), probe::descriptor(&reader)) else { continue };
            if probe::feature_len(&fdesc, REPORT_WE).is_none_or(|n| n < LEN_WE)
                || probe::input_len(&idesc, REPLY_WE).is_none_or(|n| n < LEN_WE)
            {
                continue;
            }
            // Battery first: the link can drop while the charger goes in.
            let Ok(Some(battery)) = query_we(&feat, &reader, BATTERY_WE) else { continue };
            if battery[0] > 100 {
                continue;
            }
            let link = query_we(&feat, &reader, LINK_WE).ok().flatten();
            let charging = battery.len() > 1 && battery[1] == 1;
            // The receiver answered but the mouse isn't linked: keep the last reading dimmed.
            let state = if charging || link.is_some_and(|l| l[0] == 1) {
                State::Battery { percent: battery[0], charging }
            } else {
                State::Unreachable
            };
            self.seen.insert(key.clone());
            out.push(observation(&key, "Endgame Gear XM2we", state));
            return;
        }
        if self.seen.contains(&key) {
            out.push(observation(&key, "Endgame Gear XM2we", State::Unreachable));
        }
    }

    fn name(&self, key: &str) -> &str {
        self.names.get(key).map_or("Endgame Gear 4k wireless mouse", String::as_str)
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

/// Sends one 4K command and returns the reply; `None` on a bad read. Status is reply byte 1.
fn command(dev: &HidDevice, cmd: u8, target: u8) -> Option<[u8; LEN_4K]> {
    let mut req = [0u8; LEN_4K];
    req[0] = REPORT_4K;
    req[1] = cmd;
    req[2] = target;
    if let Err(e) = dev.send_feature_report(&req) {
        diag::event("endgame:4k", format!("i/o failed: {e}"));
        return None;
    }
    for attempt in 0..3 {
        std::thread::sleep(Duration::from_millis(150 + attempt * 100));
        let mut buf = [0u8; LEN_4K];
        buf[0] = REPORT_4K;
        match dev.get_feature_report(&mut buf) {
            Ok(n) if n >= 20 && buf[0] == REPORT_4K => {
                if buf[1] != BUSY {
                    return Some(buf);
                }
            }
            Ok(_) => return None,
            Err(e) => {
                diag::event("endgame:4k", format!("i/o failed: {e}"));
                return None;
            }
        }
    }
    Some([BUSY; LEN_4K]) // still busy after three reads
}

/// The mouse's own USB product id from a 0x0E info reply, when it reports Endgame Gear's vendor id.
fn mouse_pid(info: &[u8]) -> Option<u16> {
    (info.len() >= 20 && info[16] == 0x67 && info[17] == 0x33).then(|| u16::from_le_bytes([info[18], info[19]]))
}

fn mouse_name(pid: u16) -> Option<&'static str> {
    let name = match pid {
        0x1972 => "Endgame Gear OP1w 4k",
        0x1984 => "Endgame Gear OP1w 4k v2",
        0x1968 => "Endgame Gear XM2w 4k",
        0x1982 => "Endgame Gear XM2w 4k v2",
        _ => return None,
    };
    Some(name)
}

/// The receiver's two collections differ only in "&colNN" and the instance's last "&NNNN".
fn receiver_key(path: &CStr) -> String {
    let p = path.to_string_lossy().to_ascii_lowercase();
    let parts: Vec<&str> = p.split('#').collect();
    if parts.len() < 4 {
        return p;
    }
    let mut hw = parts[1].to_string();
    if let Some(i) = hw.find("&col") {
        hw.replace_range(i..(i + 6).min(hw.len()), "");
    }
    let inst = parts[2].rfind('&').map_or(parts[2], |i| &parts[2][..i]);
    format!("{hw}#{inst}")
}

/// Sends one 17-byte WE frame and waits for its input reply.
fn query_we(feat: &HidDevice, reader: &HidDevice, op: u8) -> hidapi::HidResult<Option<Vec<u8>>> {
    let mut buf = [0u8; 64];
    // A frame queued before the request isn't its answer.
    while let Ok(n) = reader.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    let mut frame = [0u8; LEN_WE];
    frame[0] = REPORT_WE;
    frame[1] = op;
    frame[LEN_WE - 1] = checksum(&frame[..LEN_WE - 1]);
    feat.send_feature_report(&frame)?;

    let deadline = Instant::now() + REPLY_TIMEOUT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match reader.read_timeout(&mut buf, left.as_millis() as i32)? {
            0 => break,
            n => {
                if let Some(p) = payload(&buf[..n], op) {
                    return Ok(Some(p));
                }
            }
        }
    }
    Ok(None)
}

/// `09 <op> 00 00 00 <size> <payload> …`, all 17 bytes summing to 0x55.
fn payload(r: &[u8], op: u8) -> Option<Vec<u8>> {
    if r.len() < LEN_WE || r[0] != REPLY_WE || r[1] != op || r[2] != 0 || r[3] != 0 || r[4] != 0 {
        return None;
    }
    if checksum(&r[..LEN_WE]) != 0 {
        return None;
    }
    let size = r[5] as usize;
    if !(1..=10).contains(&size) {
        return None;
    }
    Some(r[6..6 + size].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn we_frame_checksum() {
        let mut frame = [0u8; LEN_WE];
        frame[0] = REPORT_WE;
        frame[1] = BATTERY_WE;
        assert_eq!(checksum(&frame[..LEN_WE - 1]), 0x49);
        frame[LEN_WE - 1] = 0x49;
        assert_eq!(frame.iter().fold(0u8, |s, b| s.wrapping_add(*b)), 0x55);
    }

    fn we_reply(op: u8, values: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; LEN_WE];
        r[0] = REPLY_WE;
        r[1] = op;
        r[5] = values.len() as u8;
        r[6..6 + values.len()].copy_from_slice(values);
        r[LEN_WE - 1] = checksum(&r[..LEN_WE - 1]);
        r
    }

    #[test]
    fn we_payload_parses_battery_and_link() {
        // XM2we on the charger at 78%: [percent][charging].
        assert_eq!(payload(&we_reply(BATTERY_WE, &[78, 1]), BATTERY_WE), Some(vec![78, 1]));
        assert_eq!(payload(&we_reply(LINK_WE, &[1]), LINK_WE), Some(vec![1]));
        assert_eq!(payload(&we_reply(BATTERY_WE, &[78, 1]), LINK_WE), None); // another op
        assert_eq!(payload(&we_reply(BATTERY_WE, &[])[..16], BATTERY_WE), None); // short
    }

    #[test]
    fn we_payload_rejects_bad_checksum_and_size() {
        let mut bad = we_reply(BATTERY_WE, &[78, 1]);
        bad[16] = bad[16].wrapping_add(1);
        assert_eq!(payload(&bad, BATTERY_WE), None);

        let mut empty = we_reply(BATTERY_WE, &[0; 1]);
        empty[5] = 0;
        empty[16] = checksum(&empty[..16]);
        assert_eq!(payload(&empty, BATTERY_WE), None);
    }

    #[test]
    fn reads_the_mouse_identity_from_the_info_reply() {
        let mut info = [0u8; LEN_4K];
        info[16] = 0x67;
        info[17] = 0x33;
        info[18..20].copy_from_slice(&0x1984u16.to_le_bytes());
        assert_eq!(mouse_pid(&info), Some(0x1984));
        assert_eq!(mouse_name(0x1984), Some("Endgame Gear OP1w 4k v2"));
        assert_eq!(mouse_name(0x1968), Some("Endgame Gear XM2w 4k"));
        // Another vendor id on the same dongle: no name.
        info[16] = 0x25;
        assert_eq!(mouse_pid(&info), None);
    }

    #[test]
    fn pairs_the_receiver_collections() {
        let a = CStr::from_bytes_with_nul(b"\\\\?\\hid#vid_3367&pid_1961&mi_02&col01#7&1a2b&0&0000#{4d1e55b2}\0").unwrap();
        let b = CStr::from_bytes_with_nul(b"\\\\?\\hid#vid_3367&pid_1961&mi_02&col03#7&1a2b&0&0002#{4d1e55b2}\0").unwrap();
        assert_eq!(receiver_key(a), receiver_key(b));
    }
}
