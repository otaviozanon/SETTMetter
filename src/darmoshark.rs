//! Darmoshark M3 / M3S on their Telink receivers and M3S Pro on its NRF receiver.
//!
//! M3/M3S: 248A:FF30 / FF31 receiver, vendor collection 008C:0001, feature report 0x51.
//! A bond query (0x03) must match the paired mouse, then identify (0x06) returns percent at byte 11.
//! M3S Pro: 1915:0723, 64-byte unnumbered vendor reports on FF0A, sum 0xA1; request `41 00 81 01`,
//! reply `41 00 8C 01` with percent at byte 6.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/darmoshark.cpp).

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const TELINK_VID: u16 = 0x248A;
const FEATURE: u8 = 0x51;
const MODELS: &[(u16, u16, &str)] = &[
    (0xFF30, 0xFF12, "Darmoshark M3"),
    (0xFF31, 0xFF18, "Darmoshark M3S"),
];
const NRF_VID: u16 = 0x1915;
const NRF_PID: u16 = 0x0723;
const NRF_FRAME: usize = 64;
const NRF_SUM: u8 = 0xA1;

#[derive(Default)]
pub struct Darmoshark {
    seen: Option<(String, &'static str)>,
}

impl Darmoshark {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        for &(receiver, mouse, name) in MODELS {
            for info in api.device_list().filter(|d| {
                d.vendor_id() == TELINK_VID
                    && d.product_id() == receiver
                    && d.interface_number() == 1
                    && d.usage_page() == 0x008C
                    && d.usage() == 1
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::feature_len(&desc, FEATURE).unwrap_or(0) < 12 {
                    continue;
                }
                if let Some(percent) = read_telink(&dev, mouse) {
                    let id = format!("darmoshark:{receiver:04x}");
                    self.seen = Some((id.clone(), name));
                    return vec![observation(id, name, State::Battery { percent, charging: false })];
                }
            }
        }
        if let Some(percent) = self.nrf(api) {
            let id = "darmoshark:1915:0723".to_string();
            self.seen = Some((id.clone(), "Darmoshark M3S Pro"));
            return vec![observation(id, "Darmoshark M3S Pro", State::Battery { percent, charging: false })];
        }
        if let Some((id, name)) = &self.seen {
            return vec![observation(id.clone(), name, State::Unreachable)];
        }
        Vec::new()
    }

    fn nrf(&self, api: &HidApi) -> Option<u8> {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == NRF_VID
                && d.product_id() == NRF_PID
                && d.interface_number() == 3
                && d.usage_page() == 0xFF0A
                && d.usage() == 1
        }) {
            let dev = api.open_path(info.path()).ok()?;
            let desc = probe::descriptor(&dev)?;
            if probe::output_len(&desc, 0) != Some(NRF_FRAME) || probe::input_len(&desc, 0) != Some(NRF_FRAME) {
                continue;
            }
            if let Some(percent) = read_nrf(&dev) {
                return Some(percent);
            }
        }
        None
    }
}

fn observation(id: String, name: &str, state: State) -> Observation {
    Observation { id, name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

fn read_telink(dev: &HidDevice, mouse: u16) -> Option<u8> {
    let start = Instant::now();
    try_telink(dev, mouse, start + Duration::from_millis(300))
        .or_else(|| try_telink(dev, mouse, start + Duration::from_millis(1000)))
}

fn try_telink(dev: &HidDevice, mouse: u16, deadline: Instant) -> Option<u8> {
    let bond = query(dev, 0x03, deadline)?;
    if read_id(&bond, 3) != TELINK_VID || read_id(&bond, 5) != mouse || bond[7] == 0 {
        return None;
    }
    let identify = query(dev, 0x06, deadline)?;
    if identify[2] != 1
        || read_id(&identify, 4) != TELINK_VID
        || read_id(&identify, 6) != mouse
        || identify[11] > 100
    {
        return None;
    }
    Some(identify[11])
}

fn query(dev: &HidDevice, opcode: u8, deadline: Instant) -> Option<Vec<u8>> {
    let mut req = vec![0u8; 1001];
    req[0] = FEATURE;
    req[1] = opcode;
    while let Ok(n) = dev.read_timeout(&mut [0u8; 64], 0) {
        if n == 0 {
            break;
        }
    }
    dev.send_feature_report(&req).ok()?;
    loop {
        let mut input = [0u8; 64];
        let _ = dev.read_timeout(&mut input, 20);
        if input[0] == 0x54 && input[1] == 0xE4 && input[2] == 0x02 {
            return None;
        }
        let mut reply = vec![0u8; 1001];
        reply[0] = FEATURE;
        if let Ok(n) = dev.get_feature_report(&mut reply) {
            if n >= 12 && reply[0] == FEATURE && reply[1] == opcode {
                return Some(reply[..n].to_vec());
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
    }
}

fn read_id(reply: &[u8], offset: usize) -> u16 {
    u16::from(reply[offset]) | (u16::from(reply[offset + 1]) << 8)
}

fn read_nrf(dev: &HidDevice) -> Option<u8> {
    let mut req = [0u8; NRF_FRAME + 1];
    req[1] = 0x41;
    req[3] = 0x81;
    req[4] = 0x01;
    req[NRF_FRAME] = nrf_checksum(&req[1..NRF_FRAME]);
    let mut buf = [0u8; NRF_FRAME];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
    dev.write(&req).ok()?;
    let deadline = Instant::now() + Duration::from_millis(600);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis().min(100) as i32) {
            Ok(n) if n == NRF_FRAME && buf[0] == 0x41 && buf[2] == 0x8C && buf[3] == 0x01 => {
                if buf[NRF_FRAME - 1] == nrf_checksum(&buf[..NRF_FRAME - 1]) && buf[6] <= 100 {
                    return Some(buf[6]);
                }
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

fn nrf_checksum(frame: &[u8]) -> u8 {
    NRF_SUM.wrapping_sub(frame.iter().fold(0u8, |s, b| s.wrapping_add(*b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telink_ids_are_little_endian() {
        let mut r = vec![0u8; 12];
        r[3] = 0x8A;
        r[4] = 0x24;
        r[5] = 0x12;
        r[6] = 0xFF;
        assert_eq!(read_id(&r, 3), 0x248A);
        assert_eq!(read_id(&r, 5), 0xFF12);
    }

    #[test]
    fn nrf_request_sums_to_the_marker() {
        let mut req = [0u8; NRF_FRAME];
        req[0] = 0x41;
        req[2] = 0x81;
        req[3] = 0x01;
        let checksum = nrf_checksum(&req);
        req[NRF_FRAME - 1] = checksum;
        assert_eq!(req.iter().fold(0u8, |s, b| s.wrapping_add(*b)), NRF_SUM);
    }
}
