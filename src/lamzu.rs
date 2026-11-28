//! LAMZU wireless mice on their own vendor ids 0x373E and 0x37B0 (Maya X, Maya, Tachi, Inca,
//! Paro, Thorn, Atlantis, Orcus…). The Atlantis on Compx 0x3554 uses the Kysona protocol instead.
//!
//! Protocol from LAMZU's Aurora web driver ("IsNewProtocol" models): 65-byte feature report 0
//! with `00 00 00 02 02 00 83 …`, then the same report read back about 100 ms later:
//! `A1 .. .. 02 .. 83 <charging> <percent>` after the report id byte (the web driver accepts the
//! reply with or without that byte, so both shapes are parsed). Charging 1 = on the cable; a
//! receiver answering percent 0 means the mouse is off or out of range.
//!
//! Model ids from lamzu.net/Config/env-models.json. Several newer models share one dongle id, so
//! those dongles get a generic name. Read-only: only the battery query is sent.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::Duration;

/// 65-byte feature report 0: the report id byte plus the 64-byte payload.
const LEN: usize = 65;
const MARKER: u8 = 0x83;
const WAIT: Duration = Duration::from_millis(100);

/// Arbiter Studio Akitsu mice share 0x373E and the same query but are handled by `arbiter.rs`;
/// its driver runs first upstream and claims them so they aren't named as LAMZU here.
const ARBITER_PIDS: &[u16] = &[0x0009, 0x000A, 0x0038, 0x0039, 0x0066, 0x0067];

/// (vendor id, product ids, name)
const MODELS: &[(u16, &[u16], &str)] = &[
    (0x373E, &[0x001C, 0x001D, 0x001E], "LAMZU Maya X"),
    (0x373E, &[0x0016], "LAMZU Maya X V2"),
    (0x37B0, &[0x001F], "LAMZU Maya X V2"),
    (0x37B0, &[0x0005, 0x000B, 0x000C], "LAMZU Tachi"),
    (0x37B0, &[0x001C, 0x001E], "LAMZU Tachi Lite"),
    (0x37B0, &[0x0009, 0x000F, 0x0010], "LAMZU Inca"),
    (0x37B0, &[0x0011, 0x0013, 0x0015], "LAMZU Maya"),
    (0x37B0, &[0x0007, 0x000D, 0x000E], "LAMZU Paro"),
    (0x37B0, &[0x0017, 0x0019, 0x001B], "LAMZU Thorn"),
    (0x37B0, &[0x0021, 0x0003, 0x0023], "LAMZU Thorn V2"),
    (0x37B0, &[0x0030, 0x0032, 0x0040], "LAMZU Thorn V2"),
    (0x37B0, &[0x0025, 0x0027], "LAMZU Atlantis OG Champion"),
    (0x37B0, &[0x0028, 0x002B, 0x002A], "LAMZU Atlantis Mini"),
    (0x37B0, &[0x003E, 0x003F], "LAMZU Atlantis Mini"),
    (0x37B0, &[0x002C, 0x0052], "LAMZU Maya M"),
    (0x37B0, &[0x003A, 0x003B, 0x0044], "LAMZU Maya X"),
    (0x37B0, &[0x003C, 0x0050], "LAMZU Maya"),
    (0x37B0, &[0x0042, 0x0056], "LAMZU Orcus"),
    (0x37B0, &[0x0046, 0x0054], "LAMZU Mini"),
];

/// Wired product ids: the mouse itself on its USB cable.
const CABLES: &[u16] = &[
    0x0005, 0x0007, 0x0009, 0x0011, 0x0017, 0x001C, 0x001F, 0x0021, 0x0025, 0x0028, 0x002C, 0x0030, 0x0034, 0x0038,
    0x003A, 0x003C, 0x003E, 0x0040, 0x0042, 0x0044, 0x0046, 0x0050, 0x0052, 0x0056,
];

#[derive(Default)]
pub struct Lamzu {
    /// A dongle that answered once keeps its row while the mouse sleeps.
    seen: HashSet<String>,
}

impl Lamzu {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut products: Vec<(u16, u16)> = api
            .device_list()
            .filter(|d| matches!(d.vendor_id(), 0x373E | 0x37B0))
            .filter(|d| !(d.vendor_id() == 0x373E && ARBITER_PIDS.contains(&d.product_id())))
            .map(|d| (d.vendor_id(), d.product_id()))
            .collect();
        products.sort_unstable();
        products.dedup();

        let mut out = Vec::new();
        for (vid, pid) in products {
            let key = format!("lamzu:{vid:04x}:{pid:04x}");
            let cable = is_cable(vid, pid);
            let name = model_name(vid, pid).unwrap_or("LAMZU wireless mouse");
            let mut found = None;
            for info in api.device_list().filter(|d| d.vendor_id() == vid && d.product_id() == pid) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                // The 65-byte request means 64 payload bytes plus the id byte.
                if !matches!(probe::feature_len(&desc, 0), Some(64 | 65)) {
                    continue;
                }
                match query(&dev) {
                    Ok(Some(reply)) => {
                        found = Some(reply);
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => diag::event(&key, format!("i/o failed: {e}")),
                }
            }
            match found {
                Some((charging, percent)) if percent > 0 || cable => {
                    self.seen.insert(key.clone());
                    out.push(observation(&key, name, State::Battery { percent, charging }));
                }
                Some(_) => {
                    // 0 from a dongle: the mouse is off or out of range.
                    self.seen.insert(key.clone());
                    out.push(observation(&key, name, State::Unreachable));
                }
                None if self.seen.contains(&key) => out.push(observation(&key, name, State::Unreachable)),
                None => {}
            }
        }
        out
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

fn model_name(vid: u16, pid: u16) -> Option<&'static str> {
    MODELS.iter().find(|(v, pids, _)| *v == vid && pids.contains(&pid)).map(|(_, _, name)| *name)
}

fn is_cable(vid: u16, pid: u16) -> bool {
    if vid == 0x373E {
        return pid == 0x001C;
    }
    CABLES.contains(&pid)
}

fn query(dev: &HidDevice) -> hidapi::HidResult<Option<(bool, u8)>> {
    let mut req = [0u8; LEN];
    req[3] = 0x02;
    req[4] = 0x02;
    req[6] = MARKER;
    dev.send_feature_report(&req)?;
    std::thread::sleep(WAIT);
    let mut buf = [0u8; LEN];
    let n = dev.get_feature_report(&mut buf)?;
    Ok(parse(&buf[..n]))
}

/// `<status> .. .. 02 .. 83 <charging> <percent>`, with or without the leading report id byte.
fn parse(r: &[u8]) -> Option<(bool, u8)> {
    if r.len() < 10 {
        return None;
    }
    let (charging, percent) = if r[1] == 0xA1 && r[4] == 0x02 && r[6] == MARKER {
        (r[7], r[8])
    } else if r[0] == 0xA1 && r[3] == 0x02 && r[5] == MARKER {
        (r[6], r[7])
    } else {
        return None;
    };
    (percent <= 100).then_some((charging == 1, percent))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(percent: u8, charging: u8) -> Vec<u8> {
        let mut r = vec![0u8; LEN];
        r[1] = 0xA1;
        r[4] = 0x02;
        r[6] = MARKER;
        r[7] = charging;
        r[8] = percent;
        r
    }

    #[test]
    fn parses_reply_with_and_without_the_report_id() {
        assert_eq!(parse(&reply(87, 0)), Some((false, 87)));
        assert_eq!(parse(&reply(64, 1)), Some((true, 64)));
        // The same answer shifted one byte: no leading report id.
        assert_eq!(parse(&reply(87, 0)[1..]), Some((false, 87)));
    }

    #[test]
    fn rejects_noise() {
        assert_eq!(parse(&reply(87, 0)[..9]), None); // short
        let mut wrong = reply(50, 0);
        wrong[6] = 0x00; // no marker
        assert_eq!(parse(&wrong), None);
        assert_eq!(parse(&reply(101, 0)), None); // out of range
    }

    #[test]
    fn names_and_cables_match_the_model_list() {
        assert_eq!(model_name(0x373E, 0x001D), Some("LAMZU Maya X"));
        assert_eq!(model_name(0x37B0, 0x001C), Some("LAMZU Tachi Lite"));
        assert_eq!(model_name(0x37B0, 0x0046), Some("LAMZU Mini"));
        assert_eq!(model_name(0x37B0, 0x9999), None);
        assert!(is_cable(0x373E, 0x001C));
        assert!(!is_cable(0x373E, 0x001D));
        assert!(is_cable(0x37B0, 0x0005));
        assert!(!is_cable(0x37B0, 0x000B));
    }
}
