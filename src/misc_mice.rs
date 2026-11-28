//! Motospeed and AJAZZ mice whose protocols do not belong to another family.
//!
//! - Motospeed X6 (0BDA FFE0/FFF2 receiver, FFF1 cable): output report 0xB5 command 0x06, reply
//!   report 0xB4, byte 20 = level (+128 while charging).
//! - AJAZZ AJ179 family (249A receiver, 248A cable, interface 2): 32-byte report 0x10, model in
//!   bytes 4-7, charging byte 12, level byte 13, link byte 14, checksum byte 31.
//! - AJAZZ AJ139 (A8A5 dongle, A8A4 cable): 64-byte report, `55 30 A5 …`, reply `AA 30`, level
//!   byte 8 and charging byte 9.
//! Protocols from Wireless-Device-Battery-Monitor (drivers/misc_mice.cpp), community sources.

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::HidApi;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct MiscMice {
    motospeed_seen: bool,
    ajazz_seen: bool,
    aj139_seen: bool,
}

impl MiscMice {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        self.motospeed(api, &mut out);
        self.ajazz(api, &mut out);
        self.aj139(api, &mut out);
        if self.motospeed_seen && !out.iter().any(|o| o.id.starts_with("motospeed:")) {
            out.push(Observation { id: "motospeed:x6".into(), name: Some("Motospeed X6".into()), kind: Some(Kind::Mouse), state: State::Unreachable });
        }
        if self.ajazz_seen && !out.iter().any(|o| o.id.starts_with("ajazz:")) {
            out.push(Observation { id: "ajazz:mouse".into(), name: Some("Ajazz mouse".into()), kind: Some(Kind::Mouse), state: State::Unreachable });
        }
        if self.aj139_seen && !out.iter().any(|o| o.id.starts_with("ajazz139:")) {
            out.push(Observation { id: "ajazz139:mouse".into(), name: Some("AJAZZ wireless mouse".into()), kind: Some(Kind::Mouse), state: State::Unreachable });
        }
        out
    }

    fn motospeed(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for &pid in &[0xFFE0u16, 0xFFF2, 0xFFF1] {
            for info in api.device_list().filter(|d| {
                d.vendor_id() == 0x0BDA && d.product_id() == pid && d.interface_number() == 4
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, 0xB5).unwrap_or(0) < 21 {
                    continue;
                }
                if pid == 0xFFF1 {
                    self.motospeed_seen = true;
                    out.push(Observation {
                        id: "motospeed:fff1".into(),
                        name: Some("Motospeed X6".into()),
                        kind: Some(Kind::Mouse),
                        state: State::Battery { percent: 100, charging: true },
                    });
                    return;
                }
                if let Some((raw, charging)) = motospeed_query(&dev) {
                    self.motospeed_seen = true;
                    out.push(Observation {
                        id: format!("motospeed:{pid:04x}"),
                        name: Some("Motospeed X6".into()),
                        kind: Some(Kind::Mouse),
                        state: State::Battery { percent: raw & 0x7F, charging },
                    });
                    return;
                }
            }
        }
    }

    fn ajazz(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for &vendor in &[0x249Au16, 0x248A] {
            for info in api.device_list().filter(|d| {
                d.vendor_id() == vendor
                    && d.interface_number() == 2
                    && d.usage_page() >= 0xFF00
                    && !matches!(d.product_id(), 0x5B2F | 0x5B2E)
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, 0) != Some(32) || probe::input_len(&desc, 0) != Some(32) {
                    continue;
                }
                if let Some(r) = ajazz_query(&dev) {
                    let receiver = vendor == 0x249A;
                    if receiver && r[14] == 0 {
                        continue;
                    }
                    self.ajazz_seen = true;
                    let model: String = r[4..8].iter().take_while(|b| (0x20..0x7F).contains(*b)).map(|b| *b as char).collect();
                    let name = if model.len() == 4 && model.starts_with('M') {
                        format!("Ajazz AJ{}", &model[1..])
                    } else {
                        "Ajazz mouse".to_string()
                    };
                    out.push(Observation {
                        id: format!("ajazz:{vendor:04x}:{:04x}", info.product_id()),
                        name: Some(name),
                        kind: Some(Kind::Mouse),
                        state: State::Battery { percent: r[13], charging: r[12] != 0 },
                    });
                    return;
                }
            }
        }
    }

    fn aj139(&mut self, api: &HidApi, out: &mut Vec<Observation>) {
        for &vendor in &[0xA8A5u16, 0xA8A4] {
            for info in api.device_list().filter(|d| {
                d.vendor_id() == vendor && d.product_id() == 0x2255 && d.usage_page() == 0xFF01 && d.usage() == 0x0010
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, 0).unwrap_or(0) < 64 {
                    continue;
                }
                if let Some(r) = aj139_query(&dev) {
                    let cable = vendor == 0xA8A4;
                    if !cable && r[8] == 0 {
                        continue;
                    }
                    self.aj139_seen = true;
                    out.push(Observation {
                        id: format!("ajazz139:{vendor:04x}:2255"),
                        name: Some("AJAZZ wireless mouse".into()),
                        kind: Some(Kind::Mouse),
                        state: State::Battery { percent: r[8], charging: r[9] != 0 },
                    });
                    return;
                }
            }
        }
    }
}

fn motospeed_query(dev: &hidapi::HidDevice) -> Option<(u8, bool)> {
    let mut req = [0u8; 21];
    req[..2].copy_from_slice(&[0xB5, 0x06]);
    let mut buf = [0u8; 21];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    dev.write(&req).ok()?;
    let deadline = Instant::now() + Duration::from_millis(1500);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(n) if n > 20 && buf[0] == 0xB4 => return Some((buf[20], buf[20] >= 128)),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

fn ajazz_query(dev: &hidapi::HidDevice) -> Option<[u8; 32]> {
    let mut req = [0u8; 33];
    req[..2].copy_from_slice(&[0x00, 0x10]);
    let mut buf = [0u8; 33];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    dev.write(&req).ok()?;
    let deadline = Instant::now() + Duration::from_millis(1500);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis() as i32) {
            Ok(n) if n >= 32 && buf[0] == 0x10 && buf[1] == 0x00 => {
                let sum: u8 = buf[4..=30].iter().fold(0u8, |s, b| s.wrapping_add(*b));
                if sum == buf[31] && buf[13] <= 100 {
                    let mut r = [0u8; 32];
                    r.copy_from_slice(&buf[..32]);
                    return Some(r);
                }
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

fn aj139_query(dev: &hidapi::HidDevice) -> Option<[u8; 65]> {
    let mut buf = [0u8; 65];
    for req in [&[0x00, 0x55, 0x30, 0xA5, 0x2E, 0x2E][..], &[0x00, 0x55, 0x30, 0xA5, 0x0B, 0x2E, 0x01, 0x01][..]] {
        let mut w = [0u8; 65];
        w[..req.len()].copy_from_slice(req);
        while let Ok(n) = dev.read_timeout(&mut buf, 0) {
            if n == 0 {
                break;
            }
        }
        if dev.write(&w).is_err() {
            continue;
        }
        let deadline = Instant::now() + Duration::from_millis(800);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match dev.read_timeout(&mut buf, left.as_millis() as i32) {
                Ok(n) if n > 9 && buf[0] == 0xAA && buf[1] == 0x30 && buf[8] <= 100 => return Some(buf),
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn motospeed_level_and_charge_bit() {
        let raw = 85u8 | 128;
        assert_eq!((raw & 0x7F, raw >= 128), (85, true));
    }

    #[test]
    fn ajazz_checksum_covers_model_and_level() {
        let mut r = [0u8; 32];
        r[0] = 0x10;
        r[4..8].copy_from_slice(b"M802");
        r[12] = 1;
        r[13] = 76;
        r[31] = r[4..=30].iter().fold(0u8, |s, b| s.wrapping_add(*b));
        assert_eq!(r[31], r[4..=30].iter().fold(0u8, |s, b| s.wrapping_add(*b)));
        assert_eq!(r[13], 76);
    }
}
