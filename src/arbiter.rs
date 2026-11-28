//! Arbiter Studio Akitsu wireless mice on the shared DriverCore vendor id 0x373E:
//!   * Akitsu (PAW3395):       0x0009 USB cable, 0x000A 8K receiver
//!   * Akitsu 3950 (PAW3950):  0x0038 USB cable, 0x0039 8K receiver
//!   * Akitsu Medium (PAW3950): 0x0066 USB cable, 0x0067 8K receiver
//!
//! Protocol and model ids from Arbiter's web HUB ("IsNewProtocol" getBatPer() in the DriverCore
//! bundle, same family as LAMZU and WLMouse): the vendor collection FFFF:0000 carries a 65-byte
//! feature report 0. The request is `00 00 00 02 02 00 83 …`; about 100 ms later report 0 reads
//! back `<status> .. .. 02 .. 83 <charging> <percent>` (with or without the leading report id
//! byte). Status 0xA1 / 0xA2 = answered, 0xA0 = mouse asleep; charging 1 = on the cable; a
//! receiver answering percent 0 means the mouse is off or not linked.
//!
//! Read-only: no settings or mode writes are sent. Cables are listed before receivers so a cable
//! that answers claims its paired receiver and both share one id.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::Duration;

const VID: u16 = 0x373E;
const LEN: usize = 65;
const MARKER: u8 = 0x83;

struct Model {
    pid: u16,
    receiver: bool,
    name: &'static str,
    /// The receiver this cable pairs with, if any.
    receiver_pid: u16,
}

const MODELS: &[Model] = &[
    Model { pid: 0x0009, receiver: false, name: "Arbiter Studio Akitsu", receiver_pid: 0x000A },
    Model { pid: 0x0038, receiver: false, name: "Arbiter Studio Akitsu 3950", receiver_pid: 0x0039 },
    Model { pid: 0x0066, receiver: false, name: "Arbiter Studio Akitsu Medium", receiver_pid: 0x0067 },
    Model { pid: 0x000A, receiver: true, name: "Arbiter Studio Akitsu", receiver_pid: 0 },
    Model { pid: 0x0039, receiver: true, name: "Arbiter Studio Akitsu 3950", receiver_pid: 0 },
    Model { pid: 0x0067, receiver: true, name: "Arbiter Studio Akitsu Medium", receiver_pid: 0 },
];

#[derive(PartialEq, Debug)]
enum Reply {
    Battery { percent: u8, charging: bool },
    Asleep,
    None,
}

#[derive(Default)]
pub struct Arbiter {
    seen: HashSet<String>,
}

impl Arbiter {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out: Vec<Observation> = Vec::new();
        for model in MODELS {
            // One id per cable/receiver pair, so charging updates the existing row.
            let key = format!("arbiter:{VID:04x}:{:04x}", if model.receiver_pid != 0 { model.receiver_pid } else { model.pid });
            let mut colls: Vec<_> = api
                .device_list()
                .filter(|d| d.vendor_id() == VID && d.product_id() == model.pid && d.usage_page() == 0xFFFF)
                .collect();
            // Usage 0 first, as the C++ driver sorts.
            colls.sort_by_key(|d| !(d.usage() == 0));

            let mut handled = false;
            for info in colls {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                // The 65-byte request means 64 payload bytes plus the id byte.
                if !matches!(probe::feature_len(&desc, 0), Some(64 | 65)) {
                    continue;
                }
                match query(&dev) {
                    Ok(Reply::None) => continue,
                    Err(e) => {
                        diag::event(&key, format!("i/o failed: {e}"));
                        continue;
                    }
                    Ok(Reply::Asleep) => {
                        handled = true;
                        self.seen.insert(key.clone());
                        merge(&mut out, observation(&key, model.name, State::Unreachable));
                        break;
                    }
                    Ok(Reply::Battery { percent, charging }) => {
                        handled = true;
                        // 0 from a dongle: the mouse is off or out of range.
                        let connected = percent > 0 || !model.receiver;
                        let state = if connected {
                            State::Battery { percent, charging }
                        } else {
                            State::Unreachable
                        };
                        self.seen.insert(key.clone());
                        merge(&mut out, observation(&key, model.name, state));
                        break;
                    }
                }
            }
            if !handled && self.seen.contains(&key) {
                merge(&mut out, observation(&key, model.name, State::Unreachable));
            }
        }
        out
    }
}

fn observation(key: &str, name: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(Kind::Mouse), state }
}

/// Cable and receiver share an id; the cable's charging reading wins over the receiver's.
fn merge(out: &mut Vec<Observation>, o: Observation) {
    if let Some(old) = out.iter_mut().find(|x| x.id == o.id) {
        if matches!(o.state, State::Battery { charging: true, .. }) && !matches!(old.state, State::Battery { charging: true, .. }) {
            *old = o;
        }
        return;
    }
    out.push(o);
}

fn query(dev: &HidDevice) -> hidapi::HidResult<Reply> {
    let mut req = [0u8; LEN];
    req[3] = 0x02;
    req[4] = 0x02;
    req[6] = MARKER;
    for _ in 0..2 {
        dev.send_feature_report(&req)?;
        for read in 0..3 {
            std::thread::sleep(Duration::from_millis(if read == 0 { 100 } else { 60 }));
            let mut buf = [0u8; LEN];
            let n = dev.get_feature_report(&mut buf)?;
            let reply = decode(&buf[..n]);
            if reply != Reply::None {
                return Ok(reply);
            }
        }
    }
    Ok(Reply::None)
}

/// `<status> .. .. 02 .. 83 <charging> <percent>`, with or without the leading report id byte.
fn decode(r: &[u8]) -> Reply {
    let (status, charging, percent) = if r.len() >= 9 && r[4] == 0x02 && r[6] == MARKER {
        (r[1], r[7], r[8])
    } else if r.len() >= 8 && r[3] == 0x02 && r[5] == MARKER {
        (r[0], r[6], r[7])
    } else {
        return Reply::None;
    };
    if status == 0xA0 {
        return Reply::Asleep;
    }
    if status != 0xA1 && status != 0xA2 {
        return Reply::None;
    }
    if percent > 100 {
        return Reply::None;
    }
    Reply::Battery { percent, charging: charging == 0x01 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(percent: u8, charging: u8, status: u8) -> Vec<u8> {
        let mut r = vec![0u8; LEN];
        r[1] = status;
        r[4] = 0x02;
        r[6] = MARKER;
        r[7] = charging;
        r[8] = percent;
        r
    }

    #[test]
    fn decodes_both_reply_shapes() {
        // Akitsu Medium on its 8K receiver at 64%, not charging.
        assert_eq!(decode(&reply(64, 0, 0xA1)), Reply::Battery { percent: 64, charging: false });
        assert_eq!(decode(&reply(100, 1, 0xA2)), Reply::Battery { percent: 100, charging: true });
        // Same answer without the leading report id byte.
        assert_eq!(decode(&reply(55, 0, 0xA1)[1..]), Reply::Battery { percent: 55, charging: false });
    }

    #[test]
    fn asleep_and_bad_replies() {
        assert_eq!(decode(&reply(0, 0, 0xA0)), Reply::Asleep);
        assert_eq!(decode(&reply(50, 0, 0x01)), Reply::None); // other status
        assert_eq!(decode(&reply(101, 0, 0xA1)), Reply::None); // out of range
        let mut wrong = reply(50, 0, 0xA1);
        wrong[6] = 0x00; // no marker
        assert_eq!(decode(&wrong), Reply::None);
        assert_eq!(decode(&reply(50, 0, 0xA1)[..8]), Reply::None); // short
    }
}
