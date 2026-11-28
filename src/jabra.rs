//! Jabra headsets that speak Jabra's GN Protocol (GNP), on their own USB cable or behind a Jabra
//! Link dongle.
//!
//! GNP runs on the vendor collection: usage page `FF00`, usage `05` (Link 390, Evolve3) or `01`
//! (Link 380, Evolve2), report `05`, 64 bytes. Every Jabra product with that collection is asked;
//! what differs between products is worked out per device.
//!
//! ```text
//! query: [05][dst][src 00][seq][40|len][class][op][payload]   len = bytes after the report id
//! reply: [05][dst 00][src][seq][C0|len][class][op][payload]   class FE = NAK, op = reason
//! ```
//!
//! Addresses: `08` = a headset on its own USB cable, `01` = a Link dongle, `04` = the headset
//! behind it. Link dongles, by their USB product string, are asked at `04` first, everything else
//! at `08` first; the address that answers is kept per product. Battery = class `12` / op `02`,
//! reply payload `[flags][level]`: flags bit 0 = charging. Name = class `02` / op `00`, reply
//! payload `[n][n bytes UTF-8]`. A switched-off headset behind a dongle gets a NAK (`F3`, peer
//! offline) while the dongle stays present. DECT bases are left out.
//!
//! Protocol from jabridge and HeadsetControl's GNP driver; verified on a Link 390 with an Evolve3
//! 75 and on the headset's own USB cable. Ported from jabra.cpp.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{DeviceInfo, HidApi, HidDevice};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const VID: u16 = 0x0B0E;
const DONGLE_HEADSET: u8 = 0x04;
const USB_HEADSET: u8 = 0x08;
const NAK_PEER_OFFLINE: u8 = 0xF3;
const USAGE_GNP_ALT: u16 = 0x0001;
const REPORT: u8 = 0x05;
const BATTERY_CLASS: u8 = 0x12;
const BATTERY_OP: u8 = 0x02;
const NAME_CLASS: u8 = 0x02;
const NAME_OP: u8 = 0x00;
const SILENT_RETRY: Duration = Duration::from_secs(5 * 60);

/// DECT bases and dongles (Engage 65/75 incl. SE, PRO 9xx, GO 6400, Link 400, NXT UC-7500, H40),
/// from Jabra Direct's DeviceInfo.xml.
const DECT_BASES: &[u16] = &[
    0x1000, 0x1016, 0x1017, 0x1018, 0x1021, 0x1022, 0x1023, 0x1037, 0x1038, 0x1039, 0x1041, 0x1042, 0x1043,
    0x1110, 0x1112, 0x1113, 0x1118, 0x111A, 0x111B, 0x111D, 0x111E, 0x111F, 0x1120, 0x1121, 0x1122, 0x1123,
    0x1124, 0x1126, 0x1131, 0x1132, 0x1133, 0x1134, 0x1135, 0x1136, 0x1140, 0x1141, 0x1142, 0x1146, 0x1147,
    0x1148, 0x114A, 0x114B, 0x114C, 0x114D, 0x114E, 0x114F, 0x1150, 0x1151, 0xA700, 0xA701, 0xA702, 0xA703,
    0xA704, 0xA705, 0xA706, 0xAE60, 0xAE62, 0xAE63,
];

/// Fresh instances are made on every poll, so what is known per product lives in a module static.
#[derive(Default)]
pub struct Jabra;

#[derive(Default)]
struct Cache {
    /// Headset address per product, kept once a headset answered there.
    addresses: HashMap<u16, u8>,
    /// Headset name per product: asked once while connected.
    names: HashMap<u16, String>,
    /// Products not asked again before this instant; None = for good (every collection NAKed).
    skip: HashMap<u16, Option<Instant>>,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

impl Jabra {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut cache = cache().lock().unwrap();
        // Usage 05 first: products with both collections (Evolve3 75 on USB) answer on 05.
        let mut products: BTreeMap<u16, Vec<&DeviceInfo>> = BTreeMap::new();
        for info in api.device_list() {
            if info.vendor_id() != VID
                || info.usage_page() != 0xFF00
                || !matches!(info.usage(), 0x05 | USAGE_GNP_ALT)
                || DECT_BASES.contains(&info.product_id())
            {
                continue;
            }
            products.entry(info.product_id()).or_default().push(info);
        }

        let mut out = Vec::new();
        for (pid, mut collections) in products {
            if cache.skip.get(&pid).is_some_and(|until| until.is_none_or(|t| Instant::now() < t)) {
                continue;
            }
            collections.sort_by_key(|d| d.usage() != 0x05);

            let mut reported = false;
            let mut probed = false;
            let mut skippable = true;
            let mut all_nak = true;
            for info in collections {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                let out_len = probe::output_len(&desc, REPORT).unwrap_or(0);
                let in_len = probe::input_len(&desc, REPORT).unwrap_or(0);
                if out_len < 64 || in_len < 64 {
                    continue;
                }

                let dongle = match cache.addresses.get(&pid) {
                    Some(address) => *address == DONGLE_HEADSET,
                    None => info.product_string().unwrap_or_default().contains("Link"),
                };
                let order = match cache.addresses.get(&pid) {
                    Some(address) => vec![*address],
                    None if dongle => vec![DONGLE_HEADSET, USB_HEADSET],
                    None => vec![USB_HEADSET, DONGLE_HEADSET],
                };
                if dongle || order.len() == 1 {
                    skippable = false;
                }

                let mut address = 0;
                let mut offline = false;
                let mut payload = Vec::new();
                for candidate in order {
                    let reply = query(&dev, out_len, in_len, candidate, BATTERY_CLASS, BATTERY_OP, Duration::from_millis(700));
                    match &reply {
                        None | Some((false, _)) => all_nak = false,
                        Some((true, _)) => {}
                    }
                    let Some((nak, data)) = reply else { continue };
                    if !nak && data.len() >= 2 {
                        address = candidate;
                        payload = data;
                        break;
                    }
                    // A dongle reports its headset offline rather than absent (F3); elsewhere a NAK
                    // means there is no headset with a battery at that address.
                    if nak && candidate == DONGLE_HEADSET && (dongle || data.first() == Some(&NAK_PEER_OFFLINE)) {
                        address = candidate;
                        offline = true;
                        break;
                    }
                }
                if address == 0 {
                    cache.addresses.remove(&pid);
                    probed = true;
                    continue;
                }
                cache.addresses.insert(pid, address);

                let key = format!("jabra:{pid:04x}");
                if offline {
                    // Dongle present, headset off or out of range; a new headset may be paired next.
                    let name = cache.names.remove(&pid).unwrap_or_else(|| "Jabra headset".into());
                    diag::event(&key, "headset offline behind the dongle");
                    out.push(observation(pid, &name, State::Off));
                    reported = true;
                    break;
                }
                let Some(percent) = payload.get(1).copied().filter(|p| *p <= 100) else { continue };
                let charging = payload.first().is_some_and(|flags| flags & 0x01 != 0);
                let name = cache
                    .names
                    .entry(pid)
                    .or_insert_with(|| headset_name(&dev, out_len, in_len, address).unwrap_or_else(|| "Jabra headset".into()))
                    .clone();
                diag::event(
                    &key,
                    format!(
                        "{percent}%{} on {}",
                        if charging { ", charging" } else { "" },
                        if address == DONGLE_HEADSET { "a Link dongle" } else { "USB" }
                    ),
                );
                out.push(observation(pid, &name, State::Battery { percent, charging }));
                reported = true;
                break;
            }

            if !reported && probed && skippable {
                cache.skip.insert(pid, if all_nak { None } else { Some(Instant::now() + SILENT_RETRY) });
            }
        }
        out
    }
}

fn observation(pid: u16, name: &str, state: State) -> Observation {
    Observation { id: format!("jabra:{pid:04x}"), name: Some(name.into()), kind: Some(Kind::Headset), state }
}

fn headset_name(dev: &HidDevice, out_len: usize, in_len: usize, address: u8) -> Option<String> {
    let (nak, payload) = query(dev, out_len, in_len, address, NAME_CLASS, NAME_OP, Duration::from_millis(400))?;
    if nak || payload.is_empty() {
        return None;
    }
    let n = usize::from(payload[0]);
    if n == 0 || n + 1 > payload.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&payload[1..n + 1]).into_owned())
}

/// Ok/Nak payload, or None when nothing answered in time.
fn query(dev: &HidDevice, out_len: usize, in_len: usize, dst: u8, class: u8, op: u8, timeout: Duration) -> Option<(bool, Vec<u8>)> {
    static SEQ: AtomicU8 = AtomicU8::new(0x20);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);

    drain(dev, in_len);
    let mut request = vec![0u8; out_len.max(7)];
    request[..7].copy_from_slice(&[REPORT, dst, 0x00, seq, 0x46, class, op]);
    dev.write(&request).ok()?;

    let deadline = Instant::now() + timeout;
    let mut buf = vec![0u8; in_len];
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        let n = dev.read_timeout(&mut buf, left.as_millis().max(1) as i32).ok()?;
        // Unrelated short reports are skipped, like an unrelated collection's traffic.
        if n < 7 {
            continue;
        }
        if let Some(reply) = parse_reply(&buf[..n], dst, seq, class, op) {
            return Some(reply);
        }
    }
}

fn parse_reply(r: &[u8], dst: u8, seq: u8, class: u8, op: u8) -> Option<(bool, Vec<u8>)> {
    if r.len() < 7 || r[0] != REPORT || r[1] != 0x00 || r[2] != dst || r[3] != seq || (r[4] & 0xC0) != 0xC0 {
        return None;
    }
    if r[5] == 0xFE {
        return Some((true, vec![r[6]]));
    }
    let len = usize::from(r[4] & 0x3F); // counts from r[1]
    if r[5] != class || r[6] != op || len < 6 || len + 1 > r.len() {
        return None;
    }
    Some((false, r[7..len + 1].to_vec()))
}

fn drain(dev: &HidDevice, len: usize) {
    let mut buf = vec![0u8; len];
    while dev.read_timeout(&mut buf, 0).is_ok_and(|n| n > 0) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_and_name_replies() {
        // 8 bytes after the report id: dst 00, src 08, seq 21, C0|8, class 12, op 02, [flags 00][level 50].
        let r = [0x05, 0x00, 0x08, 0x21, 0xC8, 0x12, 0x02, 0x00, 0x50];
        let (nak, p) = parse_reply(&r, USB_HEADSET, 0x21, BATTERY_CLASS, BATTERY_OP).unwrap();
        assert!(!nak);
        assert_eq!(p, [0x00, 0x50]);
        // Charging flag.
        let r = [0x05, 0x00, 0x04, 0x22, 0xC8, 0x12, 0x02, 0x01, 0x4B];
        let (_, p) = parse_reply(&r, DONGLE_HEADSET, 0x22, BATTERY_CLASS, BATTERY_OP).unwrap();
        assert_eq!(p[0] & 0x01, 1);
        assert_eq!(p[1], 75);
        // Name: [n][n bytes].
        let r = [0x05, 0x00, 0x08, 0x23, 0xCA, 0x02, 0x00, 0x04, b'E', b'v', b'o'];
        let (_, p) = parse_reply(&r, USB_HEADSET, 0x23, NAME_CLASS, NAME_OP).unwrap();
        assert_eq!(&p[1..], b"Evo");
    }

    #[test]
    fn nak_and_mismatches() {
        // NAK: class FE, reason F3 = peer offline.
        let nak = [0x05, 0x00, 0x04, 0x24, 0xC6, 0xFE, 0xF3];
        assert_eq!(parse_reply(&nak, DONGLE_HEADSET, 0x24, BATTERY_CLASS, BATTERY_OP), Some((true, vec![0xF3])));
        // Another sequence number or a truncated reply is not ours.
        assert_eq!(parse_reply(&[0x05, 0x00, 0x08, 0x25, 0xC8, 0x12, 0x02, 0, 80], USB_HEADSET, 0x21, BATTERY_CLASS, BATTERY_OP), None);
        assert_eq!(parse_reply(&[0x05, 0x00, 0x08, 0x21, 0xC8, 0x12], USB_HEADSET, 0x21, BATTERY_CLASS, BATTERY_OP), None);
    }

    #[test]
    fn dect_bases_are_skipped() {
        assert!(DECT_BASES.contains(&0xAE60));
        assert!(!DECT_BASES.contains(&0x2E51)); // Link 390
    }
}
