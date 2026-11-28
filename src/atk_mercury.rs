//! ATK Mercury I / M1 headset on its 373B:107C receiver.
//!
//! 64-byte feature-report mailbox on FF22:0001. A firmware check (`AA 01`), then two memory
//! reads: the power-pointer address must equal 0x0100FE5C and the capacity address holds the
//! percentage. Read-only.
//! Protocol from Wireless-Device-Battery-Monitor (drivers/atk_mercury.cpp, Joule PR #79).

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::HidApi;
use std::time::Duration;

const VID: u16 = 0x373B;
const PID: u16 = 0x107C;
const REPORT_ID: u8 = 0xAA;
const LEN: usize = 64;
const POWER_POINTER: u32 = 0x0100_FE94;
const EXPECTED_POWER: u32 = 0x0100_FE5C;
const CAPACITY: u32 = 0x0100_FE78;

#[derive(Default)]
pub struct AtkMercury {
    seen: bool,
}

impl AtkMercury {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == 0xFF22 && d.usage() == 0x0001
        }) {
            let Some(dev) = api.open_path(info.path()).ok() else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::feature_len(&desc, REPORT_ID) != Some(LEN) {
                continue;
            }
            if let Some(percent) = read(&dev) {
                self.seen = true;
                return vec![observation(State::Battery { percent, charging: false })];
            }
        }
        if self.seen { vec![observation(State::Unreachable)] } else { Vec::new() }
    }
}

fn observation(state: State) -> Observation {
    Observation {
        id: "atk_mercury:107c".into(),
        name: Some("ATK Mercury I / M1".into()),
        kind: Some(Kind::Headset),
        state,
    }
}

fn read(dev: &hidapi::HidDevice) -> Option<u8> {
    let mut firmware = [0u8; LEN];
    firmware[..2].copy_from_slice(&[REPORT_ID, 0x01]);
    let reply = query(dev, &firmware)?;
    if !known_firmware(&reply) {
        return None;
    }
    let pointer = query(dev, &memory_request(POWER_POINTER))?;
    if parse_memory(&pointer)? != EXPECTED_POWER {
        return None;
    }
    let capacity = query(dev, &memory_request(CAPACITY))?;
    let value = parse_memory(&capacity)?;
    (value <= 100).then_some(value as u8)
}

fn query(dev: &hidapi::HidDevice, request: &[u8; LEN]) -> Option<[u8; LEN]> {
    dev.send_feature_report(request).ok()?;
    std::thread::sleep(Duration::from_millis(300));
    let mut reply = [0u8; LEN];
    reply[0] = request[0];
    let n = dev.get_feature_report(&mut reply).ok()?;
    (n == LEN).then_some(reply)
}

fn known_firmware(reply: &[u8; LEN]) -> bool {
    reply.starts_with(&[0xAA, 0x01, 0x00, 0x00, 0x00, 0x07])
        || reply.starts_with(&[0xAA, 0x01, 0x00, 0x00, 0x01, 0x03, 0xFF, 0x25, 0x05, 0x08, 0x11, 0x34])
}

fn memory_request(address: u32) -> [u8; LEN] {
    let mut r = [0u8; LEN];
    r[..9].copy_from_slice(&[REPORT_ID, 0x03, 0x00, 0x01, 0x04, (address >> 24) as u8, (address >> 16) as u8, (address >> 8) as u8, address as u8]);
    r
}

fn parse_memory(reply: &[u8; LEN]) -> Option<u32> {
    if reply[0] != REPORT_ID || reply[1] != 0x03 || reply[2] != 0x04 {
        return None;
    }
    Some(u32::from(reply[3]) | (u32::from(reply[4]) << 8) | (u32::from(reply[5]) << 16) | (u32::from(reply[6]) << 24))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_frame_and_value() {
        let r = memory_request(CAPACITY);
        assert_eq!(r[0], REPORT_ID);
        assert_eq!(&r[5..9], &[0x01, 0x00, 0xFE, 0x78]);
        let mut reply = [0u8; LEN];
        reply[..7].copy_from_slice(&[REPORT_ID, 0x03, 0x04, 0x64, 0, 0, 0]);
        assert_eq!(parse_memory(&reply), Some(100));
    }
}
