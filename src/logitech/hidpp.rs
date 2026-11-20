//! Minimal HID++ transport for Logitech devices.
//!
//! Message layout: [report_id, device_index, feature_index, (function << 4) | sw_id, params...]
//!
//! On Windows each report size is a separate HID collection, so we write long reports to the
//! long collection but also listen on the short one: receivers answer some errors (e.g. "no
//! device in this slot") with short HID++ 1.0 reports.

use hidapi::{HidApi, HidDevice};
use std::ffi::CString;
use std::fmt;
use std::time::{Duration, Instant};

pub const LOGITECH_VID: u16 = 0x046D;

const REPORT_SHORT: u8 = 0x10;
const REPORT_LONG: u8 = 0x11;
const LEN_LONG: usize = 20;
/// Our software id, so we can tell our replies apart from G HUB's.
const SW_ID: u8 = 0x0B;
const TIMEOUT: Duration = Duration::from_millis(600);

/// HID++ 1.0 error: no device paired at this receiver index.
pub const ERR_UNKNOWN_DEVICE: u8 = 0x08;

#[derive(Debug)]
pub enum Error {
    Hid(hidapi::HidError),
    /// No reply in time (device asleep or off).
    Timeout,
    /// Device or receiver returned an HID++ error code.
    Protocol(u8),
}

impl From<hidapi::HidError> for Error {
    fn from(e: hidapi::HidError) -> Self {
        Error::Hid(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Hid(e) => write!(f, "hid: {e}"),
            Error::Timeout => write!(f, "timeout"),
            Error::Protocol(c) => write!(f, "hid++ error 0x{c:02X}"),
        }
    }
}

/// One HID++-capable USB interface (a receiver, or a directly attached device/dongle).
pub struct Endpoint {
    pub pid: u16,
    pub iface: i32,
    pub product: String,
    pub(super) short: Option<CString>,
    pub(super) long: CString,
}

/// One HID top-level collection as reported by the OS (a plain-data view of `DeviceInfo`).
pub struct Collection {
    pub pid: u16,
    pub iface: i32,
    pub product: String,
    pub usage_page: u16,
    pub usage: u16,
    pub path: CString,
}

/// Groups Logitech vendor-defined HID collections by interface, keeping those that accept
/// long (HID++ 2.0) reports.
pub fn endpoints(api: &HidApi) -> Vec<Endpoint> {
    group_collections(api.device_list().filter(|d| d.vendor_id() == LOGITECH_VID).map(|d| Collection {
        pid: d.product_id(),
        iface: d.interface_number(),
        product: d.product_string().unwrap_or_default().trim().to_string(),
        usage_page: d.usage_page(),
        usage: d.usage(),
        path: d.path().to_owned(),
    }))
}

fn group_collections(collections: impl IntoIterator<Item = Collection>) -> Vec<Endpoint> {
    struct Partial {
        pid: u16,
        iface: i32,
        product: String,
        short: Option<CString>,
        long: Option<CString>,
    }
    let mut groups: Vec<Partial> = Vec::new();
    for d in collections.into_iter().filter(|d| d.usage_page >= 0xFF00) {
        let i = match groups.iter().position(|g| g.pid == d.pid && g.iface == d.iface) {
            Some(i) => i,
            None => {
                groups.push(Partial { pid: d.pid, iface: d.iface, product: d.product, short: None, long: None });
                groups.len() - 1
            }
        };
        match d.usage & 0xFF {
            0x01 => groups[i].short = Some(d.path),
            0x02 => groups[i].long = Some(d.path),
            _ => {}
        }
    }
    groups
        .into_iter()
        .filter_map(|g| Some(Endpoint { pid: g.pid, iface: g.iface, product: g.product, short: g.short, long: g.long? }))
        .collect()
}

/// A HID++ 2.0 request/response channel. `Link` is the real one; tests use a scripted fake.
pub trait Hidpp {
    /// Sends a request to device `index` (1..=6 behind a receiver, 0xFF for a direct device)
    /// and waits for the matching reply; returns its 16 parameter bytes.
    fn request(&self, index: u8, feature_idx: u8, function: u8, params: &[u8]) -> Result<[u8; 16], Error>;

    /// Root feature lookup: maps a feature id (e.g. 0x1001) to its index on the device.
    fn feature_index(&self, index: u8, feature: u16) -> Result<Option<u8>, Error> {
        let r = self.request(index, 0x00, 0x0, &feature.to_be_bytes())?;
        Ok((r[0] != 0).then_some(r[0]))
    }
}

/// The raw HID handle `Link` talks through (a fake in tests).
pub trait Transport {
    fn write(&self, data: &[u8]) -> Result<usize, hidapi::HidError>;
    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, hidapi::HidError>;
}

impl Transport for HidDevice {
    fn write(&self, data: &[u8]) -> Result<usize, hidapi::HidError> {
        HidDevice::write(self, data)
    }
    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, hidapi::HidError> {
        HidDevice::read_timeout(self, buf, timeout_ms)
    }
}

pub struct Link<T: Transport = HidDevice> {
    short: Option<T>,
    long: T,
}

impl Link {
    pub fn open(api: &HidApi, ep: &Endpoint) -> Result<Self, Error> {
        let long = api.open_path(&ep.long)?;
        let short = ep.short.as_ref().and_then(|p| api.open_path(p).ok());
        Ok(Self { short, long })
    }
}

impl<T: Transport> Hidpp for Link<T> {
    fn request(&self, index: u8, feature_idx: u8, function: u8, params: &[u8]) -> Result<[u8; 16], Error> {
        let func_sw = (function << 4) | SW_ID;
        let mut tx = [0u8; LEN_LONG];
        tx[0] = REPORT_LONG;
        tx[1] = index;
        tx[2] = feature_idx;
        tx[3] = func_sw;
        let n = params.len().min(16);
        tx[4..4 + n].copy_from_slice(&params[..n]);
        let mut rx = [0u8; 64];
        // Drop replies still queued from earlier requests, e.g. a device that woke up and
        // answered after we timed out. Root lookups (feature index 0) all look alike and don't
        // echo the feature id, so a stale one would be taken as this request's answer.
        for dev in self.short.iter().chain([&self.long]) {
            for _ in 0..64 {
                if dev.read_timeout(&mut rx, 0)? == 0 {
                    break;
                }
            }
        }
        self.long.write(&tx)?;

        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(short) = &self.short {
                loop {
                    let n = short.read_timeout(&mut rx, 0)?;
                    if n == 0 {
                        break;
                    }
                    if let Some(r) = reply(&rx[..n], index, feature_idx, func_sw) {
                        return r;
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout);
            }
            let slice = remaining.min(Duration::from_millis(10)).as_millis() as i32;
            let n = self.long.read_timeout(&mut rx, slice)?;
            if n > 0 {
                if let Some(r) = reply(&rx[..n], index, feature_idx, func_sw) {
                    return r;
                }
            }
        }
    }
}

/// Matches a report against our pending request.
fn reply(rx: &[u8], index: u8, feature_idx: u8, func_sw: u8) -> Option<Result<[u8; 16], Error>> {
    if rx.len() < 6 || (rx[0] != REPORT_SHORT && rx[0] != REPORT_LONG) || rx[1] != index {
        return None;
    }
    // HID++ 2.0 error (0xFF) or HID++ 1.0 error from the receiver (0x8F).
    if (rx[2] == 0xFF || rx[2] == 0x8F) && rx[3] == feature_idx && rx[4] == func_sw {
        return Some(Err(Error::Protocol(rx[5])));
    }
    if rx[2] == feature_idx && rx[3] == func_sw {
        let mut out = [0u8; 16];
        let m = (rx.len() - 4).min(16);
        out[..m].copy_from_slice(&rx[4..4 + m]);
        return Some(Ok(out));
    }
    None
}

/// Debug helper: prints every Logitech HID collection.
pub fn list(api: &HidApi) {
    for d in api.device_list().filter(|d| d.vendor_id() == LOGITECH_VID) {
        println!(
            "pid={:04X} if={} usage_page={:04X} usage={:04X} product={:?}",
            d.product_id(),
            d.interface_number(),
            d.usage_page(),
            d.usage(),
            d.product_string().unwrap_or("?"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(pid: u16, iface: i32, page: u16, usage: u16, path: &str) -> Collection {
        Collection { pid, iface, product: "G733 Gaming Headset".into(), usage_page: page, usage, path: CString::new(path).unwrap() }
    }

    // G733 dongle as documented by HeadsetControl (usage page 0xFF43, usage 0x0202 for HID++
    // long reports). The exact collection list is not captured from a real device.
    #[test]
    fn g733_long_collection_alone_is_an_endpoint() {
        for pid in [0x0AB5, 0x0AFE, 0x0B1F] {
            let eps = group_collections([col(pid, 3, 0xFF43, 0x0202, "long")]);
            assert_eq!(eps.len(), 1, "pid {pid:04x}");
            assert_eq!((eps[0].pid, eps[0].iface), (pid, 3));
            assert!(eps[0].short.is_none());
        }
    }

    #[test]
    fn short_and_long_collections_are_paired() {
        let eps = group_collections([col(0xC52B, 2, 0xFF00, 0x0001, "short"), col(0xC52B, 2, 0xFF00, 0x0002, "long")]);
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].short.as_deref().map(|p| p.to_str().unwrap()), Some("short"));
        assert_eq!(eps[0].long.to_str().unwrap(), "long");
    }

    #[test]
    fn standard_hid_collections_are_ignored() {
        // Keyboard/consumer pages (< 0xFF00) never count, even with a "long" usage.
        assert!(group_collections([col(0x0AB5, 0, 0x000C, 0x0001, "consumer"), col(0x0AB5, 0, 0x0001, 0x0002, "mouse")]).is_empty());
    }

    #[test]
    fn vendor_collection_without_long_usage_is_dropped() {
        assert!(group_collections([col(0x0AB5, 3, 0xFF43, 0x0001, "short only")]).is_empty());
    }

    /// Scripted handle: queued input reports, plus the reply to send for each write.
    struct FakeHandle {
        input: std::cell::RefCell<std::collections::VecDeque<Vec<u8>>>,
        answer: fn(&[u8]) -> Vec<u8>,
    }

    impl Transport for FakeHandle {
        fn write(&self, data: &[u8]) -> Result<usize, hidapi::HidError> {
            self.input.borrow_mut().push_back((self.answer)(data));
            Ok(data.len())
        }
        fn read_timeout(&self, buf: &mut [u8], _timeout_ms: i32) -> Result<usize, hidapi::HidError> {
            Ok(match self.input.borrow_mut().pop_front() {
                Some(r) => {
                    buf[..r.len()].copy_from_slice(&r);
                    r.len()
                }
                None => 0,
            })
        }
    }

    #[test]
    fn late_reply_to_an_earlier_lookup_is_not_taken_as_the_answer() {
        // A device that just woke up answers the lookup that had already timed out
        // ("0x1004? not supported") after we gave up on it. Root lookup replies don't echo
        // the feature id, so that stale reply looks exactly like an answer to the next one.
        let stale = vec![REPORT_LONG, 1, 0x00, SW_ID, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let long = FakeHandle {
            input: std::cell::RefCell::new([stale].into()),
            // The device has 0x1001 at index 6.
            answer: |tx| {
                let mut r = tx.to_vec();
                r[4..].fill(0);
                r[4] = 6;
                r
            },
        };
        let link = Link { short: None, long };
        assert_eq!(link.feature_index(1, 0x1001).unwrap(), Some(6));
    }

    #[test]
    fn interfaces_and_pids_are_grouped_separately() {
        let eps = group_collections([
            col(0x0AB5, 3, 0xFF43, 0x0202, "a"),
            col(0x0AB5, 4, 0xFF43, 0x0202, "b"),
            col(0x0AFE, 3, 0xFF43, 0x0202, "c"),
        ]);
        assert_eq!(eps.len(), 3);
    }
}
