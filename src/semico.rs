//! Semico tri-mode keyboards: Attack Shark X98 and AJAZZ AK820 Max Plus.
//!
//! VID 1A2C, dongle 8FFF, cable A034 / A036. Collection FF02:0002 with 64-byte reports:
//! request `[00 AA 55 CC 33 0E]`; reply `[AA 55 CC 33 0E percent state]`, state 0 = offline.
//! No charging flag (the protocol's state byte also reads 1 on battery).
//! Protocol from Wireless-Device-Battery-Monitor (drivers/semico.cpp), tested on an X98 dongle.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::HidApi;
use std::time::{Duration, Instant};

const VID: u16 = 0x1A2C;
const DONGLE: u16 = 0x8FFF;
const PIDS: &[u16] = &[DONGLE, 0xA034, 0xA036];
const KEY: &str = "semico:1a2c";

#[derive(Default)]
pub struct Semico {
    seen: bool,
}

impl Semico {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut answered_offline = false;
        // The dongle first: an active 2.4 GHz keyboard answers even when the cable is plugged in.
        for &pid in PIDS {
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == pid
                    && d.interface_number() == 0
                    && d.usage_page() == 0xFF02
                    && d.usage() == 0x0002
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, 0).unwrap_or(0) < 64 {
                    continue;
                }
                match query(&dev) {
                    Ok(Some((percent, true))) => {
                        self.seen = true;
                        let name = product_name(pid, info.product_string().unwrap_or(""));
                        return vec![Observation {
                            id: KEY.into(),
                            name: Some(name),
                            kind: Some(Kind::Keyboard),
                            state: State::Battery { percent, charging: false },
                        }];
                    }
                    Ok(Some((_, false))) => answered_offline = true,
                    Ok(None) => {}
                    Err(e) => diag::event(KEY, format!("i/o failed: {e}")),
                }
            }
        }
        if answered_offline || self.seen {
            vec![Observation {
                id: KEY.into(),
                name: Some("Wireless keyboard".into()),
                kind: Some(Kind::Keyboard),
                state: State::Unreachable,
            }]
        } else {
            Vec::new()
        }
    }
}

fn product_name(pid: u16, product: &str) -> String {
    match pid {
        0xA034 => "Attack Shark X98".into(),
        0xA036 => "AJAZZ AK820 Max Plus".into(),
        _ => {
            let name = product.trim();
            if name.is_empty() || name == "2.4 Wireless Device" || name == "USB Gaming Keyboard" {
                "Wireless keyboard".into()
            } else {
                name.into()
            }
        }
    }
}

fn query(dev: &hidapi::HidDevice) -> hidapi::HidResult<Option<(u8, bool)>> {
    let mut req = [0u8; 65];
    req[1..6].copy_from_slice(&[0xAA, 0x55, 0xCC, 0x33, 0x0E]);
    let mut buf = [0u8; 65];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    dev.write(&req)?;
    let deadline = Instant::now() + Duration::from_millis(600);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(n) if n >= 7 => {
                let r = if buf[0] == 0 { &buf[1..n] } else { &buf[..n] };
                if r.len() >= 7 && r.starts_with(&[0xAA, 0x55, 0xCC, 0x33, 0x0E]) {
                    if r[6] == 0 {
                        return Ok(Some((0, false)));
                    }
                    if r[6] <= 3 && r[5] <= 100 {
                        return Ok(Some((r[5], true)));
                    }
                }
            }
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_cable_names() {
        assert_eq!(product_name(0xA034, ""), "Attack Shark X98");
        assert_eq!(product_name(0xA036, ""), "AJAZZ AK820 Max Plus");
        assert_eq!(product_name(DONGLE, "USB Gaming Keyboard"), "Wireless keyboard");
        assert_eq!(product_name(DONGLE, "My Board"), "My Board");
    }
}
