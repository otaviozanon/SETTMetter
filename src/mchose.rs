//! MCHOSE wireless headsets (V9 Pro, X9) over their 2.4 GHz dongle.
//!
//! Protocol as documented by HeadsetControl (captured from the "M HUB" app and checked on
//! real dongles): the vendor collection on usage page 0xFF90 carries a 64-byte report 0x55
//! both ways. Writing `55 65 01` asks for the battery; the dongle answers `55 65 <pct> 02`
//! (byte 3 names the field, 0x02 = battery). Other 0x55 reports (heartbeats, telemetry)
//! arrive on the same collection and are ignored. No charging flag is known.
//!
//! Dongles stay open between scans. A reply that arrives after the scan stopped waiting is
//! picked up at the start of the next one.

use crate::device::{Kind, Observation, State};
use crate::diag;
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::ffi::CString;
use std::time::Duration;

const USAGE_PAGE: u16 = 0xFF90;
const REPORT_ID: u8 = 0x55;
const MSG_VALUE: u8 = 0x65;
const QUERY_BATTERY: u8 = 0x01;
const FIELD_BATTERY: u8 = 0x02;
const REPORT_LEN: usize = 64;
const WAIT: Duration = Duration::from_millis(400);

/// (vendor id, product id, name)
const DEVICES: &[(u16, u16, &str)] = &[
    (0x291D, 0x385D, "MCHOSE V9 Pro"), // C-Media dongle
    (0x3837, 0x6045, "MCHOSE X9"),
];

struct Dongle {
    dev: HidDevice,
    key: String,
    name: &'static str,
    /// Whether it ever answered; until then a silent dongle isn't listed.
    seen: bool,
}

#[derive(Default)]
pub struct Mchose {
    open: HashMap<CString, Dongle>,
}

impl Mchose {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let present: Vec<_> = api
            .device_list()
            .filter(|d| d.usage_page() == USAGE_PAGE)
            .filter_map(|d| {
                let (_, pid, name) = DEVICES.iter().find(|(v, p, _)| *v == d.vendor_id() && *p == d.product_id())?;
                Some((d.path().to_owned(), *pid, *name))
            })
            .collect();
        self.open.retain(|path, _| present.iter().any(|(p, ..)| p == path));

        let mut out = Vec::new();
        for (path, pid, name) in present {
            if !self.open.contains_key(&path) {
                let key = format!("mchose:{pid:04x}");
                match api.open_path(&path) {
                    Ok(dev) => {
                        self.open.insert(path.clone(), Dongle { dev, key, name, seen: false });
                    }
                    Err(e) => {
                        diag::event(&key, format!("open failed: {e}"));
                        continue;
                    }
                }
            }
            let d = self.open.get_mut(&path).unwrap();
            match query(&d.dev) {
                Ok(Some(percent)) => {
                    d.seen = true;
                    diag::event(&d.key, "battery via report 0x55");
                    out.push(observation(d, State::Battery { percent, charging: false }));
                }
                Ok(None) => {
                    diag::event(&d.key, "no battery reply");
                    if d.seen {
                        out.push(observation(d, State::Unreachable));
                    }
                }
                Err(e) => {
                    // Unplugged or the handle went bad: reopen on the next scan.
                    diag::event(&d.key, format!("i/o failed: {e}"));
                    self.open.remove(&path);
                }
            }
        }
        out
    }
}

fn observation(d: &Dongle, state: State) -> Observation {
    Observation { id: d.key.clone(), name: Some(d.name.into()), kind: Some(Kind::Headset), state }
}

fn query(dev: &HidDevice) -> hidapi::HidResult<Option<u8>> {
    let mut buf = [0u8; REPORT_LEN];
    // Late replies to the previous scan's request.
    let mut found = None;
    loop {
        match dev.read_timeout(&mut buf, 0)? {
            0 => break,
            n => found = parse(&buf[..n]).or(found),
        }
    }

    let mut req = [0u8; REPORT_LEN];
    req[..3].copy_from_slice(&[REPORT_ID, MSG_VALUE, QUERY_BATTERY]);
    dev.write(&req)?;

    let deadline = std::time::Instant::now() + WAIT;
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32)? {
            0 => break,
            n => {
                if let Some(p) = parse(&buf[..n]) {
                    return Ok(Some(p));
                }
            }
        }
    }
    Ok(found)
}

fn parse(r: &[u8]) -> Option<u8> {
    match r {
        [REPORT_ID, MSG_VALUE, p, FIELD_BATTERY, ..] if *p <= 100 => Some(*p),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse() {
        // V9 Pro reply at 50%, as captured by HeadsetControl.
        assert_eq!(super::parse(&[0x55, 0x65, 0x32, 0x02, 0x00]), Some(50));
        // Same message type, different field.
        assert_eq!(super::parse(&[0x55, 0x65, 0x32, 0x03]), None);
        assert_eq!(super::parse(&[0x55, 0x65, 0xff, 0x02]), None);
        assert_eq!(super::parse(&[0x55, 0x10]), None);
    }
}
