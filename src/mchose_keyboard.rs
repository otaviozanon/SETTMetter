//! MCHOSE keyboards on the shared 3837:3033 "MCHOSE <model> 2.4G" receiver (G75 V2, G87 V2,
//! K99 V3...). GLW func-config read: 64-byte report 0, command 0x05, payload byte 32 carries
//! bit 7 = charging and bits 0-6 = percent.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/mchose_keyboard.cpp), from M HUB.

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x3837;
const PID: u16 = 0x3033;
const LEN: usize = 65;
const FLAG: u8 = 0x55;
const CMD: u8 = 0x05;
const PAYLOAD: usize = 8;
const BATTERY: usize = 32;
const ASK_EVERY: Duration = Duration::from_secs(10 * 60);

#[derive(Default)]
pub struct MchoseKeyboard {
    last: Option<(u8, bool)>,
    next: Option<Instant>,
}

impl MchoseKeyboard {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let infos: Vec<_> = api
            .device_list()
            .filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == PID
                    && ((d.usage_page() == 0x0001 && d.usage() == 0x0000)
                        || (d.usage_page() == 0xFF31 && d.usage() == 0x0074))
            })
            .collect();
        let Some(first) = infos.first() else { return Vec::new() };
        let ask = self.next.is_none_or(|at| Instant::now() >= at);
        if ask {
            for info in &infos {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, 0).unwrap_or(0) < 64 || probe::input_len(&desc, 0).unwrap_or(0) < 64 {
                    continue;
                }
                if let Some(read) = query(&dev) {
                    self.last = Some(read);
                    break;
                }
            }
            self.next = Some(Instant::now() + ASK_EVERY);
        }
        let Some((percent, charging)) = self.last else { return Vec::new() };
        let name = first.product_string().unwrap_or("").trim().strip_suffix(" 2.4G").unwrap_or(first.product_string().unwrap_or("")).trim();
        vec![Observation {
            id: "mchose:3837:3033".into(),
            name: Some(if name.is_empty() { "MCHOSE keyboard".into() } else { name.into() }),
            kind: Some(Kind::Keyboard),
            state: State::Battery { percent, charging },
        }]
    }
}

fn query(dev: &HidDevice) -> Option<(u8, bool)> {
    let mut req = [0u8; LEN];
    req[..9].copy_from_slice(&[0x00, FLAG, CMD, 0x00, 56, 56, 0x00, 0x00, 0x00]);
    let mut buf = [0u8; LEN];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(150));
        }
        dev.write(&req).ok()?;
        let deadline = Instant::now() + Duration::from_millis(500);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match dev.read_timeout(&mut buf, left.as_millis() as i32) {
                Ok(n) if n > PAYLOAD + BATTERY => {
                    if let Some(read) = parse(&buf[..n]) {
                        return Some(read);
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
    None
}

fn parse(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < PAYLOAD + BATTERY + 1 || r[0] != 0xAA || r[1] != CMD || r[5] != 0 || r[6] != 0 {
        return None;
    }
    let size = usize::from(r[4]);
    if size < BATTERY + 1 || PAYLOAD + size > r.len() {
        return None;
    }
    let sum: u8 = r[4..8].iter().chain(r[PAYLOAD..PAYLOAD + size].iter()).fold(0u8, |s, b| s.wrapping_add(*b));
    if sum != r[3] {
        return None;
    }
    let raw = r[PAYLOAD + BATTERY];
    let percent = raw & 0x7F;
    (percent <= 100).then_some((percent, raw & 0x80 != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn func_config_checksum_and_level() {
        let mut r = [0u8; LEN];
        r[0] = 0xAA;
        r[1] = CMD;
        r[4] = 56;
        r[8 + BATTERY] = 0x55; // 85%, not charging
        let sum: u8 = r[4..8].iter().chain(r[PAYLOAD..PAYLOAD + 56].iter()).fold(0u8, |s, b| s.wrapping_add(*b));
        r[3] = sum;
        assert_eq!(parse(&r), Some((85, false)));
    }
}
