//! Marshall EMBERTON II Bluetooth speaker.
//!
//! Paired with Windows the speaker is audio only (A2DP + AVRCP, no Hands-Free), so Windows has no
//! battery level for it. The level lives on its Bluetooth LE side, which Marshall's app reads: the
//! standard Battery Service `0x180F`, Battery Level `0x2A19`, one byte 0-100 (10% steps). Windows
//! does not pair that LE side, but the speaker lets an unpaired client read the level.
//!
//! The LE side advertises from a rotating private address, so the advertisement alone does not say
//! which "[LE]" device belongs to this PC. The speaker's Fast Pair Message Stream (RFCOMM, reachable
//! through the Classic pairing) does: right after the stream opens it sends its Fast Pair model id
//! and its current LE address. Nothing is ever sent on the stream, and on LE only Battery Level is
//! read (no writes, no notifications).
//!
//! Right after an LE connection the speaker can report a placeholder 100% for about two seconds, so
//! the level is read twice, 2.5 s apart, and the second read is used.
//!
//! Worked out from an Android HCI log of the Marshall app and tested on a real EMBERTON II
//! (firmware 14.0.35). Ported from marshall.cpp / fastpair_stream.h.
//!
//! `shortcut: Windows only resolves a random LE address it has heard advertising, which needs the
//! BleAdvertisementWatcher (Devices_Bluetooth_Advertisement, not enabled), so a failed lookup is
//! expected when no other app is scanning; upgrade by adding that feature and a passive watcher.`

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::wide;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattCharacteristicUuids, GattCommunicationStatus, GattServiceUuids,
};
use windows::Devices::Bluetooth::{BluetoothAddressType, BluetoothCacheMode, BluetoothLEDevice};
use windows::Storage::Streams::DataReader;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::Properties::{DEVPROP_TYPE_BOOLEAN, DEVPROP_TYPE_STRING, DEVPROPKEY};
use windows_sys::core::GUID;

/// Fast Pair Message Stream (RFCOMM): `{df21fe2c-2515-4fdb-8886-f12c4d67927c}`.
const MESSAGE_STREAM: GUID = GUID::from_u128(0xDF21FE2C_2515_4FDB_8886_F12C4D67927C);
const STREAM_NODE: &str = "BTHENUM\\{DF21FE2C-2515-4FDB-8886-F12C4D67927C}";
/// Fast Pair model id this driver reads. Only the EMBERTON II has been checked.
const EMBERTON_II: u32 = 0x15AE46;

const QUERY_EVERY: Duration = Duration::from_secs(120);
const RETRY: Duration = Duration::from_secs(60);
const STALE: Duration = Duration::from_secs(6 * 60);
const SETTLE: Duration = Duration::from_millis(2500);

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

/// Stateless: the readings live in the shared cache the query thread writes.
#[derive(Default)]
pub struct Marshall;

#[derive(Default)]
struct Entry {
    last_attempt: Option<Instant>,
    last_ok: Option<Instant>,
    percent: u8,
}

/// One query at a time; a scan during a query reports the previous reading instead of blocking the
/// poll thread for the seconds the LE connection takes.
#[derive(Default)]
struct Shared {
    entries: HashMap<u64, Entry>,
    pending: Vec<Speaker>,
    running: bool,
}

fn shared() -> &'static Mutex<Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| Mutex::new(Shared::default()))
}

impl Marshall {
    pub fn scan(&mut self) -> Vec<Observation> {
        let speakers = find_connected();
        if speakers.is_empty() {
            return Vec::new();
        }
        let now = Instant::now();
        let mut state = shared().lock().unwrap();
        if !state.running {
            let mut due = Vec::new();
            for speaker in &speakers {
                let entry = state.entries.entry(speaker.addr).or_default();
                let wait = if entry.last_ok.is_some_and(|ok| entry.last_attempt.is_none_or(|at| ok >= at)) {
                    QUERY_EVERY
                } else {
                    RETRY
                };
                if entry.last_attempt.is_some_and(|at| now.duration_since(at) < wait) {
                    continue;
                }
                entry.last_attempt = Some(now);
                due.push(speaker.clone());
            }
            if !due.is_empty() {
                state.pending = due;
                state.running = true;
                let _ = std::thread::spawn(query_thread);
            }
        }
        speakers
            .iter()
            .filter_map(|speaker| {
                let entry = state.entries.get(&speaker.addr)?;
                let ok = entry.last_ok?;
                (now.duration_since(ok) < STALE).then(|| Observation {
                    id: format!("marshall:{:012x}", speaker.addr),
                    name: Some(speaker.name.clone()),
                    kind: Some(Kind::Other),
                    state: State::Battery { percent: entry.percent, charging: false },
                })
            })
            .collect()
    }
}

#[derive(Clone)]
struct Speaker {
    name: String,
    addr: u64,
}

/// Paired, connected EMBERTON speakers whose Classic pairing exposes the Fast Pair Message Stream.
fn find_connected() -> Vec<Speaker> {
    let nodes = bt_nodes();
    let streams: Vec<String> = nodes
        .iter()
        .map(|n| n.id.to_ascii_uppercase())
        .filter(|id| id.starts_with(STREAM_NODE))
        .collect();
    let mut out = Vec::new();
    for node in &nodes {
        if !node.connected || !node.name.to_ascii_lowercase().starts_with("emberton") {
            continue;
        }
        let Some(addr) = node.addr else { continue };
        let want = format!("&{:012X}_", addr);
        if streams.iter().any(|id| id.contains(&want)) {
            out.push(Speaker { name: node.name.clone(), addr });
        }
    }
    out
}

struct Node {
    id: String,
    addr: Option<u64>,
    connected: bool,
    name: String,
}

fn bt_nodes() -> Vec<Node> {
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
            let addr = id
                .to_ascii_uppercase()
                .strip_prefix("BTHENUM\\DEV_")
                .and_then(|rest| rest.get(..12))
                .and_then(|hex| u64::from_str_radix(hex, 16).ok())
                .filter(|a| *a != 0);
            out.push(Node {
                connected: bool_prop(set, &data, &CONNECTED_KEY).unwrap_or(false),
                name: string_prop(set, &data, &NAME_KEY).unwrap_or_default(),
                id,
                addr,
            });
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

/// Fast Pair Message Stream: `[group][code][length, 2 bytes big-endian][data...]`. Group 0x03 can
/// leave the device's LE address until the next retry.
#[derive(Default)]
struct Info {
    model_id: Option<u32>,
    le_address: Option<u64>,
}

/// Consumes every complete message in `data`; returns the bytes consumed. A trailing partial
/// message is left for bytes read after the next retry.
fn parse_stream(data: &[u8], info: &mut Info) -> usize {
    let mut pos = 0;
    while data.len() - pos >= 4 {
        let len = usize::from(data[pos + 2]) << 8 | usize::from(data[pos + 3]);
        if data.len() - pos - 4 < len {
            break;
        }
        let p = &data[pos + 4..pos + 4 + len];
        if data[pos] == 0x03 && data[pos + 1] == 0x01 && len == 3 {
            info.model_id = Some(u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2]));
        } else if data[pos] == 0x03 && data[pos + 1] == 0x02 && len == 6 {
            let addr = p.iter().fold(0u64, |a, b| a << 8 | u64::from(*b));
            info.le_address = (addr != 0).then_some(addr);
        }
        pos += 4 + len;
    }
    pos
}

/// Opens the Message Stream and collects what the speaker sends by itself. Listen only.
fn read_stream_info(addr: u64) -> Option<Info> {
    let conn = rfcomm::Connection::open(addr, MESSAGE_STREAM, Duration::from_secs(3))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut info = Info::default();
    let mut buf = Vec::new();
    while !(info.model_id.is_some() && info.le_address.is_some()) && buf.len() < 4096 {
        let left = deadline.checked_duration_since(Instant::now())?;
        if !conn.wait_readable(left) {
            break;
        }
        let mut chunk = [0u8; 512];
        let n = conn.recv(&mut chunk)?;
        buf.extend_from_slice(&chunk[..n]);
        let used = parse_stream(&buf, &mut info);
        buf.drain(..used);
    }
    (info.model_id.is_some() && info.le_address.is_some()).then_some(info)
}

/// Battery Level of the speaker's LE side at the address the Fast Pair stream reported. The address
/// is random (rotating private), so the public type is tried as a fallback.
fn read_battery_level(le_address: u64) -> Option<u8> {
    read_level(le_address, true).or_else(|| read_level(le_address, false))
}

fn read_level(address: u64, random: bool) -> Option<u8> {
    let kind = if random { BluetoothAddressType::Random } else { BluetoothAddressType::Public };
    let device = BluetoothLEDevice::FromBluetoothAddressWithBluetoothAddressTypeAsync(address, kind).ok()?.get().ok()?;
    let services = device
        .GetGattServicesForUuidWithCacheModeAsync(GattServiceUuids::Battery().ok()?, BluetoothCacheMode::Uncached)
        .ok()?
        .get()
        .ok()?;
    if services.Status().ok()? != GattCommunicationStatus::Success {
        let _ = device.Close();
        return None;
    }
    let list = services.Services().ok()?;
    if list.Size().ok()? == 0 {
        let _ = device.Close();
        return None;
    }
    let service = list.GetAt(0).ok()?;
    let chars = service
        .GetCharacteristicsForUuidWithCacheModeAsync(GattCharacteristicUuids::BatteryLevel().ok()?, BluetoothCacheMode::Uncached)
        .ok()?
        .get()
        .ok()?;
    let level = if chars.Status().ok()? != GattCommunicationStatus::Success {
        None
    } else {
        let chars = chars.Characteristics().ok()?;
        (chars.Size().ok()? > 0).then(|| chars.GetAt(0).ok()).flatten().and_then(|ch| {
            // The first value after connecting can be a placeholder 100%: read again once settled.
            let first = read_once(&ch);
            std::thread::sleep(SETTLE);
            read_once(&ch).or(first)
        })
    };
    let _ = device.Close(); // drops the LE connection
    level
}

fn read_once(ch: &GattCharacteristic) -> Option<u8> {
    let value = ch.ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached).ok()?.get().ok()?;
    if value.Status().ok()? != GattCommunicationStatus::Success {
        return None;
    }
    let level = DataReader::FromBuffer(&value.Value().ok()?).ok()?.ReadByte().ok()?;
    (level <= 100).then_some(level)
}

fn query_thread() {
    let pending = {
        let mut state = shared().lock().unwrap();
        std::mem::take(&mut state.pending)
    };
    for speaker in pending {
        let key = format!("marshall:{}", diag::bt_tag(speaker.addr));
        let level = read_stream_info(speaker.addr).and_then(|info| {
            if info.model_id != Some(EMBERTON_II) {
                diag::event(&key, format!("Fast Pair model {:06X}, not a known Marshall speaker", info.model_id.unwrap_or(0)));
                return None;
            }
            read_battery_level(info.le_address?)
        });
        match level {
            Some(level) => {
                diag::event(&key, format!("LE Battery Level {level}%"));
                let mut state = shared().lock().unwrap();
                let entry = state.entries.entry(speaker.addr).or_default();
                entry.percent = level;
                entry.last_ok = Some(Instant::now());
            }
            None => diag::event(&key, "no LE battery level (speaker off, out of range, or the address was not heard)"),
        }
    }
    shared().lock().unwrap().running = false;
}

/// Minimal Winsock RFCOMM client, read-only: the Message Stream is only listened to.
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
        pub fn open(addr: u64, service: GUID, timeout: Duration) -> Option<Connection> {
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
            if r != 0 && (unsafe { WSAGetLastError() } != WSAEWOULDBLOCK || !wait(s, true, timeout)) {
                return None;
            }
            Some(conn)
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
    fn fast_pair_messages() {
        // Model id 0x15AE46, then the LE address aa:bb:cc:dd:ee:ff, in one read.
        let mut info = Info::default();
        let stream = [0x03, 0x01, 0x00, 0x03, 0x15, 0xAE, 0x46, 0x03, 0x02, 0x00, 0x06, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        assert_eq!(parse_stream(&stream, &mut info), stream.len());
        assert_eq!(info.model_id, Some(0x15AE46));
        assert_eq!(info.le_address, Some(0xAABBCCDDEEFF));
    }

    #[test]
    fn partial_message_waits_for_more() {
        let mut info = Info::default();
        let head = [0x03, 0x01, 0x00, 0x03, 0x15, 0xAE];
        assert_eq!(parse_stream(&head, &mut info), 0);
        assert_eq!(info.model_id, None);
        let mut full = head.to_vec();
        full.push(0x46);
        let used = parse_stream(&full, &mut info);
        assert_eq!(used, 7);
        assert_eq!(info.model_id, Some(0x15AE46));
        // A zero address is not a usable LE address.
        let mut info = Info::default();
        parse_stream(&[0x03, 0x02, 0x00, 0x06, 0, 0, 0, 0, 0, 0], &mut info);
        assert_eq!(info.le_address, None);
    }
}
