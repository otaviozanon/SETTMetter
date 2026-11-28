//! Sony Bluetooth headphones and earbuds (WH-1000XM3/XM4/XM5, WF-1000XM3/XM4/XM5, WH-CH720N,
//! LinkBuds, ULT Wear, ...).
//!
//! Windows has no battery level for most of them, so they are asked over the RFCOMM service the
//! Sony Headphones Connect app uses, with read-only requests (protocol info and battery level):
//!
//! - service `{96cc203e-5068-46ad-b32d-e316f5e069ba}` (most models)
//! - service `{956c7b26-d49a-4ba8-b03f-b17d393cb6e2}` (newer firmware / models)
//!
//! Only paired devices Windows reports as connected and whose profile nodes carry Sony's Bluetooth
//! SIG vendor id (`BTHENUM ..._VID&0002054C_...`) are tried. If the phone app holds the channel on
//! the same headphones, the query fails and the last reading is kept as stale.
//!
//! Frame: `3E [type][seq][len: 4 bytes big-endian][payload...][checksum] 3C`, checksum = sum of
//! type, seq, len and payload bytes; `3C`/`3D`/`3E` inside the frame are escaped as `3D (byte &
//! 0xEF)`. Types: `0x01` ACK, `0x0C` command/reply. Ported from Gadgetbridge via
//! Wireless-Device-Battery-Monitor's sony_headphones.cpp / sony_headphones_protocol.h.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::wide;
use std::collections::HashSet;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::Properties::{DEVPROP_TYPE_BOOLEAN, DEVPROP_TYPE_STRING, DEVPROPKEY};
use windows_sys::core::GUID;

/// `{96cc203e-5068-46ad-b32d-e316f5e069ba}`
const MDR_V1: GUID = GUID::from_u128(0x96cc203e_5068_46ad_b32d_e316f5e069ba);
/// `{956c7b26-d49a-4ba8-b03f-b17d393cb6e2}`
const MDR_V2: GUID = GUID::from_u128(0x956c7b26_d49a_4ba8_b03f_b17d393cb6e2);
/// Sony's Bluetooth SIG vendor id in a profile node instance id (`_VID&0002054C_`).
const SONY_NODE_MARK: &str = "_VID&0002054C_";

const START: u8 = 0x3E;
const END: u8 = 0x3C;
const ESCAPE: u8 = 0x3D;
const TYPE_ACK: u8 = 0x01;
const TYPE_COMMAND: u8 = 0x0C;

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

/// Fresh instances are made on every poll, so which headphones ever answered lives in a module
/// static: until then a silent link isn't listed.
#[derive(Default)]
pub struct SonyHeadphones;

fn seen() -> &'static Mutex<HashSet<u64>> {
    static SEEN: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

impl SonyHeadphones {
    pub fn scan(&mut self) -> Vec<Observation> {
        let mut out = Vec::new();
        for h in find_connected() {
            let key = format!("sonymdr:{}", diag::bt_tag(h.addr));
            let battery = read_battery(h.addr, &MDR_V1).or_else(|| read_battery(h.addr, &MDR_V2));
            let Some(b) = battery else {
                diag::event(&key, "no answer on the MDR service (busy link or another app holds it)");
                if seen().lock().unwrap().contains(&h.addr) {
                    out.push(observation(&h, State::Unreachable));
                }
                continue;
            };
            if b.level < 0 {
                continue;
            }
            seen().lock().unwrap().insert(h.addr);
            diag::event(
                &key,
                format!(
                    "MDR level {}%{}{}{}",
                    b.level,
                    if b.charging { ", charging" } else { "" },
                    if b.left >= 0 || b.right >= 0 { format!(", L {}, R {}", pct(b.left), pct(b.right)) } else { String::new() },
                    if b.case_level >= 0 { format!(", case {}", b.case_level) } else { String::new() },
                ),
            );
            out.push(observation(
                &h,
                State::Battery { percent: b.level.clamp(0, 100) as u8, charging: b.charging },
            ));
        }
        out
    }
}

fn observation(h: &Headphones, state: State) -> Observation {
    Observation { id: format!("sonymdr:{:012x}", h.addr), name: Some(h.name.clone()), kind: Some(Kind::Headset), state }
}

fn pct(v: i32) -> String {
    if v >= 0 { format!("{v}%") } else { "-".into() }
}

struct Headphones {
    name: String,
    addr: u64,
}

/// Paired, connected Sony device nodes: profile nodes with Sony's vendor id name the addresses,
/// the device nodes (`BTHENUM\DEV_<address>`) carry the name and connection state.
fn find_connected() -> Vec<Headphones> {
    let nodes = bt_nodes();
    let mut sony = HashSet::new();
    for (id, _, _) in &nodes {
        let id = id.to_ascii_uppercase();
        if !id.contains(SONY_NODE_MARK) {
            continue;
        }
        let Some(tail) = id.rfind("_C00000000") else { continue };
        if tail < 12 {
            continue;
        }
        sony.insert(id[tail - 12..tail].to_string());
    }
    let mut out = Vec::new();
    for (id, connected, name) in &nodes {
        let upper = id.to_ascii_uppercase();
        let Some(rest) = upper.strip_prefix("BTHENUM\\DEV_") else { continue };
        let Some(hex) = rest.get(..12) else { continue };
        if !*connected || hex.len() != 12 || !sony.contains(hex) || name.is_empty() {
            continue;
        }
        let Some(addr) = u64::from_str_radix(hex, 16).ok().filter(|a| *a != 0) else { continue };
        out.push(Headphones { name: name.clone(), addr });
    }
    out
}

/// (instance id, connected, friendly name) of every present BTHENUM node.
fn bt_nodes() -> Vec<(String, bool, String)> {
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
            let connected = bool_prop(set, &data, &CONNECTED_KEY).unwrap_or(false);
            let name = string_prop(set, &data, &NAME_KEY).unwrap_or_default();
            out.push((String::from_utf16_lossy(&id[..len]), connected, name));
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
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

/// One MDR message.
struct Message {
    ty: u8,
    seq: u8,
    payload: Vec<u8>,
}

fn encode(ty: u8, seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut raw = vec![ty, seq];
    raw.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    raw.extend_from_slice(payload);
    let sum = raw.iter().fold(0u8, |s, b| s.wrapping_add(*b));
    raw.push(sum);
    let mut out = vec![START];
    for b in raw {
        if b == START || b == END || b == ESCAPE {
            out.push(ESCAPE);
            out.push(b & 0xEF);
        } else {
            out.push(b);
        }
    }
    out.push(END);
    out
}

/// Takes the first complete, valid frame out of `buf` (dropping anything before it).
fn take_message(buf: &mut Vec<u8>) -> Option<Message> {
    loop {
        let Some(start) = buf.iter().position(|b| *b == START) else {
            buf.clear();
            return None;
        };
        buf.drain(..start);
        let end = buf[1..].iter().position(|b| *b == END || *b == START)? + 1;
        if buf[end] == START {
            buf.drain(..end); // truncated frame: resync on the next start byte
            continue;
        }
        let mut raw = Vec::new();
        let mut bad = false;
        let mut i = 1;
        while i < end {
            if buf[i] == ESCAPE {
                i += 1;
                if i >= end {
                    bad = true;
                    break;
                }
                raw.push(buf[i] | 0x10);
            } else {
                raw.push(buf[i]);
            }
            i += 1;
        }
        buf.drain(..=end);
        if bad || raw.len() < 7 {
            continue;
        }
        let len = u32::from_be_bytes([raw[2], raw[3], raw[4], raw[5]]) as usize;
        if len != raw.len() - 7 {
            continue;
        }
        let sum = raw[..raw.len() - 1].iter().fold(0u8, |s, b| s.wrapping_add(*b));
        if sum != raw[raw.len() - 1] {
            continue;
        }
        return Some(Message { ty: raw[0], seq: raw[1], payload: raw[6..raw.len() - 1].to_vec() });
    }
}

#[derive(Clone, Copy)]
struct Battery {
    level: i32,
    left: i32,
    right: i32,
    case_level: i32,
    charging: bool,
    dual: bool,
}

impl Battery {
    fn new() -> Battery {
        Battery { level: -1, left: -1, right: -1, case_level: -1, charging: false, dual: false }
    }
}

fn level(v: u8) -> i32 {
    if v <= 100 { i32::from(v) } else { -1 }
}

/// Applies a battery reply (payload starting with `0x11`) to `b`.
fn parse_battery(p: &[u8], b: &mut Battery) -> bool {
    if p.len() < 4 || p[0] != 0x11 {
        return false;
    }
    match p[1] {
        0x00 => {
            b.level = level(p[2]);
            b.charging = p[3] == 1;
            b.level >= 0
        }
        0x01 if p.len() >= 6 => {
            b.dual = true;
            b.left = if p[2] >= 1 { level(p[2]) } else { -1 };
            b.right = if p[4] >= 1 { level(p[4]) } else { -1 };
            b.charging = p[3] == 1 || p[5] == 1;
            b.level = match (b.left, b.right) {
                (l, r) if l >= 0 && r >= 0 => l.min(r),
                (l, r) => l.max(r),
            };
            b.level >= 0
        }
        0x0A => {
            b.case_level = if p[2] >= 1 { level(p[2]) } else { -1 };
            true
        }
        _ => false,
    }
}

/// A connected MDR session; the first failed read ends it (the 4 s deadline is the link budget).
struct Session {
    conn: rfcomm::Connection,
    deadline: Instant,
    seq: u8,
    buf: Vec<u8>,
}

impl Session {
    fn new(conn: rfcomm::Connection) -> Session {
        Session { conn, deadline: Instant::now() + Duration::from_secs(4), seq: 0, buf: Vec::new() }
    }

    /// Sends a command and waits for the reply whose first payload byte is `reply_id`.
    fn ask(&mut self, payload: &[u8], reply_id: u8, timeout: Duration) -> Option<Message> {
        if !self.conn.send_all(&encode(TYPE_COMMAND, self.seq, payload)) {
            return None;
        }
        let until = (Instant::now() + timeout).min(self.deadline);
        loop {
            let m = self.next(until)?;
            match m.ty {
                TYPE_ACK => self.seq = m.seq, // the headphones' expected sequence number
                TYPE_COMMAND => {
                    let _ = self.conn.send_all(&encode(TYPE_ACK, 1u8.wrapping_sub(m.seq), &[]));
                    if m.payload.first() == Some(&reply_id) {
                        return Some(m);
                    }
                }
                _ => {}
            }
        }
    }

    fn next(&mut self, until: Instant) -> Option<Message> {
        loop {
            if let Some(m) = take_message(&mut self.buf) {
                return Some(m);
            }
            let now = Instant::now();
            if now >= until || self.buf.len() > 8192 || !self.conn.wait_readable(until - now) {
                return None;
            }
            let mut chunk = [0u8; 512];
            let n = self.conn.recv(&mut chunk)?;
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

fn read_battery(addr: u64, service: &GUID) -> Option<Battery> {
    let mut session = Session::new(rfcomm::Connection::open(addr, *service)?);
    // Protocol info first: some models ignore requests until asked.
    let _ = session.ask(&[0x00, 0x00], 0x01, Duration::from_millis(1200));
    if let Some(m) = session.ask(&[0x10, 0x00], 0x11, Duration::from_millis(1200)) {
        let mut b = Battery::new();
        if parse_battery(&m.payload, &mut b) && !b.dual {
            return Some(b);
        }
    }
    let m = session.ask(&[0x10, 0x01], 0x11, Duration::from_millis(1200))?;
    let mut b = Battery::new();
    if !parse_battery(&m.payload, &mut b) || b.level < 0 {
        return None;
    }
    if let Some(m) = session.ask(&[0x10, 0x0A], 0x11, Duration::from_millis(800)) {
        parse_battery(&m.payload, &mut b);
    }
    Some(b)
}

/// Minimal Winsock RFCOMM client: Windows looks the channel number up over SDP, so only the
/// address and the service UUID are needed. Read-only: nothing but these MDR frames is sent.
mod rfcomm {
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

    /// SOCKADDR_BTH.
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
            if r != 0 && (unsafe { WSAGetLastError() } != WSAEWOULDBLOCK || !wait(s, true, Duration::from_secs(4))) {
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

    #[test]
    fn frame_roundtrip() {
        // 00 00 protocol info request, seq 0, empty payload.
        assert_eq!(encode(TYPE_COMMAND, 0, &[0x00, 0x00]), [0x3E, 0x0C, 0x00, 0, 0, 0, 2, 0x00, 0x00, 0x0E, 0x3C]);
        let mut buf = encode(TYPE_COMMAND, 0, &[0x00, 0x00]);
        let m = take_message(&mut buf).expect("complete frame");
        assert_eq!((m.ty, m.seq, m.payload.as_slice()), (TYPE_COMMAND, 0, &[0x00, 0x00][..]));
        assert!(buf.is_empty());
        assert!(take_message(&mut buf).is_none());
    }

    #[test]
    fn escapes_and_skips_bad_checksum() {
        // Payload with the frame delimiters: START, END and ESCAPE must be escaped and restored.
        let payload = [START, END, ESCAPE];
        let mut buf = encode(TYPE_COMMAND, 1, &payload);
        assert!(buf.iter().filter(|b| **b == START).count() == 1);
        let m = take_message(&mut buf).expect("escaped frame");
        assert_eq!(m.payload, payload);

        let mut bad = encode(TYPE_COMMAND, 0, &[0x10, 0x00]);
        let last = bad.len() - 2;
        bad[last] ^= 0xFF; // checksum
        assert!(take_message(&mut bad).is_none());
    }

    #[test]
    fn battery_replies() {
        let mut b = Battery::new();
        assert!(parse_battery(&[0x11, 0x00, 80, 0], &mut b));
        assert_eq!((b.level, b.charging, b.dual), (80, false, false));

        let mut b = Battery::new();
        assert!(parse_battery(&[0x11, 0x01, 70, 1, 60, 0], &mut b));
        assert!(b.dual && b.charging);
        assert_eq!((b.left, b.right, b.level), (70, 60, 60));

        let mut b = Battery::new();
        assert!(parse_battery(&[0x11, 0x0A, 35, 0], &mut b));
        assert_eq!(b.case_level, 35);
        // A disconnected earbud (0) is reported as unknown, not as 0%.
        let mut b = Battery::new();
        assert!(parse_battery(&[0x11, 0x01, 0, 0, 55, 0], &mut b));
        assert_eq!((b.left, b.level), (-1, 55));
        // Out-of-range levels are rejected.
        let mut b = Battery::new();
        assert!(!parse_battery(&[0x11, 0x00, 120, 0], &mut b));
        assert!(!parse_battery(&[0x10, 0x00, 80, 0], &mut b));
    }

    #[test]
    fn sony_nodes_are_matched_by_vendor_id() {
        // Instance ids as Windows writes them: profile node carries the vendor id, the device
        // node the address; only the connected device node yields a device.
        let nodes = vec![
            (
                r"BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&0002054C_PID&0D58\7&1&88C9E8390284_C00000000"
                    .to_string(),
                false,
                String::new(),
            ),
            (r"BTHENUM\DEV_88C9E8390284\7&2&88C9E8390284".to_string(), true, "WH-1000XM5".to_string()),
            (r"BTHENUM\DEV_001122334455\7&3&001122334455".to_string(), true, "Other".to_string()),
        ];
        let sony: HashSet<String> = nodes
            .iter()
            .filter_map(|(id, _, _)| {
                let id = id.to_ascii_uppercase();
                let tail = id.rfind("_C00000000").filter(|t| *t >= 12)?;
                id.contains(SONY_NODE_MARK).then(|| id[tail - 12..tail].to_string())
            })
            .collect();
        assert_eq!(sony.len(), 1);
        assert!(sony.contains("88C9E8390284"));
    }
}
