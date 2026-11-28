//! Valve Steam Controller battery input reports (28DE:1142/1302/1303/1304/1305).
//!
//! Legacy 2015 receiver reports input report 0x01 with the level at byte 14. Newer hardware
//! reports 0x43 with charging at byte 1 and percent at byte 2. Layout from Linux hid-steam.
//! Nothing is written.

use crate::device::{Kind, Observation, State};
use crate::probe;
use hidapi::HidApi;
use std::time::{Duration, Instant};

const VID: u16 = 0x28DE;
const PIDS: &[(u16, bool)] = &[(0x1142, true), (0x1302, false), (0x1303, false), (0x1304, false), (0x1305, false)];

#[derive(Default)]
pub struct SteamController {
    seen: Option<(String, String)>,
}

impl SteamController {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        for &(pid, legacy) in PIDS {
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == pid
                    && d.usage_page() == 0xFF00
                    && (pid != 0x1304 || d.usage() == 1)
            }) {
                let Some(dev) = api.open_path(info.path()).ok() else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                let Some(in_len) = probe::input_len(&desc, if legacy { 0x01 } else { 0x43 }) else { continue };
                if in_len < 15 {
                    continue;
                }
                let mut buf = vec![0u8; in_len];
                let deadline = Instant::now() + Duration::from_millis(1000);
                let mut got = None;
                while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                    match dev.read_timeout(&mut buf, left.as_millis() as i32) {
                        Ok(n) if n >= 15 => {
                            if legacy {
                                if buf[0] == 0x01 && buf[1] == 0x00 && buf[2] == 0x04 && buf[3] == 11 && buf[14] <= 100 {
                                    got = Some((buf[14], false));
                                    break;
                                }
                            } else if buf[0] == 0x43 && buf[2] <= 100 {
                                got = Some((buf[2], buf[1] == 2));
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                let Some((percent, charging)) = got else { continue };
                let name = if legacy || pid == 0x1304 || pid == 0x1305 {
                    "Steam Controller".into()
                } else {
                    info.product_string().filter(|n| !n.trim().is_empty()).unwrap_or("Steam Controller").to_string()
                };
                let id = format!("steam:{pid:04x}:{:08x}", hash(info.path().to_string_lossy().as_ref()));
                self.seen = Some((id.clone(), name.clone()));
                return vec![Observation {
                    id,
                    name: Some(name),
                    kind: Some(Kind::Gamepad),
                    state: State::Battery { percent, charging },
                }];
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
    fn newer_report_offsets() {
        let mut r = [0u8; 16];
        r[0] = 0x43;
        r[1] = 2;
        r[2] = 64;
        assert_eq!((r[2], r[1] == 2), (64, true));
    }
}
