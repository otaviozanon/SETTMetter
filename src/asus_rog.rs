//! ASUS ROG wireless keyboards, mice and headsets (VID 0B05), plus the ROG Omni Receiver
//! (0B05:1ACE).
//!
//! Query: `[report id][12][sub]`, `sub 01` = keyboard battery, `sub 07` = mouse battery.
//! Reply: `[report id][12][sub]...`
//!   keyboard: `[6]` % (Falchion: `[11]`), `[9] == 1` charging
//!   mouse:    `[5]` level (x25 on older 0-4 gauges), `[10]` charging
//! `[1..2] == FF AA` means the device behind the receiver did not answer (asleep / off).
//! Offsets count the report id byte. Only query commands are sent.
//!
//! Headsets: ROG Pelta (1B84) and Delta II (1AFA), vendor collection FF00:0001 on interface 3,
//! report id CC, 64 bytes each way:
//!   `CC 12 07` -> `CC 12 07 00 00 [5] sleep min [6] battery % (FE = unknown) [7] alert %`
//!   `CC 12 08` -> `CC 12 08 00 00 [5] 1 = charging`
//! ROG Cetra Open Wireless (1CAC) on the same collection puts the parts behind `CC 12 12`:
//! `[5]` left %, `[6]` right %, `[7]` case % (the case level is not live). Verified against
//! Armoury Crate: L 99 / R 99 / case 86.
//!
//! Protocol and model table from Wireless-Device-Battery-Monitor (drivers/rog.cpp), which in
//! turn credits SteveWang92/peripheral-battery-tray (MIT, based on G-Helper) and 1gcat/BatteryTool.
//! Not verified in this project.
//!
//! 0B05:1AD0 (Harpe II Ace on its SpeedNova receiver) is handled by `crate::asus`, so that
//! table row is skipped here to keep one observation per device. The Azoth 96 HE keyboard
//! channel of the same receiver is still read below.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const VID: u16 = 0x0B05;
const OMNI_PID: u16 = 0x1ACE;
/// Already claimed by `crate::asus`.
const HANDLED_ELSEWHERE: u16 = 0x1AD0;
const REPLY_TIMEOUT: Duration = Duration::from_millis(500);
const HEADSET_TIMEOUT: Duration = Duration::from_millis(1200);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Type {
    Keyboard,
    Mouse,
}

struct Model {
    pid: u16,
    name: &'static str,
    kind: Type,
    /// 0 = unnumbered.
    report_id: u8,
    /// USB interface (-1 = any vendor-page collection).
    iface: i32,
    /// Required collection tag in the path ("col02"), if any.
    col: Option<&'static str>,
    /// Battery byte, counting the report id.
    offset: usize,
    /// 25 on the older 0-4 gauges, 1 otherwise.
    scale: u8,
}

const MODELS: &[Model] = &[
    // Keyboards (Omni = col02 report 02, receiver/wired = interface 1 unnumbered)
    Model { pid: 0x1A85, name: "ROG Azoth", kind: Type::Keyboard, report_id: 2, iface: 2, col: Some("col02"), offset: 6, scale: 1 },
    Model { pid: 0x1A85, name: "ROG Azoth", kind: Type::Keyboard, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1A83, name: "ROG Azoth", kind: Type::Keyboard, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1C10, name: "ROG Azoth 96 HE", kind: Type::Keyboard, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1B42, name: "ROG Azoth Extreme", kind: Type::Keyboard, report_id: 2, iface: 2, col: Some("col02"), offset: 6, scale: 1 },
    Model { pid: 0x1B3F, name: "ROG Azoth Extreme", kind: Type::Keyboard, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1CF1, name: "ROG Azoth Extreme SE", kind: Type::Keyboard, report_id: 2, iface: 2, col: Some("col02"), offset: 6, scale: 1 },
    Model { pid: 0x1CEF, name: "ROG Azoth Extreme SE", kind: Type::Keyboard, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1C25, name: "ROG Azoth X", kind: Type::Keyboard, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1C24, name: "ROG Azoth X", kind: Type::Keyboard, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x193E, name: "ROG Falchion", kind: Type::Keyboard, report_id: 0, iface: 1, col: None, offset: 11, scale: 1 },
    Model { pid: 0x193C, name: "ROG Falchion", kind: Type::Keyboard, report_id: 0, iface: 1, col: None, offset: 11, scale: 1 },
    Model { pid: 0x1B04, name: "ROG Falchion RX", kind: Type::Keyboard, report_id: 0, iface: -1, col: None, offset: 11, scale: 1 },
    Model { pid: 0x1B06, name: "ROG Falchion RX Low Profile", kind: Type::Keyboard, report_id: 2, iface: 2, col: Some("col02"), offset: 11, scale: 1 },
    Model { pid: 0x1AB0, name: "ROG Strix Scope II 96", kind: Type::Keyboard, report_id: 2, iface: 2, col: Some("col02"), offset: 6, scale: 1 },
    Model { pid: 0x1AAE, name: "ROG Strix Scope II 96", kind: Type::Keyboard, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1B7A, name: "ROG Strix Scope II 96 RX", kind: Type::Keyboard, report_id: 2, iface: 2, col: Some("col02"), offset: 6, scale: 1 },
    Model { pid: 0x1B78, name: "ROG Strix Scope II 96 RX", kind: Type::Keyboard, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1A07, name: "ROG Strix Scope RX TKL", kind: Type::Keyboard, report_id: 0, iface: -1, col: None, offset: 6, scale: 1 },
    // Mice (Omni = col03 report 03, receiver/wired = interface 0 unnumbered)
    Model { pid: 0x18E5, name: "ROG Chakram", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x18E3, name: "ROG Chakram", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1A1A, name: "ROG Chakram X", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1A18, name: "ROG Chakram X", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x18A0, name: "ROG Gladius II Wireless", kind: Type::Mouse, report_id: 0, iface: 2, col: None, offset: 5, scale: 25 },
    Model { pid: 0x197F, name: "ROG Gladius III Wireless", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x197D, name: "ROG Gladius III Wireless", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1A72, name: "ROG Gladius III Aimpoint", kind: Type::Mouse, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1A72, name: "ROG Gladius III Aimpoint", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1A70, name: "ROG Gladius III Aimpoint", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1B0C, name: "ROG Gladius III Aimpoint EVA-02", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1B0A, name: "ROG Gladius III Aimpoint EVA-02", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1A94, name: "ROG Harpe Ace Aim Lab Edition", kind: Type::Mouse, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1A94, name: "ROG Harpe Ace Aim Lab Edition", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1A92, name: "ROG Harpe Ace Aim Lab Edition", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1B68, name: "ROG Harpe Ace Extreme", kind: Type::Mouse, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1B69, name: "ROG Harpe Ace Extreme", kind: Type::Mouse, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1B67, name: "ROG Harpe Ace Extreme", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1B65, name: "ROG Harpe Ace Mini", kind: Type::Mouse, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1B63, name: "ROG Harpe Ace Mini", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1AD0, name: "ROG Harpe II Ace", kind: Type::Mouse, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1C69, name: "ROG Harpe II Ace", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1960, name: "ROG Keris Wireless", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x195E, name: "ROG Keris Wireless", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1A59, name: "ROG Keris Wireless EVA Edition", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1A57, name: "ROG Keris Wireless EVA Edition", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1A68, name: "ROG Keris Wireless Aimpoint", kind: Type::Mouse, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1A6A, name: "ROG Keris Wireless Aimpoint", kind: Type::Mouse, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1A68, name: "ROG Keris Wireless Aimpoint", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1A66, name: "ROG Keris Wireless Aimpoint", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1B1A, name: "ROG Keris II Ace", kind: Type::Mouse, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1B18, name: "ROG Keris II Ace", kind: Type::Mouse, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1B16, name: "ROG Keris II Ace", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1C0E, name: "ROG Keris II Origin", kind: Type::Mouse, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1C0C, name: "ROG Keris II Origin", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1D4E, name: "ROG Keris II Origin KJP", kind: Type::Mouse, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1D4C, name: "ROG Keris II Origin KJP", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1908, name: "ROG Pugio II", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1906, name: "ROG Pugio II", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1979, name: "ROG Spatha X", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1977, name: "ROG Spatha X", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x18B4, name: "ROG Strix Carry", kind: Type::Mouse, report_id: 0, iface: 1, col: None, offset: 7, scale: 25 },
    Model { pid: 0x1949, name: "ROG Strix Impact II Wireless", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1947, name: "ROG Strix Impact II Wireless", kind: Type::Mouse, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1AD7, name: "ROG Strix Impact III Wireless", kind: Type::Mouse, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
];

struct HeadsetModel {
    pid: u16,
    name: &'static str,
}

const HEADSETS: &[HeadsetModel] =
    &[HeadsetModel { pid: 0x1B84, name: "ROG Pelta" }, HeadsetModel { pid: 0x1AFA, name: "ROG Delta II" }];

#[derive(Default)]
pub struct AsusRog {
    /// Receiver channels that have answered awake at least once this session.
    awake: HashSet<u16>,
}

impl AsusRog {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let mut out = Vec::new();
        let mut seen: HashSet<&'static str> = HashSet::new();
        let mut claimed: HashSet<u16> = HashSet::new();
        self.pelta(api, &mut out, &mut seen, &mut claimed);
        self.cetra(api, &mut out, &mut seen, &mut claimed);
        self.omni(api, &mut out, &mut seen, &mut claimed);
        self.azoth_dongle(api, &mut out, &mut seen);
        self.models(api, &mut out, &mut seen, &mut claimed);
        out
    }

    fn pelta(
        &mut self,
        api: &HidApi,
        out: &mut Vec<Observation>,
        seen: &mut HashSet<&'static str>,
        claimed: &mut HashSet<u16>,
    ) {
        for hs in HEADSETS {
            if claimed.contains(&hs.pid) {
                continue;
            }
            let key = format!("rog:{:04x}:hs", hs.pid);
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == hs.pid
                    && d.interface_number() == 3
                    && d.usage_page() == 0xFF00
                    && d.usage() == 0x0001
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::input_len(&desc, 0xCC).unwrap_or(0) < 8 || probe::output_len(&desc, 0xCC).unwrap_or(0) < 8 {
                    continue;
                }
                let state = match headset_query(&dev, 0x07) {
                    HeadsetReply::Asleep => State::Unreachable,
                    HeadsetReply::Ok(r) => {
                        let charging = matches!(headset_query(&dev, 0x08), HeadsetReply::Ok(c) if c[5] == 1);
                        pelta_state(&r, charging)
                    }
                    HeadsetReply::None => continue,
                };
                diag::event(&key, format!("battery via report 0xCC: {state:?}"));
                add_once(out, seen, &format!("rog:{:04x}", hs.pid), hs.name, Kind::Headset, state);
                claimed.insert(hs.pid);
                break;
            }
        }
    }

    fn cetra(
        &mut self,
        api: &HidApi,
        out: &mut Vec<Observation>,
        seen: &mut HashSet<&'static str>,
        claimed: &mut HashSet<u16>,
    ) {
        const PID: u16 = 0x1CAC;
        const NAME: &str = "ROG Cetra Open Wireless";
        if claimed.contains(&PID) {
            return;
        }
        let key = format!("rog:{PID:04x}:hs");
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID
                && d.product_id() == PID
                && d.interface_number() == 3
                && d.usage_page() == 0xFF00
                && d.usage() == 0x0001
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::input_len(&desc, 0xCC).unwrap_or(0) < 8 || probe::output_len(&desc, 0xCC).unwrap_or(0) < 8 {
                continue;
            }
            let HeadsetReply::Ok(r) = headset_query(&dev, 0x12) else { continue };
            let state = cetra_state(&r);
            let level = |at: usize| r.get(at).copied().filter(|v| *v > 0 && *v <= 100);
            diag::event(
                &key,
                format!(
                    "battery via report 0xCC sub 0x12 (L {:?}, R {:?}, case {:?})",
                    level(5),
                    level(6),
                    level(7)
                ),
            );
            add_once(out, seen, &format!("rog:{PID:04x}"), NAME, Kind::Headset, state);
            claimed.insert(PID);
            break;
        }
    }

    fn omni(
        &mut self,
        api: &HidApi,
        out: &mut Vec<Observation>,
        seen: &mut HashSet<&'static str>,
        claimed: &mut HashSet<u16>,
    ) {
        let key = format!("rog:{OMNI_PID:04x}");
        let mut list = None;
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID
                && d.product_id() == OMNI_PID
                && d.interface_number() == 2
                && has_col(d.path().to_string_lossy().as_ref(), "col01")
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, 0x01).unwrap_or(0) < 12 {
                continue;
            }
            if let QueryReply::Ok(r) = query(&dev, 0x01, 0xA0, 0x00) {
                list = Some(r);
                break;
            }
        }
        let Some(r) = list else { return };
        // Paired product ids in 4-byte slots from byte 5; 0 ends the list.
        let pids: Vec<u16> = (5..r.len().saturating_sub(1))
            .step_by(4)
            .map(|at| u16::from(r[at]) | u16::from(r[at + 1]) << 8)
            .take_while(|pid| *pid != 0)
            .collect();
        for pid in pids {
            let Some(m) = MODELS.iter().find(|m| m.pid == pid && m.report_id != 0) else { continue };
            let Some(col) = m.col else { continue };
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == OMNI_PID
                    && d.interface_number() == 2
                    && has_col(d.path().to_string_lossy().as_ref(), col)
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, m.report_id).unwrap_or(0) < 12 {
                    continue;
                }
                let Some(state) = read_model(&dev, m) else { continue };
                diag::event(&key, format!("Omni channel {:04x}: {state:?}", m.pid));
                add_once(out, seen, &format!("rog:{:04x}", m.pid), m.name, kind_of(m), state);
                claimed.insert(m.pid);
                break;
            }
        }
    }

    /// ROG Azoth 96 HE (1C10) on the SpeedNova 8K receiver (1AD0), keyboard channel col02.
    /// An FF AA answer is shown only after the keyboard has answered awake once, because the
    /// receiver gives the same answer when nothing is paired.
    fn azoth_dongle(&mut self, api: &HidApi, out: &mut Vec<Observation>, seen: &mut HashSet<&'static str>) {
        const PID: u16 = 0x1C10;
        const NAME: &str = "ROG Azoth 96 HE";
        let key = format!("rog:{PID:04x}:kb");
        let mut saw_asleep = false;
        for info in api.device_list().filter(|d| {
            d.vendor_id() == VID
                && d.product_id() == HANDLED_ELSEWHERE
                && d.interface_number() == 2
                && has_col(d.path().to_string_lossy().as_ref(), "col02")
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            if probe::output_len(&desc, 0x02).unwrap_or(0) < 12 {
                continue;
            }
            match query(&dev, 0x02, 0x12, 0x01) {
                QueryReply::Asleep => saw_asleep = true,
                QueryReply::Ok(r) => {
                    let state = keyboard_state(&r, 6);
                    self.awake.insert(PID);
                    diag::event(&key, format!("battery on the receiver's keyboard channel: {state:?}"));
                    add_once(out, seen, &format!("rog:{PID:04x}"), NAME, Kind::Keyboard, state);
                    return;
                }
                QueryReply::None => {}
            }
        }
        if saw_asleep && self.awake.contains(&PID) {
            diag::event(&key, "asleep on the receiver's keyboard channel");
            add_once(out, seen, &format!("rog:{PID:04x}"), NAME, Kind::Keyboard, State::Unreachable);
        }
    }

    fn models(
        &mut self,
        api: &HidApi,
        out: &mut Vec<Observation>,
        seen: &mut HashSet<&'static str>,
        claimed: &mut HashSet<u16>,
    ) {
        for m in MODELS {
            if m.pid == HANDLED_ELSEWHERE || claimed.contains(&m.pid) {
                continue;
            }
            let key = format!("rog:{:04x}:{}", m.pid, if m.kind == Type::Keyboard { "kb" } else { "ms" });
            for info in api.device_list().filter(|d| {
                d.vendor_id() == VID
                    && d.product_id() == m.pid
                    && d.usage_page() >= 0xFF00
                    && (m.iface < 0 || d.interface_number() == m.iface)
                    && m.col.is_none_or(|c| has_col(d.path().to_string_lossy().as_ref(), c))
            }) {
                let Ok(dev) = api.open_path(info.path()) else { continue };
                let Some(desc) = probe::descriptor(&dev) else { continue };
                if probe::output_len(&desc, m.report_id).unwrap_or(0) < 12 {
                    continue;
                }
                let Some(state) = read_model(&dev, m) else { continue };
                // A receiver channel answers FF AA when nothing is paired to it too.
                let asleep = matches!(state, State::Unreachable);
                if m.report_id != 0 && asleep && !self.awake.contains(&m.pid) {
                    continue;
                }
                if m.report_id != 0 && !asleep {
                    self.awake.insert(m.pid);
                }
                diag::event(&key, format!("battery via report 0x{:02x}: {state:?}", m.report_id));
                add_once(out, seen, &format!("rog:{:04x}", m.pid), m.name, kind_of(m), state);
                claimed.insert(m.pid);
                break;
            }
        }
    }
}

fn kind_of(m: &Model) -> Kind {
    if m.kind == Type::Keyboard { Kind::Keyboard } else { Kind::Mouse }
}

fn add_once(
    out: &mut Vec<Observation>,
    seen: &mut HashSet<&'static str>,
    id: &str,
    name: &'static str,
    kind: Kind,
    state: State,
) {
    if seen.insert(name) {
        out.push(Observation { id: id.into(), name: Some(name.into()), kind: Some(kind), state });
    }
}

fn read_model(dev: &HidDevice, m: &Model) -> Option<State> {
    let sub = if m.kind == Type::Keyboard { 0x01 } else { 0x07 };
    match query(dev, m.report_id, 0x12, sub) {
        QueryReply::Asleep => Some(State::Unreachable),
        QueryReply::None => None,
        QueryReply::Ok(r) => Some(if m.kind == Type::Keyboard {
            keyboard_state(&r, m.offset)
        } else {
            mouse_state(&r, m.offset, m.scale)
        }),
    }
}

fn keyboard_state(r: &[u8], offset: usize) -> State {
    let Some(&level) = r.get(offset) else { return State::Unreachable };
    if level > 100 || r.len() <= 9 {
        return State::Unreachable;
    }
    State::Battery { percent: level, charging: r[9] == 1 }
}

/// `[6]` is the level, `FE` means not known yet.
fn pelta_state(r: &[u8], charging: bool) -> State {
    match r.get(6) {
        Some(&level) if level <= 100 => State::Battery { percent: level, charging },
        _ => State::Unreachable,
    }
}

/// `[5]` left %, `[6]` right %, `[7]` case %; the lower earbud wins, 0 = not reported.
fn cetra_state(r: &[u8]) -> State {
    let level = |at: usize| r.get(at).copied().filter(|v| *v > 0 && *v <= 100);
    let (left, right) = (level(5), level(6));
    match (left, right) {
        (None, None) => State::Unreachable,
        _ => State::Battery { percent: left.unwrap_or(100).min(right.unwrap_or(100)), charging: false },
    }
}

fn mouse_state(r: &[u8], offset: usize, scale: u8) -> State {
    let Some(&raw) = r.get(offset) else { return State::Unreachable };
    if r.len() <= 10 {
        return State::Unreachable;
    }
    let level = u16::from(raw) * u16::from(scale);
    // 0 means unavailable, not empty.
    if level == 0 || level > 100 {
        return State::Unreachable;
    }
    State::Battery { percent: level as u8, charging: r[10] != 0 }
}

enum QueryReply {
    Ok(Vec<u8>),
    Asleep,
    None,
}

fn query(dev: &HidDevice, rid: u8, cmd: u8, sub: u8) -> QueryReply {
    let desc = probe::descriptor(dev).unwrap_or_default();
    // For an unnumbered report the descriptor length excludes the id slot hidapi wants.
    let min = if rid == 0 { 2 } else { 3 };
    let data = probe::output_len(&desc, rid).unwrap_or(min).max(min);
    let mut req = vec![0u8; if rid == 0 { data + 1 } else { data }];
    req[0] = rid;
    req[1] = cmd;
    req[2] = sub;

    drain(dev);
    if dev.write(&req).is_err() {
        return QueryReply::None;
    }
    let deadline = Instant::now() + REPLY_TIMEOUT;
    let mut buf = [0u8; 1024];
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now()).as_millis().max(1) as i32;
        let n = match dev.read_timeout(&mut buf, left.min(250)) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(_) => return QueryReply::None,
        };
        let mut r = Vec::with_capacity(n + 1);
        if rid == 0 {
            r.push(0); // restore the report id slot so offsets match
        }
        r.extend_from_slice(&buf[..n]);
        if r.len() < 3 {
            continue;
        }
        if r[1] == 0xFF && r[2] == 0xAA {
            return QueryReply::Asleep;
        }
        if r[1] == cmd && r[2] == sub {
            return QueryReply::Ok(r);
        }
    }
    QueryReply::None
}

enum HeadsetReply {
    Ok(Vec<u8>),
    Asleep,
    None,
}

fn headset_query(dev: &HidDevice, cmd: u8) -> HeadsetReply {
    let mut req = [0u8; 64];
    req[0] = 0xCC;
    req[1] = 0x12;
    req[2] = cmd;
    drain(dev);
    if dev.write(&req).is_err() {
        return HeadsetReply::None;
    }
    let deadline = Instant::now() + HEADSET_TIMEOUT;
    let mut buf = [0u8; 1024];
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now()).as_millis().max(1) as i32;
        let n = match dev.read_timeout(&mut buf, left.min(300)) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(_) => return HeadsetReply::None,
        };
        let r = &buf[..n];
        if r.len() < 3 || r[0] != 0xCC {
            continue;
        }
        if r[1] == 0xFF && r[2] == 0xAA {
            return HeadsetReply::Asleep;
        }
        if r[1] == 0x12 {
            if r.len() >= 7 && r[5] == 0xFF && r[6] == 0xAA {
                return HeadsetReply::Asleep;
            }
            if r[2] == cmd {
                return HeadsetReply::Ok(r.to_vec());
            }
        }
    }
    HeadsetReply::None
}

fn drain(dev: &HidDevice) {
    let mut stale = [0u8; 1024];
    while let Ok(n) = dev.read_timeout(&mut stale, 0) {
        if n == 0 {
            break;
        }
    }
}

fn has_col(path: &str, tag: &str) -> bool {
    path.to_ascii_lowercase().contains(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keyboard_reply() {
        // Documented layout: [6] level, [9] == 1 charging.
        let mut r = [0u8; 64];
        r[1] = 0x12;
        r[2] = 0x01;
        r[6] = 81;
        r[9] = 1;
        assert_eq!(keyboard_state(&r, 6), State::Battery { percent: 81, charging: true });
        r[6] = 101; // not a level
        assert_eq!(keyboard_state(&r, 6), State::Unreachable);
    }

    #[test]
    fn falchion_uses_byte_11() {
        let mut r = [0u8; 64];
        r[11] = 64;
        assert_eq!(keyboard_state(&r, 11), State::Battery { percent: 64, charging: false });
    }

    #[test]
    fn parses_mouse_reply_and_legacy_gauge() {
        let mut r = [0u8; 64];
        r[5] = 53;
        r[10] = 1;
        assert_eq!(mouse_state(&r, 5, 1), State::Battery { percent: 53, charging: true });
        // Old 0-4 gauge is stored in 25% steps.
        r[5] = 3;
        r[10] = 0;
        assert_eq!(mouse_state(&r, 5, 25), State::Battery { percent: 75, charging: false });
        r[5] = 0; // unavailable, not empty
        assert_eq!(mouse_state(&r, 5, 25), State::Unreachable);
        r[5] = 5; // 125%
        assert_eq!(mouse_state(&r, 5, 25), State::Unreachable);
    }

    #[test]
    fn pelta_reply_and_charging() {
        // CC 12 07 00 00 00 2A ... = 42%, and CC 12 08 00 00 01 ... = charging.
        let mut r = [0u8; 64];
        r[..3].copy_from_slice(&[0xCC, 0x12, 0x07]);
        r[6] = 0x2A;
        assert_eq!(pelta_state(&r, true), State::Battery { percent: 42, charging: true });
        r[6] = 0xFE; // level not known yet
        assert_eq!(pelta_state(&r, false), State::Unreachable);
    }

    #[test]
    fn cetra_levels_from_the_verified_capture() {
        // Armoury Crate showed L 99 / R 99 / case 86.
        let mut r = [0u8; 64];
        r[..3].copy_from_slice(&[0xCC, 0x12, 0x12]);
        r[5] = 99;
        r[6] = 99;
        r[7] = 86;
        assert_eq!(cetra_state(&r), State::Battery { percent: 99, charging: false });
        // [5] rose to 100 with only the left earbud in the case; the right one is unknown.
        r[5] = 100;
        r[6] = 0;
        assert_eq!(cetra_state(&r), State::Battery { percent: 100, charging: false });
        // Both earbuds out / in the closed case: no level reported.
        r[5] = 0;
        assert_eq!(cetra_state(&r), State::Unreachable);
    }

    #[test]
    fn unnumbered_reply_restores_the_report_id_slot() {
        // Raw read for a model on interface 0 starts with 12; offsets then match
        // the numbered frames by prepending the missing report id byte.
        let raw = [0x12, 0x07, 0x00, 0x00, 0x35, 0x02, 0x14, 0x00, 0x00, 0x00];
        let mut r = vec![0];
        r.extend_from_slice(&raw);
        assert_eq!(mouse_state(&r, 5, 1), State::Battery { percent: 53, charging: false });
    }

    #[test]
    fn omni_lists_paired_product_ids_in_four_byte_slots() {
        // [01 A0 00] lists them from byte 5, ending at the first zero slot.
        let r: [u8; 15] = [0x01, 0xA0, 0x00, 0x00, 0x00, 0x85, 0x1A, 0x00, 0x00, 0x42, 0x1B, 0x00, 0x00, 0x00, 0x00];
        let pids: Vec<u16> = (5..r.len().saturating_sub(1))
            .step_by(4)
            .map(|at| u16::from(r[at]) | u16::from(r[at + 1]) << 8)
            .take_while(|pid| *pid != 0)
            .collect();
        assert_eq!(pids, [0x1A85, 0x1B42]);
    }

    #[test]
    fn table_has_no_duplicate_receiver_entries() {
        for (i, m) in MODELS.iter().enumerate() {
            assert!(
                MODELS[i + 1..].iter().all(|n| !(n.pid == m.pid && n.report_id == m.report_id)),
                "pid {:04x} report {} twice",
                m.pid,
                m.report_id
            );
        }
    }
}
