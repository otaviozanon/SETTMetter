//! Poly / Plantronics headsets (Voyager 8200 UC) behind the BT600 USB Bluetooth adapter
//! (047F:02EE / 02F7).
//!
//! Report 0x07 on the vendor collection FFA2:0003, 64 bytes, zero-padded:
//!
//! ```text
//! 07 01 01 10 <len> <addr> 00 00 <op> <grp> <item> <value…>
//! ```
//!
//! Address 00 is the adapter, 20 the headset; replies carry 00 (adapter) or 02 (headset).
//! The adapter needs the attach sequence first (output report `06 01`, a HELLO to the adapter,
//! a HELLO to the headset). Then GET group 0A item 1A answers
//! `<level> <level count> <charging> <minutes hi> <minutes lo>`, and the percentage is
//! `level * 100 / (count - 1)`. Only the battery is read; no setting is written.
//!
//! Protocol from HaloBattery (github.com/HeyOkay/HaloBattery, providers/plantronics.py),
//! itself from HeadsetControl's plantronics_bt600.hpp.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice, HidResult};
use std::collections::HashSet;

const VID: u16 = 0x047F;
const PIDS: [u16; 2] = [0x02EE, 0x02F7];
const PAGE: u16 = 0xFFA2;
const USAGE: u16 = 0x0003;

const REPORT: u8 = 0x07;
const ADDR_DONGLE: u8 = 0x00;
const ADDR_HEADSET: u8 = 0x20;
const REPLY_DONGLE: u8 = 0x00;
const REPLY_HEADSET: u8 = 0x02;
const OP_HELLO: u8 = 0x01;
const OP_GET: u8 = 0x02;
const OP_HELLO_ACK: u8 = 0x08;
const OP_GET_REPLY: u8 = 0x03;
const GRP_HELLO: u8 = 0x01;
const ITEM_HELLO: u8 = 0x02;
const GRP_BATTERY: u8 = 0x0A;
const ITEM_BATTERY: u8 = 0x1A;

#[derive(Default)]
pub struct Plantronics {
    /// Keys that answered once, so a headset that goes quiet is picked up again.
    seen: HashSet<String>,
}

impl Plantronics {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for pid in PIDS {
            let key = format!("plantronics:{pid:04x}");
            let mut state = None;
            for info in api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == pid && d.usage_page() == PAGE && d.usage() == USAGE) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, REPORT).is_none_or(|n| n < 11) || probe::input_len(&desc, REPORT).is_none_or(|n| n < 14) {
                    continue;
                }
                match read_battery(&dev) {
                    Ok(Some((percent, charging))) => {
                        state = Some(State::Battery { percent, charging });
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => diag::event(&key, format!("i/o failed: {e}")),
                }
            }
            match state {
                Some(state) => {
                    self.seen.insert(key.clone());
                    out.push(observation(&key, state));
                }
                None if self.seen.contains(&key) => out.push(observation(&key, State::Unreachable)),
                None => {}
            }
        }
        out
    }
}

fn observation(key: &str, state: State) -> Observation {
    Observation { id: key.into(), name: Some("Poly / Plantronics headset".into()), kind: Some(Kind::Headset), state }
}

fn frame(op: u8, grp: u8, item: u8, addr: u8) -> [u8; 11] {
    [REPORT, 0x01, 0x01, 0x10, if op == OP_GET { 0x06 } else { 0x07 }, addr, 0x00, 0x00, op, grp, item]
}

fn is_reply(r: &[u8], op: u8, grp: u8, item: u8, addr: u8) -> bool {
    r.len() > 10 && r[0] == REPORT && r[1] == 0x01 && r[2] == 0x01 && r[3] == 0x10 && r[5] == addr && r[8] == op && r[9] == grp && r[10] == item
}

/// Waits for the reply to (op, grp, item), skipping events.
fn await_reply(dev: &HidDevice, op: u8, grp: u8, item: u8, addr: u8) -> HidResult<Option<Vec<u8>>> {
    let mut buf = [0u8; 128];
    for _ in 0..16 {
        let n = match dev.read_timeout(&mut buf, 1000) {
            Ok(0) => return Ok(None),
            Ok(n) => n,
            Err(e) => return Err(e),
        };
        if is_reply(&buf[..n], op, grp, item, addr) {
            return Ok(Some(buf[..n].to_vec()));
        }
    }
    Ok(None)
}

fn read_battery(dev: &HidDevice) -> HidResult<Option<(u8, bool)>> {
    drain(dev)?;
    // The adapter's attach nudge; harmless if it is already attached.
    dev.write(&[0x06, 0x01])?;
    dev.write(&frame(OP_HELLO, GRP_HELLO, ITEM_HELLO, ADDR_DONGLE))?;
    let _ = await_reply(dev, OP_HELLO_ACK, GRP_HELLO, ITEM_HELLO, REPLY_DONGLE)?;
    dev.write(&frame(OP_HELLO, GRP_HELLO, ITEM_HELLO, ADDR_HEADSET))?;
    // Missing here: the headset is off, out of range or not paired with the adapter.
    if await_reply(dev, OP_HELLO_ACK, GRP_HELLO, ITEM_HELLO, REPLY_HEADSET)?.is_none() {
        return Ok(None);
    }
    dev.write(&frame(OP_GET, GRP_BATTERY, ITEM_BATTERY, ADDR_HEADSET))?;
    let Some(r) = await_reply(dev, OP_GET_REPLY, GRP_BATTERY, ITEM_BATTERY, REPLY_HEADSET)? else {
        return Ok(None);
    };
    Ok(battery(&r))
}

/// `[11]` level, `[12]` level count, `[13]` charging flag.
fn battery(r: &[u8]) -> Option<(u8, bool)> {
    let level = i32::from(*r.get(11)?);
    let count = i32::from(*r.get(12)?);
    if count < 2 || level >= count {
        return None; // not the adapter's own scale
    }
    let charging = *r.get(13)? != 0;
    Some(((level * 100 / (count - 1)).clamp(0, 100) as u8, charging))
}

fn drain(dev: &HidDevice) -> HidResult<()> {
    let mut buf = [0u8; 128];
    while dev.read_timeout(&mut buf, 0)? > 0 {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery_reply(level: u8, count: u8, charging: u8) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[..11].copy_from_slice(&[0x07, 0x01, 0x01, 0x10, 0x06, 0x02, 0x00, 0x00, 0x03, 0x0A, 0x1A]);
        r[11] = level;
        r[12] = count;
        r[13] = charging;
        r
    }

    #[test]
    fn parses_a_battery_reply() {
        assert_eq!(battery(&battery_reply(3, 4, 0)), Some((100, false)));
        assert_eq!(battery(&battery_reply(2, 4, 1)), Some((66, true)));
        assert_eq!(battery(&battery_reply(0, 2, 0)), Some((0, false)));
    }

    #[test]
    fn rejects_a_different_scale() {
        assert_eq!(battery(&battery_reply(1, 1, 0)), None); // count must be at least 2
        assert_eq!(battery(&battery_reply(4, 4, 0)), None); // level out of range
        assert_eq!(battery(&battery_reply(2, 4, 0)[..13]), None); // no charging byte
    }

    #[test]
    fn reply_matcher_checks_the_header() {
        let reply = battery_reply(3, 4, 0);
        assert!(is_reply(&reply, OP_GET_REPLY, GRP_BATTERY, ITEM_BATTERY, REPLY_HEADSET));
        assert!(!is_reply(&reply, OP_GET_REPLY, GRP_BATTERY, ITEM_BATTERY, REPLY_DONGLE));
        assert!(!is_reply(&reply[..10], OP_GET_REPLY, GRP_BATTERY, ITEM_BATTERY, REPLY_HEADSET));
        // The hello frame asks for 0x07 length, the get frame for 0x06.
        assert_eq!(frame(OP_GET, GRP_BATTERY, ITEM_BATTERY, ADDR_HEADSET)[4], 0x06);
        assert_eq!(frame(OP_HELLO, GRP_HELLO, ITEM_HELLO, ADDR_DONGLE)[4], 0x07);
    }
}
