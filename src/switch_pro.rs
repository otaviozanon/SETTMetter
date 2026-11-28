//! Switch Pro-compatible controllers (057E:2009), including GameSir pads in Switch mode.
//!
//! Passive full input report 0x30: bits 7-5 of byte 2 are the level (0-4) and bit 4 is charging.
//! Steam Input or another host must enable full reports; nothing is written.
//! Layout from SDL's SDL_hidapi_switch.c.

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::HidApi;
use std::time::{Duration, Instant};

const VID: u16 = 0x057E;
const PID: u16 = 0x2009;

#[derive(Default)]
pub struct SwitchPro {
    seen: Option<(String, String)>,
}

impl SwitchPro {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == 0x01 && matches!(d.usage(), 0x04 | 0x05)
        }) {
            let Some(dev) = api.open_path(info.path()).ok() else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::input_len(&desc, 0x30).unwrap_or(0) < 13 {
                continue;
            }
            let deadline = Instant::now() + Duration::from_millis(350);
            let mut buf = [0u8; 64];
            while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                match dev.read_timeout(&mut buf, left.as_millis() as i32) {
                    Ok(n) if n >= 13 && buf[0] == 0x30 => {
                        let level = (buf[2] & 0xE0) >> 5;
                        if level > 4 {
                            continue;
                        }
                        let name = info
                            .product_string()
                            .filter(|n| !n.trim().is_empty())
                            .unwrap_or("Nintendo Switch Pro Controller")
                            .to_string();
                        let id = format!("switchpro:{:08x}", hash(info.path().to_string_lossy().as_ref()));
                        self.seen = Some((id.clone(), name.clone()));
                        return vec![Observation {
                            id,
                            name: Some(name),
                            kind: Some(Kind::Gamepad),
                            state: State::Battery { percent: level * 25, charging: (buf[2] & 0x10) != 0 && level < 4 },
                        }];
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
        self.seen.as_ref().map_or_else(Vec::new, |(id, name)| {
            vec![Observation {
                id: id.clone(),
                name: Some(name.clone()),
                kind: Some(Kind::Gamepad),
                state: State::Unreachable,
            }]
        })
    }
}

fn hash(path: &str) -> u32 {
    path.bytes().fold(2_166_136_261u32, |h, b| (h ^ u32::from(b)).wrapping_mul(16_777_619))
}

#[cfg(test)]
mod tests {
    #[test]
    fn level_bits() {
        let b = 0b0110_0000u8;
        assert_eq!((b & 0xE0) >> 5, 3);
        assert_eq!(3 * 25, 75);
        assert_eq!(b & 0x10, 0);
    }
}
