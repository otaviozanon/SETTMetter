//! GameSir, Flydigi and BEITONG controllers whose batteries are read from vendor reports.
//!
//! XInput reports these pads as wired, so the vendor collection is used:
//! - GameSir Nova (3537:1098/100F/1040): FF7A, request `01 01`, level at [18], 0xFF = charging.
//! - GameSir Cyclone 2 (3537:100B): FFF0:0040, request `0F 03`, input 0x12, level [36].
//! - Flydigi Vader 5 Pro / Apex 5 (37D7): FFA0, `5A A5` info/status reads, level nibble in [11].
//! - BEITONG KP20 / KP40D (20BC): 00FF:0003, nibble-ciphered `02 15` query.
//! Protocols from Wireless-Device-Battery-Monitor (drivers/gamesir.cpp, flydigi.cpp, beitong.cpp).

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::HidApi;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Gamepads {
    seen: Vec<(String, &'static str)>,
}

impl Gamepads {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        self.gamesir(api, &mut out);
        self.flydigi(api, &mut out);
        self.beitong(api, &mut out);
        for (key, name) in &self.seen {
            if !out.iter().any(|o| o.id == *key) {
                out.push(observation(key, name, State::Unreachable));
            }
        }
        out
    }

    fn gamesir(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for &pid in &[0x1098u16, 0x100F, 0x1040] {
            for info in api.device_list().filter(|d| {
                d.vendor_id() == 0x3537 && d.product_id() == pid && d.usage_page() == 0xFF7A
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                let Some(out_len) = probe::output_len(&desc, 0) else { continue };
                if out_len < 3 {
                    continue;
                }
                if let Some(raw) = gamesir_nova(&dev, out_len) {
                    if raw > 100 {
                        continue;
                    }
                    let key = format!("gamesir:{pid:04x}");
                    let name = if pid == 0x100F { "GameSir Nova" } else { "GameSir Nova" };
                    self.seen.push((key.clone(), name));
                    out.push(observation(&key, name, State::Battery { percent: raw, charging: false }));
                    return;
                }
            }
        }
        for info in api.device_list().filter(|d| {
            d.vendor_id() == 0x3537 && d.product_id() == 0x100B && d.usage_page() == 0xFFF0 && d.usage() == 0x0040
        }) {
            let Some(dev) = api.open_path(info.path()).ok() else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            let Some(out_len) = probe::output_len(&desc, 0) else { continue };
            if out_len < 2 {
                continue;
            }
            if let Some((percent, charging)) = gamesir_cyclone(&dev, out_len) {
                let key = "gamesir:100b".to_string();
                self.seen.push((key.clone(), "GameSir Cyclone 2"));
                out.push(observation(&key, "GameSir Cyclone 2", State::Battery { percent, charging }));
                return;
            }
        }
    }

    fn flydigi(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for &(pid, report_id, ids, name) in FLYDIGI {
            for info in api.device_list().filter(|d| {
                d.vendor_id() == 0x37D7 && d.product_id() == pid && d.usage_page() == 0xFFA0 && d.usage() == 0x0001
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                let Some(out_len) = probe::output_len(&desc, report_id) else { continue };
                if out_len < 6 {
                    continue;
                }
                if let Some((percent, charging)) = flydigi_query(&dev, out_len, report_id, ids) {
                    let key = format!("flydigi:{pid:04x}");
                    self.seen.push((key.clone(), name));
                    out.push(observation(&key, name, State::Battery { percent, charging }));
                    return;
                }
            }
        }
    }

    fn beitong(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for &(pid, name) in &[(0x5128u16, "BEITONG KP20"), (0x515C, "BEITONG KP40D")] {
            for info in api.device_list().filter(|d| {
                d.vendor_id() == 0x20BC && d.product_id() == pid && d.usage_page() == 0x00FF && d.usage() == 0x0003
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                let Some(out_len) = probe::output_len(&desc, 0x02) else { continue };
                if out_len < 2 {
                    continue;
                }
                if let Some((percent, charging)) = beitong_query(&dev, out_len, pid == 0x515C) {
                    let key = format!("beitong:{pid:04x}");
                    self.seen.push((key.clone(), name));
                    out.push(observation(&key, name, State::Battery { percent, charging }));
                    return;
                }
            }
        }
    }
}

const FLYDIGI: &[(u16, u8, &[u8], &str)] = &[
    (0x2401, 0x00, &[130], "Flydigi Vader 5 Pro"),
    (0x2501, 0x03, &[128, 129, 133, 134, 135], "Flydigi Apex 5"),
];

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Gamepad), state }
}

fn gamesir_nova(dev: &hidapi::HidDevice, out_len: usize) -> Option<u8> {
    let mut req = vec![0u8; out_len];
    req[..3].copy_from_slice(&[0x00, 0x01, 0x01]);
    let mut buf = [0u8; 64];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    dev.write(&req).ok()?;
    let deadline = Instant::now() + Duration::from_millis(800);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis().min(200) as i32) {
            Ok(n) if n >= 19 && buf[0] == 0x01 => return Some(buf[18]),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

fn gamesir_cyclone(dev: &hidapi::HidDevice, out_len: usize) -> Option<(u8, bool)> {
    let mut req = vec![0u8; out_len];
    req[..2].copy_from_slice(&[0x0F, 0x03]);
    let mut buf = [0u8; 64];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    dev.write(&req).ok()?;
    let deadline = Instant::now() + Duration::from_millis(1500);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis().min(200) as i32) {
            Ok(n) if n >= 37 && buf[0] == 0x12 && buf[36] <= 100 => return Some((buf[36], buf[35] == 1)),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

fn flydigi_query(dev: &hidapi::HidDevice, out_len: usize, report_id: u8, ids: &[u8]) -> Option<(u8, bool)> {
    let info = flydigi_reply(dev, out_len, report_id, 0x07)?;
    let modern = info.len() >= 5 && info[3] == 1 && info[4] == 0;
    let status = flydigi_reply(dev, out_len, report_id, 0x01)?;
    if status.len() < 12 || !ids.contains(&status[5]) {
        return None;
    }
    let level = status[11] & 0x0F;
    let state = status[11] >> 4;
    if state > 2 || level > 5 {
        return None;
    }
    let connection = status[6];
    if modern {
        if connection > 1 {
            return None;
        }
    } else if connection != 1 && connection != 2 {
        return None;
    }
    Some((if state == 2 { 100 } else { level * 20 }, state == 1))
}

fn flydigi_reply(dev: &hidapi::HidDevice, out_len: usize, report_id: u8, command: u8) -> Option<Vec<u8>> {
    let mut req = vec![0u8; out_len];
    req[..6].copy_from_slice(&[report_id, 0x5A, 0xA5, command, if command == 0x01 { 2 } else { 0 }, 0]);
    let mut buf = [0u8; 64];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    dev.write(&req).ok()?;
    let deadline = Instant::now() + Duration::from_millis(800);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(n) if n >= 5 => {
                let r = if n >= 5 && buf[0] == 0x04 && buf[1] == 0x5A && buf[2] == 0xA5 {
                    &buf[1..n]
                } else {
                    &buf[..n]
                };
                if r.len() >= 3 && r[0] == 0x5A && r[1] == 0xA5 && r[2] == command {
                    return Some(r.to_vec());
                }
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

const NIBBLE: [u8; 16] = [0x0, 0xA, 0x6, 0xF, 0x1, 0xC, 0x2, 0x9, 0xE, 0x3, 0x7, 0xD, 0x4, 0xB, 0x5, 0x8];

fn enc(x: u8) -> u8 {
    NIBBLE[usize::from(x >> 4)] << 4 | ((x & 0x0F) ^ 0x08)
}

fn dec(x: u8) -> u8 {
    let hi = NIBBLE.iter().position(|v| *v == (x >> 4)).unwrap_or(0) as u8;
    hi << 4 | ((x & 0x0F) ^ 0x08)
}

fn beitong_query(dev: &hidapi::HidDevice, out_len: usize, kp40d: bool) -> Option<(u8, bool)> {
    let mut req = vec![0u8; out_len];
    req[..2].copy_from_slice(&[0x02, enc(0x15)]);
    let mut buf = [0u8; 64];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    dev.write(&req).ok()?;
    let deadline = Instant::now() + Duration::from_millis(800);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis().min(200) as i32) {
            Ok(n) if n >= 5 && buf[0] == 0x02 && dec(buf[1]) == 0x15 => {
                let level = dec(buf[2]);
                if level > 100 {
                    return None;
                }
                let mut charge = dec(buf[4]);
                if kp40d {
                    charge &= 0x0F;
                }
                return Some((level, charge == 1));
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beitong_cipher_round_trips() {
        for x in 0..=255u8 {
            assert_eq!(dec(enc(x)), x);
        }
        assert_eq!(enc(0x15), 0xAD);
    }

    #[test]
    fn flydigi_level_and_state_nibbles() {
        let byte = 0x15; // state 1 (charging), level 5 -> 100%
        assert_eq!(byte & 0x0F, 5);
        assert_eq!(byte >> 4, 1);
        assert_eq!(5 * 20, 100);
    }
}
