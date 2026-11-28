//! Rongyuan 2.4 GHz receivers used by Attack Shark, AJAZZ, MonsGeek, Akko and other mice and
//! keyboards (and some MSI devices).
//!
//! One feature report returns both slots, on interface 2, vendor collection FFFF:0002:
//! request `00 F7 00 …`, reply `[00][ok][keyboard %][mouse %][keyboard offline][mouse offline]…`.
//! A level is only shown for a slot whose offline flag is zero.
//!
//! AJAZZ-vendor receivers (VID 3151) can additionally name the paired mouse from its 32-bit
//! device id, read the way the vendor web driver does: route the next command to the mouse slot
//! (`F6 05`), poll the status until ready (`F7`), GET_INFO (`8F 00…` with complement checksum),
//! poll again, then fetch the answer (`FC`). F6 only picks which paired device the receiver
//! forwards to; nothing is written to the mouse.
//!
//! Receiver list and protocol from Wireless-Device-Battery-Monitor (drivers/rongyuan.cpp),
//! itself from BatteryTray and the official ATTACK SHARK web driver. Read-only.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::time::Duration;

const VID: u16 = 0x3151;
/// The receiver's status report is shorter than 7 bytes on some clones; the vendor checks 7.
const MIN_FEATURE: usize = 6;
const MAX_ID_TRIES: u8 = 3;

/// (vendor id, product id) of every known receiver.
const RECEIVERS: &[(u16, u16)] = &[
    (0x0DB0, 0x197E),
    (0x0DB0, 0x8E9A),
    (0x145F, 0x0376),
    (0x145F, 0x038D),
    (0x14A5, 0xC045),
    (0x25AA, 0x2008),
    (0x25AA, 0x4008),
    (0x25AA, 0x4018),
    (0x3151, 0x4011),
    (0x3151, 0x4014),
    (0x3151, 0x4017),
    (0x3151, 0x4021),
    (0x3151, 0x4027),
    (0x3151, 0x402D),
    (0x3151, 0x4038),
    (0x3151, 0x5006),
    (0x3151, 0x5007),
    (0x3151, 0x5008),
    (0x3151, 0x5020),
    (0x3151, 0x5026),
    (0x3151, 0x502B),
    (0x3151, 0x5031),
    (0x3151, 0x5037),
    (0x3151, 0x5038),
    (0x3151, 0x503D),
    (0x3151, 0x503F),
    (0x3151, 0x5040),
    (0x3151, 0x5042),
    (0x3151, 0x5045),
    (0x3151, 0x504B),
    (0x3151, 0x504E),
    (0x3151, 0x5050),
    (0x3151, 0x5052),
    (0x3151, 0x5055),
    (0x3151, 0x505A),
    (0x342D, 0xE3E0),
    (0x342D, 0xE3E1),
    (0x342D, 0xE3E2),
    (0x342D, 0xE3E6),
    (0x342D, 0xE3E7),
    (0x342D, 0xE3E8),
    (0x342D, 0xE3F5),
    (0x342D, 0xE3F6),
    (0x342D, 0xE3F7),
    (0x342D, 0xE3FB),
    (0x342D, 0xE3FC),
    (0x342D, 0xE3FD),
    (0x347A, 0xA302),
    (0x347A, 0xA305),
    (0x374A, 0xA205),
    (0x374A, 0xA217),
    (0x374A, 0xA220),
    (0x374A, 0xA223),
    (0x374A, 0xA226),
    (0x374A, 0xA229),
    (0x374A, 0xA234),
    (0x374A, 0xA237),
    (0x374A, 0xA251),
    (0x374A, 0xA263),
    (0x374A, 0xA268),
    (0x374A, 0xA311),
    (0x374A, 0xA314),
    (0x374A, 0xA319),
    (0x374A, 0xA322),
    (0x374A, 0xA329),
    (0x374A, 0xA355),
    (0x379A, 0x1804),
    (0x379A, 0x1807),
    (0x38A9, 0x0002),
    (0x38EE, 0x0000),
    (0x38EE, 0x0002),
    (0x38EE, 0x0039),
    (0x391D, 0x1A04),
    (0x391D, 0x1A08),
    (0x391D, 0x1A0E),
    (0x391D, 0x1A13),
    (0x391D, 0x1A25),
    (0x391D, 0x1A2B),
    (0x391D, 0x1A34),
    (0x391D, 0x2017),
    (0x391D, 0x2A05),
];

/// Model ids reported by AJAZZ-vendor receivers.
const AJAZZ_MICE: &[(u32, &str)] = &[
    (1754, "AJAZZ AJ159 PRO"),
    (3767, "AJAZZ AJ159 PRO"),
    (1755, "AJAZZ AJ159 APEX"),
    (3346, "AJAZZ AJ159 APEX"),
    (3806, "AJAZZ AJ159 APEX"),
    (3407, "AJAZZ AJ159P MC PRO"),
    (1851, "AJAZZ AJ179 PRO"),
    (3766, "AJAZZ AJ179 PRO"),
    (1852, "AJAZZ AJ179 APEX"),
    (3849, "AJAZZ AJ179 APEX"),
    (3441, "AJAZZ AJ179P MC PRO"),
];

#[derive(Default)]
pub struct Royuan {
    /// Mouse model id and attempt count per receiver path, so a mouse without the command costs
    /// at most three reads per connection.
    mouse_ids: HashMap<CString, (u32, u8)>,
}

impl Royuan {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let present: HashSet<CString> = api
            .device_list()
            .filter(|d| is_receiver(d.vendor_id(), d.product_id()))
            .map(|d| d.path().to_owned())
            .collect();
        self.mouse_ids.retain(|path, _| present.contains(path));

        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for info in api.device_list() {
            let (vid, pid) = (info.vendor_id(), info.product_id());
            if !is_receiver(vid, pid)
                || info.interface_number() != 2
                || info.usage_page() != 0xFFFF
                || info.usage() != 0x0002
                || !seen.insert((vid, pid))
            {
                continue;
            }
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            let Some(payload) = probe::feature_len(&desc, 0) else { continue };
            if payload < MIN_FEATURE {
                continue;
            }
            let len = payload + 1; // report id 0
            let key = format!("royuan:{vid:04x}:{pid:04x}");
            let reply = match query_status(&dev, len) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(e) => {
                    diag::event(&key, format!("i/o failed: {e}"));
                    continue;
                }
            };
            let product = info.product_string().unwrap_or("");
            let brand = brand(product);

            let mut mouse_name = None;
            let mut mouse_id_asked = false;
            if vid == VID && reply[5] == 0 {
                let entry = self.mouse_ids.entry(info.path().to_owned()).or_insert((0, 0));
                // Ask only while the keyboard slot is empty, so the routing cannot redirect a
                // vendor app that is talking to a paired keyboard.
                if entry.0 == 0 && entry.1 < MAX_ID_TRIES && reply[4] != 0 {
                    entry.0 = read_mouse_id(&dev, len).unwrap_or_default();
                    entry.1 += 1;
                }
                if entry.1 > 0 {
                    mouse_id_asked = true;
                }
                mouse_name = ajazz_name(entry.0);
            }

            let Some((keyboard, mouse)) = slots(&reply) else { continue };
            for (is_keyboard, level) in [(true, keyboard), (false, mouse)] {
                let Some(level) = level else { continue };
                let id = format!("{key}:{}", if is_keyboard { "kb" } else { "ms" });
                let name = slot_name(vid, pid, is_keyboard, product, &brand, mouse_name, mouse_id_asked);
                diag::event(&id, format!("battery via report F7: {level}%"));
                out.push(Observation {
                    id,
                    name: Some(name),
                    kind: Some(if is_keyboard { Kind::Keyboard } else { Kind::Mouse }),
                    state: State::Battery { percent: level, charging: false },
                });
            }
        }
        out
    }
}

fn is_receiver(vid: u16, pid: u16) -> bool {
    RECEIVERS.contains(&(vid, pid))
}

fn ajazz_name(id: u32) -> Option<&'static str> {
    AJAZZ_MICE.iter().find(|(model, _)| *model == id).map(|(_, name)| *name)
}

/// `[0]` report id 0, `[1]` ok, `[2]` keyboard level, `[3]` mouse level, `[4]`/`[5]` offline flags.
fn slots(r: &[u8]) -> Option<(Option<u8>, Option<u8>)> {
    if r.len() < 6 {
        return None;
    }
    let slot = |offline: u8, level: u8| (offline == 0 && (1..=100).contains(&level)).then_some(level);
    Some((slot(r[4], r[2]), slot(r[5], r[3])))
}

/// Product string without link or device-type words: "AJAZZ 2.4G 8K" -> "AJAZZ".
fn brand(product: &str) -> String {
    product
        .split_whitespace()
        .filter(|word| {
            !matches!(
                word.to_ascii_uppercase().as_str(),
                "2.4G" | "2.4GHZ" | "8K" | "4K" | "2K" | "1K" | "RECEIVER" | "WIRELESS" | "DONGLE" | "USB" | "KEYBOARD" | "MOUSE"
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn slot_name(
    vid: u16,
    pid: u16,
    keyboard: bool,
    product: &str,
    brand: &str,
    mouse_name: Option<&str>,
    mouse_id_asked: bool,
) -> String {
    let upper = brand.to_ascii_uppercase();
    let other_brand = !brand.is_empty() && !upper.contains("ATTACK SHARK");
    let generic = product.is_empty() || product.contains("Receiver") || product.contains("2.4G");
    if vid == VID && pid == 0x402D && !keyboard && !other_brand {
        "Attack Shark R3".into()
    } else if vid == VID && pid == 0x5038 && keyboard {
        "MonsGeek keyboard".into()
    } else if !keyboard && mouse_name.is_some() {
        mouse_name.unwrap().into()
    } else if generic && !brand.is_empty() && (keyboard || !mouse_id_asked) {
        format!("{brand} wireless {}", if keyboard { "keyboard" } else { "mouse" })
    } else if vid == VID && pid == 0x5006 && keyboard && product.contains("Akko") {
        "Akko Air 01".into()
    } else if generic {
        if keyboard { "Wireless keyboard".into() } else { "Wireless mouse".into() }
    } else {
        product.into()
    }
}

fn query_status(dev: &HidDevice, len: usize) -> hidapi::HidResult<Option<Vec<u8>>> {
    let mut cmd = vec![0u8; len];
    cmd[1] = 0xF7;
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        dev.send_feature_report(&cmd)?;
        std::thread::sleep(Duration::from_millis(20));
        let mut buf = vec![0u8; len];
        if let Ok(n) = dev.get_feature_report(&mut buf) {
            if n >= 7 && (buf[2] | buf[3] | buf[4] | buf[5] | buf[6]) != 0 {
                buf.truncate(n);
                return Ok(Some(buf));
            }
        }
    }
    Ok(None)
}

/// Status polls for the mouse slot: index 6 means "ready to send", index 1 "answer ready".
fn wait_status(dev: &HidDevice, len: usize, index: usize) -> hidapi::HidResult<bool> {
    let mut cmd = vec![0u8; len];
    cmd[1] = 0xF7;
    let mut buf = vec![0u8; len];
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(100));
        dev.send_feature_report(&cmd)?;
        std::thread::sleep(Duration::from_millis(10));
        if let Ok(n) = dev.get_feature_report(&mut buf) {
            if n > index && buf[index] == 1 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn read_mouse_id(dev: &HidDevice, len: usize) -> hidapi::HidResult<u32> {
    if len < 9 {
        return Ok(0);
    }
    let mut route = vec![0u8; len];
    let mut info = vec![0u8; len];
    let mut fetch = vec![0u8; len];
    route[1] = 0xF6;
    route[2] = 0x05;
    info[1] = 0x8F;
    info[8] = 0xFF - 0x8F;
    fetch[1] = 0xFC;

    dev.send_feature_report(&route)?;
    if !wait_status(dev, len, 6)? {
        return Ok(0);
    }
    dev.send_feature_report(&info)?;
    if !wait_status(dev, len, 1)? {
        return Ok(0);
    }
    dev.send_feature_report(&fetch)?;
    std::thread::sleep(Duration::from_millis(10));

    let mut buf = vec![0u8; len];
    let n = dev.get_feature_report(&mut buf)?;
    if n < 6 || buf[1] != 0x8F {
        return Ok(0);
    }
    let id = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]);
    Ok(if id == 0xFFFF_FFFF { 0 } else { id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_drops_link_words() {
        assert_eq!(brand("AJAZZ 2.4G 8K"), "AJAZZ");
        assert_eq!(brand("2.4G Wireless Receiver"), "");
        assert_eq!(brand("Akko 2.4G Wireless Keyboard"), "Akko");
        assert_eq!(brand("MonsGeek 2.4G Wireless Keyboard"), "MonsGeek");
    }

    #[test]
    fn slots_skip_offline_and_uninitialised() {
        // keyboard 80%, mouse 55%, both online.
        assert_eq!(slots(&[0, 1, 80, 55, 0, 0, 0]), Some((Some(80), Some(55))));
        // mouse offline; keyboard level 0 is not a reading.
        assert_eq!(slots(&[0, 1, 0, 55, 0, 1]), Some((None, None)));
        assert_eq!(slots(&[0, 1]), None);
    }

    #[test]
    fn names_follow_the_vendor_rules() {
        let generic = "2.4G Wireless Receiver";
        assert_eq!(slot_name(0x3151, 0x402D, false, generic, "", None, false), "Attack Shark R3");
        assert_eq!(slot_name(0x3151, 0x5038, true, "MonsGeek 2.4G Wireless Keyboard", "MonsGeek", None, false), "MonsGeek keyboard");
        assert_eq!(slot_name(0x3151, 0x4011, true, "2.4G Wireless Receiver", "", None, false), "Wireless keyboard");
        assert_eq!(slot_name(0x3151, 0x4011, true, "AJAZZ 2.4G 8K", "AJAZZ", None, false), "AJAZZ wireless keyboard");
        assert_eq!(slot_name(0x3151, 0x4011, false, "AJAZZ 2.4G 8K", "AJAZZ", Some("AJAZZ AJ159 APEX"), true), "AJAZZ AJ159 APEX");
        assert_eq!(slot_name(0x3151, 0x5006, true, "Akko 5075B Plus V2", "Akko", None, false), "Akko Air 01");
        assert_eq!(slot_name(0x0DB0, 0x197E, false, "MSI Mouse", "MSI", None, false), "MSI Mouse");
    }

    #[test]
    fn ajazz_model_ids() {
        assert_eq!(ajazz_name(1755), Some("AJAZZ AJ159 APEX"));
        assert_eq!(ajazz_name(0xFFFF_FFFF), None);
    }
}
