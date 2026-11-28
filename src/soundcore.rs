//! Anker Soundcore Bluetooth headphones and earbuds (Life Q20/Q30/Q35, Space Q45/One/One Pro/A40,
//! Liberty 2/3/4/4 NC/4 Pro/5, Life P3/A3i/Dot/Note, P20i/P30i/P40i/R50i, Sport X10/X20, AeroFit...).
//!
//! Reads battery level and charging status over the Soundcore RFCOMM control channel (vendor
//! service `0cf12d31-fac3-4553-bd80-d6832e7XXXXX`, falling back to the Fast Pair message stream and
//! standard SPP `00001101`). Works even when Windows' Hands-Free AG (HFP) service is disabled for
//! audio quality, and gives charging status plus left/right levels on TWS earbuds.
//!
//! Only paired devices Windows reports as connected that either expose a Soundcore vendor RFCOMM
//! service or carry a Soundcore/Anker name are queried. If another app (e.g. OpenSCQ30) holds the
//! channel, the last reading is kept.
//!
//! Packet layout (little-endian length, 1-byte additive checksum over all preceding bytes):
//!
//! ```text
//! outbound: 08 EE 00 00 00 [cmd0] [cmd1] [lenLo] [lenHi] [body...] [checksum]
//! inbound:  09 FF 00 00 01 [cmd0] [cmd1] [lenLo] [lenHi] [body...] [checksum]
//! ```
//!
//! Commands: `01 03` battery level, `01 04` charging, `01 05` serial/firmware (model code),
//! `01 01` state (fallback). Protocol and per-model scales verified against OpenSCQ30 and a Life
//! Q30 (A3028). Ported from soundcore.cpp / soundcore_protocol.h.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::hidutil::sum as checksum;
use crate::wide;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::Properties::{
    DEVPROP_TYPE_BOOLEAN, DEVPROP_TYPE_GUID, DEVPROP_TYPE_STRING, DEVPROPKEY,
};
use windows_sys::core::GUID;

/// Standard Serial Port Profile: `{00001101-0000-1000-8000-00805f9b34fb}`
const SPP: GUID = GUID::from_u128(0x00001101_0000_1000_8000_00805F9B34FB);
/// Google Fast Pair Message Stream RFCOMM: `{df21fe2c-2515-4fdb-8886-f12c4d67927c}`
const FAST_PAIR: GUID = GUID::from_u128(0xDF21FE2C_2515_4FDB_8886_F12C4D67927C);

const BATTERY_LEVEL: Command = Command { group: 0x01, id: 0x03 };
const BATTERY_CHARGING: Command = Command { group: 0x01, id: 0x04 };
const SERIAL_AND_FW: Command = Command { group: 0x01, id: 0x05 };
const STATE: Command = Command { group: 0x01, id: 0x01 };

/// DEVPKEY_Device_IsConnected.
const CONNECTED_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x83DA6326_97A6_4088_9453_A1923F573B29),
    pid: 15,
};
/// DEVPKEY_Device_FriendlyName.
const NAME_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0xA45C254E_DF1C_4EFD_8020_67D146A850E0),
    pid: 14,
};
/// DEVPKEY_Bluetooth_DeviceAddress.
const ADDRESS_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x2BD67D8B_8BEB_48D5_87E0_6CDA3428040A),
    pid: 1,
};
/// DEVPKEY_Bluetooth_ServiceGuid.
const SERVICE_KEY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x2BD67D8B_8BEB_48D5_87E0_6CDA3428040A),
    pid: 2,
};

/// Fresh instances are made on every poll, so the per-device model scale and last level live in a
/// module static.
#[derive(Default)]
pub struct Soundcore;

#[derive(Default)]
struct Cached {
    scale: Scale,
    dual_hint: bool,
    probed: bool,
    last: Option<BatteryStatus>,
}

fn cache() -> &'static Mutex<HashMap<u64, Cached>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

impl Soundcore {
    pub fn scan(&mut self) -> Vec<Observation> {
        let mut cache = cache().lock().unwrap();
        let mut out = Vec::new();
        for d in find_connected() {
            let key = format!("soundcore:{}", diag::bt_tag(d.addr));
            let mut status = None;
            for service in &d.services {
                if let Some(s) = query(&mut cache, &d, service) {
                    status = Some(s);
                    break;
                }
            }
            let status = match status.filter(|s| s.percent >= 0) {
                Some(s) => {
                    if let Some(c) = cache.get_mut(&d.addr) {
                        c.last = Some(s);
                    }
                    s
                }
                // Retain the last level if the query timed out while the device is still connected.
                None => match cache.get(&d.addr).and_then(|c| c.last) {
                    Some(s) => {
                        diag::event(&key, "no answer, last level kept");
                        s
                    }
                    None => {
                        diag::event(&key, "no answer on the Soundcore/Spp/Fast Pair channels");
                        continue;
                    }
                },
            };
            diag::event(
                &key,
                format!("level {}%{}{}", status.percent, if status.charging { ", charging" } else { "" },
                    if status.dual { format!(" (L {}, R {})", opt(status.left), opt(status.right)) } else { String::new() }),
            );
            out.push(Observation {
                id: format!("soundcore:{:012x}", d.addr),
                name: Some(d.name.clone()),
                kind: Some(Kind::Headset),
                state: State::Battery { percent: status.percent.clamp(0, 100) as u8, charging: status.charging },
            });
        }
        out
    }
}

fn query(cache: &mut HashMap<u64, Cached>, dev: &Device, service: &GUID) -> Option<BatteryStatus> {
    let conn = rfcomm::Connection::open(dev.addr, *service)?;
    if same_guid(service, &FAST_PAIR) {
        return fast_pair_battery(&conn);
    }
    let cache = cache.entry(dev.addr).or_default();
    if !cache.probed {
        cache.probed = true;
        cache.scale = scale_from_name(&dev.name.to_ascii_lowercase());
        if let Some(body) = exchange(&conn, SERIAL_AND_FW, Duration::from_millis(600)) {
            let code = extract_model_code(&body);
            if !code.is_empty() {
                cache.scale = scale_for_model(&code);
                cache.dual_hint = is_dual_model(&code);
            }
        }
    }
    if let Some(body) = exchange(&conn, BATTERY_LEVEL, Duration::from_millis(1000)) {
        if let Some(mut status) = parse_battery_level(&body, cache.scale) {
            if let Some(charging) = exchange(&conn, BATTERY_CHARGING, Duration::from_millis(800)) {
                apply_charging(&charging, &mut status);
            }
            return Some(status);
        }
    }
    // Fallback for models that only report battery inside the 01 01 state update.
    let body = exchange(&conn, STATE, Duration::from_millis(1200))?;
    let code = extract_model_code(&body);
    if !code.is_empty() {
        cache.scale = scale_for_model(&code);
        cache.dual_hint = is_dual_model(&code);
    }
    parse_state_update(&body, cache.scale, cache.dual_hint)
}

fn opt(v: i32) -> String {
    if v >= 0 { format!("{v}%") } else { "-".into() }
}

struct Device {
    name: String,
    addr: u64,
    services: Vec<GUID>,
}

/// Anker's 4-digit model code, e.g. "3028" for A3028 (Life Q30).
fn find_connected() -> Vec<Device> {
    let nodes = bt_nodes();
    let mut vendor: HashMap<u64, Vec<GUID>> = HashMap::new();
    for node in &nodes {
        let Some(guid) = guid_prop(node) else { continue };
        if !is_vendor_uuid(&guid) {
            continue;
        }
        let Some(addr) = node.address.as_deref().and_then(parse_hex_address) else { continue };
        vendor.entry(addr).or_default().push(guid);
    }
    let mut out = Vec::new();
    for node in &nodes {
        let Some(addr) = node.id_address else { continue };
        if !node.connected || node.name.is_empty() {
            continue;
        }
        let mut services = vendor.remove(&addr).unwrap_or_default();
        let has_vendor = !services.is_empty();
        if !has_vendor && !looks_like_soundcore(&node.name) {
            continue;
        }
        services.push(FAST_PAIR);
        services.push(SPP);
        out.push(Device { name: node.name.clone(), addr, services });
    }
    out
}

pub(crate) struct Node {
    pub(crate) id_address: Option<u64>,
    pub(crate) connected: bool,
    pub(crate) name: String,
    address: Option<String>,
    service: Option<GUID>,
}

pub(crate) fn bt_nodes() -> Vec<Node> {
    let mut out = Vec::new();
    unsafe {
        let enumerator = wide("BTHENUM");
        let set = SetupDiGetClassDevsW(null(), enumerator.as_ptr(), null_mut(), DIGCF_ALLCLASSES | DIGCF_PRESENT);
        if set as isize == -1 {
            return out;
        }
        let mut i = 0;
        loop {
            let mut data: SP_DEVINFO_DATA = zeroed();
            data.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiEnumDeviceInfo(set, i, &mut data) == 0 {
                break;
            }
            i += 1;
            let mut id = [0u16; 512];
            if SetupDiGetDeviceInstanceIdW(set, &data, id.as_mut_ptr(), id.len() as u32, null_mut()) == 0 {
                continue;
            }
            let len = id.iter().position(|c| *c == 0).unwrap_or(id.len());
            let id = String::from_utf16_lossy(&id[..len]);
            let upper = id.to_ascii_uppercase();
            let id_address = upper
                .strip_prefix("BTHENUM\\DEV_")
                .and_then(|rest| rest.get(..12))
                .and_then(parse_hex_address);
            out.push(Node {
                id_address,
                connected: bool_prop(set, &data, &CONNECTED_KEY).unwrap_or(false),
                name: string_prop(set, &data, &NAME_KEY).unwrap_or_default(),
                address: string_prop(set, &data, &ADDRESS_KEY),
                service: guid_prop_bytes(set, &data),
            });
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

fn guid_prop(node: &Node) -> Option<GUID> {
    node.service
}

fn parse_hex_address(text: &str) -> Option<u64> {
    (text.len() == 12).then(|| u64::from_str_radix(text, 16).ok()).flatten().filter(|a| *a != 0)
}

/// `0cf12d31-fac3-4553-bd80-d6832e7XXXXX`.
fn is_vendor_uuid(g: &GUID) -> bool {
    g.data1 == 0x0CF12D31
        && g.data2 == 0xFAC3
        && g.data3 == 0x4553
        && g.data4[..5] == [0xBD, 0x80, 0xD6, 0x83, 0x2E]
        && (g.data4[5] & 0xF0) == 0x70
}

fn looks_like_soundcore(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "soundcore", "anker", "life q20", "life q30", "life q35", "life tune", "life p3", "life a3", "life dot",
        "life note", "space q45", "space one", "space a40", "liberty ", "sport x10", "sport x20", "aerofit",
        "aeroclip", "vr p10", "sleep a10", "sleep a20",
    ]
    .iter()
    .any(|hint| n.contains(hint))
}

fn bool_prop(set: HDEVINFO, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<bool> {
    let mut buf = [0u8; 8];
    let (mut ty, mut size) = (0u32, 0u32);
    let ok = unsafe {
        SetupDiGetDevicePropertyW(set, data, key, &mut ty, buf.as_mut_ptr(), buf.len() as u32, &mut size, 0)
    };
    (ok != 0 && ty == DEVPROP_TYPE_BOOLEAN).then(|| buf[0] != 0)
}

fn string_prop(set: HDEVINFO, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<String> {
    let mut buf = [0u8; 512];
    let (mut ty, mut size) = (0u32, 0u32);
    let ok = unsafe {
        SetupDiGetDevicePropertyW(set, data, key, &mut ty, buf.as_mut_ptr(), buf.len() as u32, &mut size, 0)
    };
    if ok == 0 || ty != DEVPROP_TYPE_STRING {
        return None;
    }
    let bytes = &buf[..(size as usize).min(buf.len())];
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|u| *u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

fn guid_prop_bytes(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> Option<GUID> {
    let mut buf = [0u8; 16];
    let (mut ty, mut size) = (0u32, 0u32);
    let ok = unsafe {
        SetupDiGetDevicePropertyW(set, data, &SERVICE_KEY, &mut ty, buf.as_mut_ptr(), buf.len() as u32, &mut size, 0)
    };
    if ok == 0 || ty != DEVPROP_TYPE_GUID || size < 16 {
        return None;
    }
    Some(GUID {
        data1: u32::from_le_bytes(buf[0..4].try_into().ok()?),
        data2: u16::from_le_bytes(buf[4..6].try_into().ok()?),
        data3: u16::from_le_bytes(buf[6..8].try_into().ok()?),
        data4: buf[8..16].try_into().ok()?,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Command {
    group: u8,
    id: u8,
}

#[derive(Clone, Copy)]
struct Scale {
    max_level: i32,
    level_offset: i32,
    known: bool,
}

impl Default for Scale {
    fn default() -> Scale {
        Scale { max_level: 5, level_offset: 0, known: false }
    }
}

#[derive(Clone, Copy)]
struct BatteryStatus {
    dual: bool,
    percent: i32,
    left: i32,
    right: i32,
    charging: bool,
    left_charging: bool,
    right_charging: bool,
}

impl BatteryStatus {
    fn new() -> BatteryStatus {
        BatteryStatus { dual: false, percent: -1, left: -1, right: -1, charging: false, left_charging: false, right_charging: false }
    }
}



fn build_empty_request(cmd: Command) -> [u8; 10] {
    let mut pkt = [0x08, 0xEE, 0x00, 0x00, 0x00, cmd.group, cmd.id, 0x0A, 0x00, 0x00];
    pkt[9] = checksum(&pkt[..9]);
    pkt
}

/// The last valid inbound packet matching `wanted` in `data`.
fn find_inbound(data: &[u8], wanted: Command) -> Option<Vec<u8>> {
    if data.len() < 10 {
        return None;
    }
    let mut found = None;
    let mut i = 0;
    while i + 10 <= data.len() {
        if data[i..i + 5] != [0x09, 0xFF, 0x00, 0x00, 0x01] {
            i += 1;
            continue;
        }
        let total = usize::from(data[i + 7]) | usize::from(data[i + 8]) << 8;
        if !(10..=1024).contains(&total) || i + total > data.len() {
            i += 1;
            continue;
        }
        if checksum(&data[i..i + total - 1]) != data[i + total - 1] {
            i += 1;
            continue;
        }
        let cmd = Command { group: data[i + 5], id: data[i + 6] };
        if cmd == wanted {
            found = Some(data[i + 9..i + total - 1].to_vec());
        }
        i += total;
    }
    found
}

fn exchange(conn: &rfcomm::Connection, cmd: Command, timeout: Duration) -> Option<Vec<u8>> {
    conn.send_all(&build_empty_request(cmd)).then_some(())?;
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    while buf.len() < 4096 {
        let left = deadline.checked_duration_since(Instant::now())?;
        if !conn.wait_readable(left) {
            break;
        }
        let mut chunk = [0u8; 512];
        let n = conn.recv(&mut chunk)?;
        buf.extend_from_slice(&chunk[..n]);
        if let Some(body) = find_inbound(&buf, cmd) {
            return Some(body);
        }
    }
    None
}

/// Fast Pair Message Stream packets: Group `0x03`, Code `0x03` (battery updated). Bit 7 of each
/// level byte is charging, bits 0-6 the percentage (0x7F = disconnected).
fn same_guid(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

fn parse_fast_pair(data: &[u8]) -> Option<BatteryStatus> {
    let mut i = 0;
    let mut out = BatteryStatus::new();
    while i + 4 <= data.len() {
        let len = usize::from(data[i + 2]) << 8 | usize::from(data[i + 3]);
        if i + 4 + len > data.len() {
            break;
        }
        let p = &data[i + 4..i + 4 + len];
        if data[i] == 0x03 && data[i + 1] == 0x03 {
            if len == 1 {
                out.charging = p[0] & 0x80 != 0;
                let v = i32::from(p[0] & 0x7F);
                if v <= 100 {
                    out.percent = v;
                    out.dual = false;
                }
            } else if len >= 2 {
                out.dual = true;
                let left = i32::from(p[0] & 0x7F);
                let right = i32::from(p[1] & 0x7F);
                out.left = if left <= 100 { left } else { -1 };
                out.right = if right <= 100 { right } else { -1 };
                out.left_charging = p[0] & 0x80 != 0;
                out.right_charging = p[1] & 0x80 != 0;
                out.percent = match (out.left, out.right) {
                    (l, r) if l >= 0 && r >= 0 => l.min(r),
                    (l, r) => l.max(r),
                };
                out.charging = (out.left >= 0 && out.left_charging) || (out.right >= 0 && out.right_charging);
            }
        }
        i += 4 + len;
    }
    (out.percent >= 0).then_some(out)
}

fn fast_pair_battery(conn: &rfcomm::Connection) -> Option<BatteryStatus> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if !conn.wait_readable(left.min(Duration::from_millis(300))) {
            continue;
        }
        let mut chunk = [0u8; 256];
        let n = conn.recv(&mut chunk)?;
        buf.extend_from_slice(&chunk[..n]);
        if let Some(status) = parse_fast_pair(&buf) {
            return Some(status);
        }
    }
    None
}

fn scale_for_model(code: &str) -> Scale {
    match code {
        "3005" | "3062" | "3388" | "3876" | "3957" => Scale { max_level: 10, level_offset: 1, known: true },
        "3959" => Scale { max_level: 10, level_offset: 0, known: true },
        "3954" => Scale { max_level: 100, level_offset: 0, known: true },
        _ if code.as_bytes().len() == 4
            && code.as_bytes()[0] == b'3'
            && code.as_bytes()[1..].iter().all(u8::is_ascii_digit) =>
        {
            Scale { max_level: 5, level_offset: 0, known: true }
        }
        _ => Scale::default(),
    }
}

fn scale_from_name(lower: &str) -> Scale {
    if lower.contains("space one pro") || lower.contains("liberty 5") || lower.contains("aeroclip") {
        Scale { max_level: 10, level_offset: 1, known: true }
    } else if lower.contains("liberty 4 pro") {
        Scale { max_level: 100, level_offset: 0, known: true }
    } else if lower.contains("p30i") || lower.contains("r50i nc") {
        Scale { max_level: 10, level_offset: 0, known: true }
    } else {
        Scale::default()
    }
}

fn is_dual_model(code: &str) -> bool {
    code.len() == 4 && code.starts_with('3') && (code.starts_with("39") || matches!(code, "3031" | "3330" | "3388" | "3876"))
}

/// Finds a 16-char serial number starting with the 4-digit model code, preferring the one that
/// follows a firmware string ("01.23", or "01.2301.23" on dual-firmware earbuds).
fn extract_model_code(body: &[u8]) -> String {
    let digit = |c: u8| c.is_ascii_digit();
    let alnum_upper = |c: u8| c.is_ascii_digit() || c.is_ascii_uppercase();
    let serial_at = |pos: usize| {
        let Some(s) = body.get(pos..pos + 16) else { return false };
        s[0] == b'3' && s[1..4].iter().all(|c| digit(*c)) && s[4..].iter().all(|c| alnum_upper(*c))
    };
    let fw_at = |pos: usize| {
        let Some(s) = body.get(pos..pos + 5) else { return false };
        digit(s[0]) && digit(s[1]) && s[2] == b'.' && digit(s[3]) && digit(s[4])
    };
    let mut i = 0;
    while i + 21 <= body.len() {
        if fw_at(i) {
            let mut sn = i + 5;
            if fw_at(sn) {
                sn += 5;
            }
            if serial_at(sn) {
                return String::from_utf8_lossy(&body[sn..sn + 4]).into_owned();
            }
        }
        i += 1;
    }
    let mut i = 0;
    while i + 16 <= body.len() {
        if (i == 0 || !(digit(body[i - 1]) || body[i - 1] == b'.')) && serial_at(i) {
            return String::from_utf8_lossy(&body[i..i + 4]).into_owned();
        }
        i += 1;
    }
    String::new()
}

fn raw_to_percent(raw: u8, scale: Scale) -> i32 {
    if raw == 0xFF {
        return -1;
    }
    if scale.known {
        if scale.max_level <= 0 {
            return -1;
        }
        return ((i32::from(raw) + scale.level_offset) * 100 / scale.max_level).clamp(0, 100);
    }
    // Unknown model: the common 0..5 scale unless the raw value says otherwise.
    match raw {
        0..=5 => i32::from(raw) * 20,
        6..=10 => i32::from(raw) * 10,
        11..=100 => i32::from(raw),
        _ => -1,
    }
}

fn parse_battery_level(body: &[u8], scale: Scale) -> Option<BatteryStatus> {
    let mut out = BatteryStatus::new();
    match body.len() {
        0 => return None,
        1 => {
            let percent = raw_to_percent(body[0], scale);
            if percent < 0 {
                return None;
            }
            out.percent = percent;
            Some(out)
        }
        _ => {
            // 2 bytes: [left, right]; 4 bytes add a newer left/right pair we ignore.
            let left = raw_to_percent(body[0], scale);
            let right = raw_to_percent(body[1], scale);
            if left < 0 && right < 0 {
                return None;
            }
            out.dual = true;
            out.left = left;
            out.right = right;
            out.percent = if left >= 0 && right >= 0 { left.min(right) } else { left.max(right) };
            Some(out)
        }
    }
}

fn apply_charging(body: &[u8], out: &mut BatteryStatus) -> bool {
    match body.len() {
        0 => false,
        1 => {
            out.charging = body[0] == 1;
            true
        }
        _ => {
            out.left_charging = body[0] == 1;
            out.right_charging = body[1] == 1;
            out.charging = (out.left >= 0 && out.left_charging) || (out.right >= 0 && out.right_charging);
            true
        }
    }
}

fn parse_state_update(body: &[u8], scale: Scale, is_dual: bool) -> Option<BatteryStatus> {
    let mut out = BatteryStatus::new();
    if is_dual {
        if body.len() < 6 {
            return None;
        }
        let left = raw_to_percent(body[2], scale);
        let right = raw_to_percent(body[3], scale);
        if left < 0 && right < 0 {
            return None;
        }
        out.dual = true;
        out.left = left;
        out.right = right;
        out.percent = if left >= 0 && right >= 0 { left.min(right) } else { left.max(right) };
        out.left_charging = body[4] == 1;
        out.right_charging = body[5] == 1;
        out.charging = (left >= 0 && out.left_charging) || (right >= 0 && out.right_charging);
        return Some(out);
    }
    if body.len() < 2 {
        return None;
    }
    let percent = raw_to_percent(body[0], scale);
    if percent < 0 {
        return None;
    }
    out.percent = percent;
    out.charging = body[1] == 1;
    Some(out)
}

/// Minimal Winsock RFCOMM client. Read-only: only the read commands above are sent.
pub(crate) mod rfcomm {
    use std::mem::size_of;
    use std::ptr::null_mut;
    use std::sync::OnceLock;
        use std::time::Duration;
    use windows_sys::core::GUID;

    const AF_BTH: i32 = 32;
    const SOCK_STREAM: i32 = 1;
    const BTHPROTO_RFCOMM: i32 = 3;
    const FIONBIO: u32 = 0x8004_667E;
    const WSAEWOULDBLOCK: i32 = 10035;

    #[repr(C, align(8))]
    struct WsaData([u8; 512]);

    #[repr(C)]
    struct SockaddrBth {
        family: u16,
        addr: u64,
        service: GUID,
        port: u32,
    }

    #[repr(C)]
    struct Timeval {
        sec: i32,
        usec: i32,
    }

    #[repr(C)]
    struct FdSet {
        count: u32,
        sockets: [usize; 64],
    }

    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn WSAStartup(version: u16, data: *mut WsaData) -> i32;
        fn WSAGetLastError() -> i32;
        fn socket(af: i32, kind: i32, protocol: i32) -> usize;
        fn connect(s: usize, name: *const SockaddrBth, len: i32) -> i32;
        fn send(s: usize, buf: *const u8, len: i32, flags: i32) -> i32;
        fn recv(s: usize, buf: *mut u8, len: i32, flags: i32) -> i32;
        fn select(nfds: i32, read: *mut FdSet, write: *mut FdSet, except: *mut FdSet, timeout: *const Timeval) -> i32;
        fn closesocket(s: usize) -> i32;
        fn ioctlsocket(s: usize, cmd: u32, arg: *mut u32) -> i32;
    }

    pub struct Connection(usize);

    impl Drop for Connection {
        fn drop(&mut self) {
            unsafe { closesocket(self.0) };
        }
    }

    impl Connection {
        pub fn open(addr: u64, service: GUID) -> Option<Connection> {
            static STARTED: OnceLock<bool> = OnceLock::new();
            let started = STARTED.get_or_init(|| unsafe {
                let mut data = WsaData([0; 512]);
                WSAStartup(0x0202, &mut data) == 0
            });
            if !started {
                return None;
            }
            let s = unsafe { socket(AF_BTH, SOCK_STREAM, BTHPROTO_RFCOMM) };
            if s == usize::MAX {
                return None;
            }
            let conn = Connection(s);
            let mut on: u32 = 1;
            unsafe { ioctlsocket(s, FIONBIO, &mut on) };
            let target = SockaddrBth { family: AF_BTH as u16, addr, service, port: 0 };
            let r = unsafe { connect(s, &target, size_of::<SockaddrBth>() as i32) };
            if r != 0 && (unsafe { WSAGetLastError() } != WSAEWOULDBLOCK || !wait(s, true, Duration::from_millis(2500))) {
                return None;
            }
            Some(conn)
        }

        pub fn send_all(&self, mut data: &[u8]) -> bool {
            while !data.is_empty() {
                if !wait(self.0, true, Duration::from_secs(1)) {
                    return false;
                }
                let n = unsafe { send(self.0, data.as_ptr(), data.len().min(i32::MAX as usize) as i32, 0) };
                if n <= 0 {
                    return false;
                }
                data = &data[n as usize..];
            }
            true
        }

        pub fn wait_readable(&self, timeout: Duration) -> bool {
            wait(self.0, false, timeout)
        }

        pub fn recv(&self, buf: &mut [u8]) -> Option<usize> {
            let n = unsafe { recv(self.0, buf.as_mut_ptr(), buf.len().min(i32::MAX as usize) as i32, 0) };
            (n > 0).then_some(n as usize)
        }
    }

    fn wait(s: usize, for_write: bool, timeout: Duration) -> bool {
        let mut set = FdSet { count: 1, sockets: [0; 64] };
        set.sockets[0] = s;
        let mut except = FdSet { count: 1, sockets: [0; 64] };
        except.sockets[0] = s;
        let (read, write): (*mut FdSet, *mut FdSet) =
            if for_write { (null_mut(), &mut set) } else { (&mut set, null_mut()) };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let tv = Timeval { sec: ms / 1000, usec: (ms % 1000) * 1000 };
        let ready = unsafe { select(0, read, write, &mut except, &tv) };
        ready > 0 && set.count > 0 && except.count == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inbound(cmd: Command, body: &[u8]) -> Vec<u8> {
        let total = 10 + body.len();
        let mut p = vec![0x09, 0xFF, 0x00, 0x00, 0x01, cmd.group, cmd.id, (total & 0xFF) as u8, (total >> 8) as u8];
        p.extend_from_slice(body);
        let sum = checksum(&p);
        p.push(sum);
        p
    }

    #[test]
    fn builds_the_documented_request() {
        // 08 EE 00 00 00 01 03 0A 00 04, from the protocol notes.
        assert_eq!(build_empty_request(BATTERY_LEVEL), [0x08, 0xEE, 0x00, 0x00, 0x00, 0x01, 0x03, 0x0A, 0x00, 0x04]);
        assert_eq!(build_empty_request(STATE), [0x08, 0xEE, 0x00, 0x00, 0x00, 0x01, 0x01, 0x0A, 0x00, 0x02]);
    }

    #[test]
    fn finds_and_checks_inbound_packets() {
        let frame = inbound(BATTERY_LEVEL, &[4]);
        assert_eq!(find_inbound(&frame, BATTERY_LEVEL).as_deref(), Some(&[4u8][..]));
        // Unrelated packets, a split read and a later matching packet: the last one wins.
        let mut stream = inbound(STATE, &[1, 2, 3]);
        stream.extend_from_slice(&frame);
        stream.extend_from_slice(&inbound(BATTERY_LEVEL, &[5]));
        assert_eq!(find_inbound(&stream, BATTERY_LEVEL).as_deref(), Some(&[5u8][..]));

        let mut bad = frame.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        assert!(find_inbound(&bad, BATTERY_LEVEL).is_none());
        assert!(find_inbound(&frame[..frame.len() - 1], BATTERY_LEVEL).is_none());
    }

    #[test]
    fn maps_model_codes_to_scales() {
        assert_eq!(scale_for_model("3028").max_level, 5); // Life Q30: 0..5
        assert_eq!(scale_for_model("3959").max_level, 10); // P30i / R50i NC
        assert_eq!(scale_for_model("3954").max_level, 100); // Liberty 4 Pro
        assert_eq!(scale_for_model("3957").level_offset, 1); // Liberty 5: 0..9 -> 10..100
        assert!(!scale_for_model("9999").known);
        assert!(is_dual_model("3957") && is_dual_model("3031") && !is_dual_model("3028"));
        assert_eq!(scale_from_name("soundcore liberty 4 pro").max_level, 100);
    }

    #[test]
    fn extracts_the_model_code() {
        let mut body = b"04.33".to_vec();
        body.extend_from_slice(b"3028A1B2C3D4E5F6");
        assert_eq!(extract_model_code(&body), "3028");
        // Dual firmware "01.2301.23" followed by the serial.
        let mut body = b"01.2301.23".to_vec();
        body.extend_from_slice(b"3959ABC123456789");
        assert_eq!(extract_model_code(&body), "3959");
        assert_eq!(extract_model_code(b"nothing here"), "");
    }

    #[test]
    fn battery_bodies() {
        // Single headphone: [level] on the common 0..5 scale.
        let s = parse_battery_level(&[3], Scale::default()).unwrap();
        assert_eq!((s.percent, s.dual), (60, false));
        // TWS: [left, right], 0xFF = disconnected.
        let s = parse_battery_level(&[2, 0xFF], Scale::default()).unwrap();
        assert!(s.dual);
        assert_eq!((s.left, s.right, s.percent), (40, -1, 40));
        // 0..100 model.
        let s = parse_battery_level(&[87], Scale { max_level: 100, level_offset: 0, known: true }).unwrap();
        assert_eq!(s.percent, 87);
        assert!(parse_battery_level(&[0xFF], Scale::default()).is_none());

        let mut s = BatteryStatus::new();
        s.percent = 80;
        assert!(apply_charging(&[1], &mut s));
        assert!(s.charging);

        // State update fallback: dual [tws, host, left, right, lChg, rChg].
        let s = parse_state_update(&[1, 0, 4, 3, 1, 0], Scale::default(), true).unwrap();
        assert_eq!((s.left, s.right, s.percent), (80, 60, 60));
        assert!(s.charging);
    }

    #[test]
    fn fast_pair_stream_bodies() {
        // Group 03, code 03, one byte: bit 7 charging, 0x50 = 80%.
        let s = parse_fast_pair(&[0x03, 0x03, 0x00, 0x01, 0xD0]).unwrap();
        assert_eq!((s.percent, s.charging), (80, true));
        // TWS: left 80%, right 60%.
        let s = parse_fast_pair(&[0x03, 0x03, 0x00, 0x02, 0x50, 0x3C]).unwrap();
        assert!(s.dual);
        assert_eq!((s.left, s.right, s.percent), (80, 60, 60));
        // An unrelated message is ignored.
        assert!(parse_fast_pair(&[0x03, 0x01, 0x00, 0x03, 0x15, 0xAE, 0x46]).is_none());
    }
}
