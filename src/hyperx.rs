//! HyperX wireless headsets and Pulsefire mice (HP id 03F0, older Kingston id 0951).
//!
//! Eleven protocol variants live behind these two vendor ids, each on its own vendor
//! collection. Every query here is read-only: link state, charge flag and level (some also
//! answer the cell voltage, which the app has no field for and is not decoded).
//!
//! - Cloud III Wireless (and the Cloud II Core Wireless that shares its protocol): output
//!   report 0x66, command 0x82 link / 0x8A charging / 0x89 level at `[4]`.
//! - Cloud II Wireless: report 0x06 `FF BB <cmd>`, level at `[7]`, charging at `[4]` of
//!   command 0x03.
//! - Cloud II Wireless (Kingston 0951:1718) and Cloud Flight S: fixed report 0x06 frame,
//!   reply `0B 00 BB <cmd>`; the Kingston dongle needs a report-6 input read first.
//! - Cloud Alpha Wireless: `21 BB <cmd>`; 0x03 link, 0x0C charging, 0x0B level at `[3]`.
//! - Cloud Alpha 2 Wireless: `52 01` link, `50 02` level at `[2]` and charging at `[3]`.
//! - Pulsefire Haste Wireless (original): report id 0, `51` asks, level at `[4]`, charging at `[5]`.
//! - Pulsefire Haste 2 (Core / Mini) Wireless: the same `52 01` / `50 02` pair as Alpha 2.
//! - Pulsefire Dart: report id 0, `51` heartbeat; only `51 00 00 03 <level> <charging>` is a level.
//! - Cloud Flight: `21 FF 05`, cell voltage at `[3..5]`, level from a curve.
//! - Cloud III S Wireless: report 0x0C `02 03 01 00 <cmd>`; 0x02 link, 0x06 level, 0x48 charging.
//! - Cloud Stinger Core Wireless (and Cloud Core Wireless DTS): feature report 0xFF, level
//!   from the cell voltage in `[11..13]`, charge code in `[9]`.
//!
//! Protocol and ids from Wireless-Device-Battery-Monitor (drivers/hyperx.cpp), which credits
//! HasteWireless captures on hardware, HyperXWM, HyperHeadset, HyperBatteryMonitor,
//! HyperX-Cloud-2-Battery-Monitor, the Pulsefire Dart reverse-engineering notes, PlasmaNGenuity
//! and HeadsetControl. Not verified in this project.
//!
//! Skipped on purpose: the 6 s settle window Haste 2 needs after power-on (it reports 40% for
//! a moment) is grace logic; the level right after such a power-on can read low until the
//! next scan.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{DeviceInfo, HidApi, HidDevice, HidResult};
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// A receiver answers within about a second.
const WAIT: Duration = Duration::from_secs(1);

struct Model {
    vid: u16,
    pids: &'static [u16],
    /// Which collection carries the protocol.
    accept: fn(&DeviceInfo, &[u8]) -> bool,
    name: fn(u16) -> &'static str,
    kind: Kind,
    poll: fn(&HidDevice, u16) -> HidResult<Option<State>>,
}

const MODELS: &[Model] = &[
    // Cloud III Wireless: report 0x66, 62-byte reports.
    Model { vid: 0x03F0, pids: &[0x0FCC, 0x05B7, 0x0C9D], accept: cloud3_accept, name: cloud3_name, kind: Kind::Headset, poll: cloud3 },
    // Cloud II Core Wireless: same protocol, 64-byte reports (HyperHeadset).
    Model { vid: 0x03F0, pids: &[0x069F, 0x0995], accept: cloud3_core_accept, name: cloud3_name, kind: Kind::Headset, poll: cloud3 },
    // Cloud III S Wireless: report 0x0C on the 64/64-byte collection.
    Model { vid: 0x03F0, pids: &[0x06BE, 0x02CC], accept: cloud3s_accept, name: cloud3s_name, kind: Kind::Headset, poll: cloud3s },
    // Cloud II Wireless, vendor page FF90.
    Model { vid: 0x03F0, pids: &[0x0696, 0x018B, 0x0D93, 0x0C8C], accept: cloud2_accept, name: cloud2_name, kind: Kind::Headset, poll: cloud2 },
    // Cloud II Wireless (Kingston dongle).
    Model { vid: 0x0951, pids: &[0x1718], accept: kingston_accept, name: cloud2_name, kind: Kind::Headset, poll: cloud2_kingston },
    // Cloud Flight S shares the Cloud II Wireless read-only protocol.
    Model { vid: 0x0951, pids: &[0x16EA, 0x16EB], accept: flight_s_accept, name: kingston_name, kind: Kind::Headset, poll: cloud2_kingston },
    Model { vid: 0x03F0, pids: &[0x098D], accept: alpha_accept, name: alpha_name, kind: Kind::Headset, poll: cloud_alpha },
    Model { vid: 0x03F0, pids: &[0x08BE], accept: alpha2_accept, name: alpha2_name, kind: Kind::Headset, poll: cloud_alpha2 },
    Model { vid: 0x03F0, pids: &[0x028E, 0x048E], accept: haste_accept, name: haste_name, kind: Kind::Mouse, poll: haste_original },
    Model { vid: 0x03F0, pids: &[0x0AB5, 0x0D97, 0x0F98, 0x0DA0], accept: haste2_accept, name: haste2_name, kind: Kind::Mouse, poll: haste_wireless },
    Model { vid: 0x0951, pids: &[0x16E1], accept: dart_accept, name: dart_name, kind: Kind::Mouse, poll: pulsefire_dart },
    Model { vid: 0x0951, pids: &[0x16C4, 0x1723, 0x1749], accept: cloud_flight_accept, name: cloud_flight_name, kind: Kind::Headset, poll: cloud_flight },
    Model { vid: 0x03F0, pids: &[0x0E90], accept: cloud_flight_accept, name: cloud_flight_name, kind: Kind::Headset, poll: cloud_flight },
    Model { vid: 0x03F0, pids: &[0x0A93, 0x0A8E], accept: stinger_accept, name: stinger_name, kind: Kind::Headset, poll: stinger_core },
    Model { vid: 0x0951, pids: &[0x170B, 0x1713, 0x173F], accept: stinger_accept, name: stinger_name, kind: Kind::Headset, poll: stinger_core },
];

#[derive(Default)]
pub struct Hyperx {
    /// Keys that answered once: a sleeping device keeps its last reading.
    seen: HashSet<String>,
}

impl Hyperx {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        for model in MODELS {
            for &pid in model.pids {
                let key = format!("hyperx:{pid:04x}");
                let mut state = None;
                for info in api.device_list().filter(|d| d.vendor_id() == model.vid && d.product_id() == pid) {
                    let Ok(dev) = api.open_path(info.path()) else { continue };
                    let Some(desc) = probe::descriptor(&dev) else { continue };
                    if !(model.accept)(info, &desc) {
                        continue;
                    }
                    match (model.poll)(&dev, pid) {
                        Ok(Some(s)) => {
                            state = Some(s);
                            break;
                        }
                        Ok(None) => {}
                        Err(e) => diag::event(&key, format!("i/o failed: {e}")),
                    }
                }
                match state {
                    Some(state) => {
                        self.seen.insert(key.clone());
                        out.push(observation(&key, (model.name)(pid), model.kind, state));
                    }
                    None if self.seen.contains(&key) => {
                        out.push(observation(&key, (model.name)(pid), model.kind, State::Unreachable));
                    }
                    None => {}
                }
            }
        }
        out
    }
}

fn observation(key: &str, name: &str, kind: Kind, state: State) -> Observation {
    Observation { id: key.into(), name: Some(name.into()), kind: Some(kind), state }
}

fn cloud3_accept(_: &DeviceInfo, desc: &[u8]) -> bool {
    probe::output_len(desc, 0x66) == Some(62) && probe::input_len(desc, 0x66) == Some(62)
}

fn cloud3_core_accept(info: &DeviceInfo, desc: &[u8]) -> bool {
    info.usage_page() >= 0xFF00
        && probe::output_len(desc, 0x66).is_some_and(|n| n >= 62)
        && probe::input_len(desc, 0x66).is_some_and(|n| n >= 62)
}

fn cloud2_accept(info: &DeviceInfo, desc: &[u8]) -> bool {
    info.usage_page() == 0xFF90 && info.usage() == 0x0303 && probe::output_len(desc, 0x06).is_some_and(|n| n >= 4)
}

fn kingston_accept(_: &DeviceInfo, desc: &[u8]) -> bool {
    probe::output_len(desc, 0x06) == Some(62)
}

fn flight_s_accept(info: &DeviceInfo, desc: &[u8]) -> bool {
    info.usage_page() == 0xFF13
        && info.usage() == 0x0001
        && probe::input_len(desc, 0x06) == Some(62)
        && probe::output_len(desc, 0x06) == Some(62)
}

fn alpha_accept(info: &DeviceInfo, desc: &[u8]) -> bool {
    info.usage_page() >= 0xFF00 && probe::output_len(desc, 0x21).is_some_and(|n| n >= 3)
}

fn alpha2_accept(info: &DeviceInfo, desc: &[u8]) -> bool {
    info.usage_page() == 0xFF13 && info.usage() == 0xFF00 && probe::output_len(desc, 0x52).is_some_and(|n| n >= 2)
}

fn haste_accept(info: &DeviceInfo, desc: &[u8]) -> bool {
    info.usage_page() == 0xFF00
        && info.usage() == 0x0001
        && probe::output_len(desc, 0).is_some_and(|n| n >= 64)
        && probe::input_len(desc, 0).is_some_and(|n| n >= 64)
}

fn haste2_accept(info: &DeviceInfo, desc: &[u8]) -> bool {
    info.usage_page() >= 0xFF00
        && probe::output_len(desc, 0x52).is_some_and(|n| n >= 2)
        && probe::input_len(desc, 0x53).is_some_and(|n| n >= 8)
}

fn dart_accept(info: &DeviceInfo, desc: &[u8]) -> bool {
    info.usage_page() == 0xFF00 && probe::output_len(desc, 0).is_some_and(|n| n >= 1) && probe::input_len(desc, 0).is_some_and(|n| n >= 5)
}

fn cloud_flight_accept(info: &DeviceInfo, desc: &[u8]) -> bool {
    info.usage_page() >= 0xFF00 && probe::output_len(desc, 0x21).is_some_and(|n| n >= 3) && probe::input_len(desc, 0x21).is_some_and(|n| n >= 5)
}

fn stinger_accept(info: &DeviceInfo, desc: &[u8]) -> bool {
    info.usage_page() == 0xFF00 && info.usage() == 0x0001 && probe::feature_len(desc, 0xFF).is_some_and(|n| n >= 32)
}

fn cloud3_name(pid: u16) -> &'static str {
    if matches!(pid, 0x069F | 0x0995) {
        "HyperX Cloud II Core Wireless"
    } else {
        "HyperX Cloud III Wireless"
    }
}

fn cloud2_name(_: u16) -> &'static str {
    "HyperX Cloud II Wireless"
}

fn kingston_name(pid: u16) -> &'static str {
    if matches!(pid, 0x16EA | 0x16EB) {
        "HyperX Cloud Flight S"
    } else {
        "HyperX Cloud II Wireless"
    }
}

fn alpha_name(_: u16) -> &'static str {
    "HyperX Cloud Alpha Wireless"
}

fn alpha2_name(_: u16) -> &'static str {
    "HyperX Cloud Alpha 2 Wireless"
}

fn haste_name(_: u16) -> &'static str {
    "HyperX Pulsefire Haste Wireless"
}

fn haste2_name(pid: u16) -> &'static str {
    match pid {
        0x0AB5 => "HyperX Pulsefire Haste 2 Core Wireless",
        0x0DA0 => "HyperX Pulsefire Haste 2 Mini",
        _ => "HyperX Pulsefire Haste 2 Wireless",
    }
}

fn dart_name(_: u16) -> &'static str {
    "HyperX Pulsefire Dart"
}

fn cloud_flight_name(_: u16) -> &'static str {
    "HyperX Cloud Flight"
}

fn stinger_name(pid: u16) -> &'static str {
    match pid {
        0x173F => "HyperX Cloud Core Wireless",
        0x0A8E => "HyperX Cloud Stinger Core Wireless DTS",
        _ => "HyperX Cloud Stinger Core Wireless",
    }
}

/// Writes `req` and returns the first input report accepted by `ok` before the wait ends.
fn query(dev: &HidDevice, req: &[u8], wait: Duration, ok: impl Fn(&[u8]) -> bool) -> HidResult<Option<Vec<u8>>> {
    dev.write(req)?;
    let mut buf = [0u8; 128];
    let deadline = Instant::now() + wait;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis().max(1) as i32) {
            Ok(0) => break,
            Ok(n) if ok(&buf[..n]) => return Ok(Some(buf[..n].to_vec())),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

fn cloud3_query(dev: &HidDevice, cmd: u8) -> HidResult<Option<Vec<u8>>> {
    query(dev, &[0x66, cmd], Duration::from_millis(800), |r| r.len() >= 5 && r[0] == 0x66 && r[1] == cmd)
}

fn cloud3(dev: &HidDevice, _: u16) -> HidResult<Option<State>> {
    let Some(link) = cloud3_query(dev, 0x82)? else { return Ok(None) };
    if link[2] == 0 {
        return Ok(Some(State::Unreachable));
    }
    let charging = cloud3_query(dev, 0x8A)?.is_some_and(|r| r[2] == 1);
    let Some(level) = cloud3_query(dev, 0x89)? else { return Ok(Some(State::Unreachable)) };
    if level[4] > 100 {
        return Ok(Some(State::Unreachable));
    }
    Ok(Some(State::Battery { percent: level[4], charging }))
}

fn cloud2_query(dev: &HidDevice, cmd: u8) -> HidResult<Option<Vec<u8>>> {
    query(dev, &[0x06, 0xFF, 0xBB, cmd], WAIT, |r| r.len() >= 8 && r[0] == 0x06 && r[1] == 0xFF && r[2] == 0xBB && r[3] == cmd)
}

fn cloud2(dev: &HidDevice, _: u16) -> HidResult<Option<State>> {
    let Some(r) = cloud2_query(dev, 0x02)? else { return Ok(None) };
    let Some(percent) = cloud2_level(&r) else { return Ok(None) };
    let charging = cloud2_query(dev, 0x03)?.is_some_and(|c| c[4] == 0x01);
    Ok(Some(State::Battery { percent, charging }))
}

fn cloud2_level(r: &[u8]) -> Option<u8> {
    let level = *r.get(7)?;
    (level <= 100).then_some(level)
}

fn cloud2_kingston(dev: &HidDevice, pid: u16) -> HidResult<Option<State>> {
    let cloud2 = pid == 0x1718;
    let ask = |cmd: u8| -> HidResult<Option<Vec<u8>>> {
        if cloud2 {
            // The Kingston dongle drops the write unless a report-6 input read comes first.
            let mut echo = [0u8; 128];
            echo[0] = 0x06;
            let _ = dev.get_input_report(&mut echo);
        }
        let req = [0x06, 0x00, 0x02, 0x00, 0x9A, 0x00, 0x00, 0x68, 0x4A, 0x8E, 0x0A, 0x00, 0x00, 0x00, 0xBB, cmd, 0x00];
        query(dev, &req, WAIT, |r| r.len() >= 8 && r[0] == 0x0B && r[2] == 0xBB && r[3] == cmd)
    };
    let Some(r) = ask(0x02)? else { return Ok(None) };
    if r[7] > 100 {
        return Ok(None);
    }
    // The Kingston dongle answers a single write per handle: charging is not readable.
    let charging = if cloud2 { false } else { ask(0x03)?.is_some_and(|c| c[4] == 0x01) };
    Ok(Some(State::Battery { percent: r[7], charging }))
}

fn alpha_query(dev: &HidDevice, cmd: u8) -> HidResult<Option<Vec<u8>>> {
    query(dev, &[0x21, 0xBB, cmd], WAIT, |r| r.len() >= 4 && r[0] == 0x21 && r[2] == cmd)
}

fn cloud_alpha(dev: &HidDevice, _: u16) -> HidResult<Option<State>> {
    let Some(link) = alpha_query(dev, 0x03)? else { return Ok(None) };
    if link[3] == 0x01 {
        return Ok(Some(State::Unreachable));
    }
    let charging = alpha_query(dev, 0x0C)?.is_some_and(|r| r[3] == 0x01);
    let Some(level) = alpha_query(dev, 0x0B)? else { return Ok(Some(State::Unreachable)) };
    if level[3] > 100 {
        return Ok(Some(State::Unreachable));
    }
    Ok(Some(State::Battery { percent: level[3], charging }))
}

/// Command `cmd` is answered by report `cmd + 1`, sub-command echoed in byte 1.
fn pair_query(dev: &HidDevice, cmd: u8, sub: u8) -> HidResult<Option<Vec<u8>>> {
    query(dev, &[cmd, sub], WAIT, move |r| r.len() >= 8 && r[0] == cmd.wrapping_add(1) && r[1] == sub)
}

fn cloud_alpha2(dev: &HidDevice, _: u16) -> HidResult<Option<State>> {
    let Some(link) = pair_query(dev, 0x52, 0x01)? else { return Ok(None) };
    if link[2] == 0 {
        return Ok(Some(State::Unreachable));
    }
    let Some(bat) = pair_query(dev, 0x50, 0x02)? else { return Ok(Some(State::Unreachable)) };
    if bat[2] > 100 {
        return Ok(Some(State::Unreachable));
    }
    Ok(Some(State::Battery { percent: bat[2], charging: bat[3] == 0x01 }))
}

fn haste_wireless(dev: &HidDevice, _: u16) -> HidResult<Option<State>> {
    // After a reconnect the dongle floods the channel, so a missing link reply is not an error.
    let link = pair_query(dev, 0x52, 0x01)?;
    let linked = link.as_ref().is_none_or(|r| r[2] != 0);
    let battery = if linked { pair_query(dev, 0x50, 0x02)?.filter(|r| r[2] <= 100) } else { None };
    match battery {
        Some(r) => Ok(Some(State::Battery { percent: r[2], charging: matches!(r[3], 0x01 | 0x02) })),
        None if link.is_none() => Ok(None), // neither query answered: not this protocol right now
        None => Ok(Some(State::Unreachable)),
    }
}

fn haste_original(dev: &HidDevice, _: u16) -> HidResult<Option<State>> {
    let Some(r) = query(dev, &[0x00, 0x51], Duration::from_millis(800), |r| r.len() >= 6 && r[0] == 0x51)? else {
        return Ok(None);
    };
    Ok(Some(match haste_level(&r) {
        Some((percent, charging)) => State::Battery { percent, charging },
        // 0 and FF mean unavailable on this protocol, notably in wired mode.
        None => State::Unreachable,
    }))
}

fn haste_level(r: &[u8]) -> Option<(u8, bool)> {
    let level = *r.get(4)?;
    let charge = *r.get(5)?;
    (1..=100).contains(&level).then_some((level, matches!(charge, 0x01 | 0x02)))
}

fn pulsefire_dart(dev: &HidDevice, _: u16) -> HidResult<Option<State>> {
    let Some(r) = query(dev, &[0x00, 0x51], WAIT, |r| r.len() >= 6 && r[0] == 0x51)? else { return Ok(None) };
    // Any other heartbeat reply means the mouse is asleep without a level.
    Ok(Some(match dart_level(&r) {
        Some((percent, charging)) => State::Battery { percent, charging },
        None => State::Unreachable,
    }))
}

/// Only `51 00 00 03 <level> <charging>` carries a level; `51 00 02 EC 05` is not one.
fn dart_level(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 6 || r[0] != 0x51 || r[1] != 0x00 || r[2] != 0x00 || r[3] != 0x03 || r[4] > 100 {
        return None;
    }
    Some((r[4], r[5] == 0x01))
}

fn cloud_flight(dev: &HidDevice, _: u16) -> HidResult<Option<State>> {
    let Some(r) = query(dev, &[0x21, 0xFF, 0x05], WAIT, |r| r.len() >= 5)? else { return Ok(None) };
    let mv = i32::from(r[3]) << 8 | i32::from(r[4]);
    let (percent, charging) = flight_percent(mv);
    Ok(Some(State::Battery { percent, charging }))
}

/// Above 0x100B the terminal voltage means charging (level taken as full): the discharging
/// curve below it is from HeadsetControl.
fn flight_percent(mv: i32) -> (u8, bool) {
    if mv > 0x100B {
        return (100, true);
    }
    if mv <= 3648 {
        return (((0.00125 * f64::from(mv)) as i32).clamp(0, 100) as u8, false);
    }
    if mv > 3975 {
        return (100, false);
    }
    let v = f64::from(mv);
    let pct = 0.00000002547505 * v.powi(4) - 0.0003900299 * v.powi(3) + 2.238321 * v * v - 5706.256 * v + 5452299.0;
    (pct.round().clamp(0.0, 100.0) as u8, false)
}

fn cloud3s_accept(_: &DeviceInfo, desc: &[u8]) -> bool {
    probe::output_len(desc, 0x0C) == Some(64) && probe::input_len(desc, 0x0C) == Some(64)
}

fn cloud3s_name(_: u16) -> &'static str {
    "HyperX Cloud III S Wireless"
}

fn cloud3s_query(dev: &HidDevice, cmd: u8) -> HidResult<Option<Vec<u8>>> {
    query(dev, &[0x0C, 0x02, 0x03, 0x01, 0x00, cmd], WAIT, move |r| {
        r.len() >= 7 && r[0] == 0x0C && r[5] == cmd && r[6] != 0xFF
    })
}

fn cloud3s(dev: &HidDevice, _: u16) -> HidResult<Option<State>> {
    let Some(link) = cloud3s_query(dev, 0x02)? else { return Ok(None) };
    if link[6] != 2 {
        return Ok(Some(State::Unreachable));
    }
    let percent = match cloud3s_query(dev, 0x06)? {
        Some(r) if r[6] <= 100 => Some(r[6]),
        _ => None,
    };
    let charging = cloud3s_query(dev, 0x48)?.is_some_and(|r| matches!(r[6], 1 | 2));
    Ok(Some(match percent {
        Some(percent) => State::Battery { percent, charging },
        None => State::Unreachable,
    }))
}

const CONNECTION: [u8; 20] =
    [0xFF, 0x07, 0x00, 0xFD, 0x04, 0x00, 0x00, 0x02, 0x82, 0x03, 0x05, 0x29, 0x0F, 0x00, 0x00, 0x03, 0x68, 0x10, 0x00, 0x00];
const BATTERY: [u8; 12] = [0xFF, 0x07, 0x00, 0xFD, 0x04, 0x0C, 0xF1, 0x02, 0x01, 0x04, 0xF0, 0x0C];
const PCT: [i32; 11] = [100, 90, 80, 70, 60, 50, 40, 30, 20, 10, 0];
const CHARGING_MV: [i32; 11] = [4130, 4090, 4050, 4000, 3967, 3946, 3930, 3916, 3871, 3831, 3400];
const DISCHARGING_MV: [i32; 11] = [4121, 4042, 3954, 3905, 3838, 3785, 3763, 3738, 3700, 3651, 3517];

/// SetFeature, then poll feature report 0xFF for the fresh reply (a stale one can sit there
/// for the first 20 ms).
fn exchange(dev: &HidDevice, cmd: &[u8], good: fn(&[u8]) -> bool) -> HidResult<(bool, Vec<u8>)> {
    dev.send_feature_report(cmd)?;
    let mut last = Vec::new();
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(20));
        let mut buf = [0u8; 128];
        buf[0] = 0xFF;
        let n = dev.get_feature_report(&mut buf)?;
        last = buf[..n].to_vec();
        if last.len() >= 13 && good(&last) {
            return Ok((true, last));
        }
    }
    Ok((false, last))
}

fn stinger_core(dev: &HidDevice, _: u16) -> HidResult<Option<State>> {
    let stale_ok = |r: &[u8]| !(r[1] == 0x01 && r[2] == 0x00) && !(r[1] == 0x12 && r[2] == 0x05);
    let (fresh, link) = exchange(dev, &CONNECTION, stale_ok)?;
    if !fresh && !(link.len() >= 13 && link[1] == 0x01 && link[2] == 0x00) {
        return Ok(None);
    }
    if link[11] != 0x01 {
        return Ok(Some(State::Unreachable));
    }
    let (fresh, r) = exchange(dev, &BATTERY, |r| r[0] == 0xFF && r[1] == 0x12 && r[2] == 0x05)?;
    if !fresh {
        return Ok(Some(State::Unreachable));
    }
    Ok(Some(stinger_battery(&r).unwrap_or(State::Unreachable)))
}

/// `FF 12 05`, code `[9]` (03 discharging / 05 charging / 06 full), cell mV in `[11..13]`.
fn stinger_battery(r: &[u8]) -> Option<State> {
    if r.len() < 13 {
        return None;
    }
    let code = r[9];
    if !matches!(code, 0x03 | 0x05 | 0x06) {
        return None;
    }
    let charging = code == 0x05;
    let percent = if code == 0x06 {
        100
    } else {
        let mv = i32::from(r[11]) | i32::from(r[12]) << 8;
        interpolate(mv, if charging { &CHARGING_MV } else { &DISCHARGING_MV })
    };
    Some(State::Battery { percent, charging })
}

/// Piecewise-linear voltage (mV) to percent, tables sorted by descending voltage.
fn interpolate(mv: i32, table: &[i32; 11]) -> u8 {
    if mv >= table[0] {
        return PCT[0] as u8;
    }
    for i in 1..table.len() {
        if mv >= table[i] {
            let span = table[i - 1] - table[i];
            let pct = PCT[i] + (PCT[i - 1] - PCT[i]) * (mv - table[i]) / if span == 0 { 1 } else { span };
            return pct.clamp(0, 100) as u8;
        }
    }
    PCT[10] as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cloud2_reply(cmd: u8, level: u8, charge: u8) -> Vec<u8> {
        let mut r = vec![0u8; 62];
        r[..4].copy_from_slice(&[0x06, 0xFF, 0xBB, cmd]);
        r[4] = charge;
        r[7] = level;
        r
    }

    #[test]
    fn cloud2_parses_its_reply() {
        assert_eq!(cloud2_level(&cloud2_reply(0x02, 87, 0)), Some(87));
        assert_eq!(cloud2_level(&cloud2_reply(0x02, 101, 0)), None);
        assert_eq!(cloud2_level(&[0x06, 0xFF, 0xBB]), None); // short
    }

    #[test]
    fn flight_curve_branches() {
        assert_eq!(flight_percent(4130), (100, true)); // above 0x100B: charging
        assert_eq!(flight_percent(4107), (100, false));
        assert_eq!(flight_percent(3600), (4, false)); // 0.00125 * 3600 = 4.5, truncated
        assert_eq!(flight_percent(3976), (100, false));
    }

    #[test]
    fn stinger_uses_the_right_voltage_curve() {
        // FF 12 05, code 03 (discharging), 3920 mV: between 3954 (80%) and 3905 (70%).
        let mut r = [0u8; 20];
        r[..3].copy_from_slice(&[0xFF, 0x12, 0x05]);
        r[9] = 0x03;
        r[11] = 0x50;
        r[12] = 0x0F;
        assert_eq!(stinger_battery(&r), Some(State::Battery { percent: 73, charging: false }));
        r[9] = 0x05; // charging curve: 4000 mV is 70%
        r[11] = 0xA0;
        r[12] = 0x0F;
        assert_eq!(stinger_battery(&r), Some(State::Battery { percent: 70, charging: true }));
        r[9] = 0x06;
        assert_eq!(stinger_battery(&r), Some(State::Battery { percent: 100, charging: false }));
        r[9] = 0x04; // not a charge code
        assert_eq!(stinger_battery(&r), None);
    }

    #[test]
    fn haste_and_dart_levels() {
        assert_eq!(haste_level(&[0x51, 0, 0, 0, 87, 0]), Some((87, false)));
        assert_eq!(haste_level(&[0x51, 0, 0, 0, 87, 2]), Some((87, true)));
        assert_eq!(haste_level(&[0x51, 0, 0, 0, 0, 0]), None); // 0 = unavailable
        assert_eq!(haste_level(&[0x51, 0, 0, 0, 0xFF, 0]), None);

        // The documented Dart reply, at the 69% the real dongle reported.
        assert_eq!(dart_level(&[0x51, 0, 0, 3, 69, 0]), Some((69, false)));
        assert_eq!(dart_level(&[0x51, 0, 0, 3, 69, 1]), Some((69, true)));
        assert_eq!(dart_level(&[0x51, 0, 2, 0xEC, 5, 0]), None); // heartbeat without a level
        assert_eq!(dart_level(&[0x51, 0, 0, 3, 101, 0]), None);
    }

    #[test]
    fn model_table_is_disjoint_and_named() {
        for (i, m) in MODELS.iter().enumerate() {
            for pid in m.pids {
                assert!(
                    MODELS[..i].iter().all(|n| !n.pids.contains(pid)),
                    "pid {pid:04x} twice"
                );
                assert!(!(m.name)(*pid).is_empty());
            }
        }
        assert_eq!(stinger_name(0x173F), "HyperX Cloud Core Wireless");
        assert_eq!(haste2_name(0x0AB5), "HyperX Pulsefire Haste 2 Core Wireless");
        assert_eq!(cloud3_name(0x069F), "HyperX Cloud II Core Wireless");
        assert_eq!(kingston_name(0x16EA), "HyperX Cloud Flight S");
        let haste = MODELS.iter().find(|m| m.pids.contains(&0x0AB5)).expect("Haste 2 entry");
        assert_eq!(haste.kind, Kind::Mouse);
    }
}
