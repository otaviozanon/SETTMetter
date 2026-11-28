//! Small, shared HID/checksum helpers used by the vendor drivers.
//!
//! These are deliberately data-only: they do not know any protocol. Each driver keeps its own
//! report ids, offsets and acceptance rules; this module only removes repeated checksum and
//! queued-report plumbing.

use crate::probe;
use hidapi::{DeviceInfo, HidApi, HidDevice, HidResult};

/// An opened HID collection plus the few `DeviceInfo` fields drivers use for names and IDs.
pub struct Collection {
    pub dev: HidDevice,
    pub desc: Vec<u8>,
    pub product: String,
    pub page: u16,
    pub usage: u16,
}

/// Opens the first collection matching `vid`/`pid` and `accept`. The predicate receives the
/// report descriptor, so drivers keep their own report-length/report-id rules.
pub fn open(
    api: &HidApi,
    vid: u16,
    pid: u16,
    accept: impl Fn(&DeviceInfo, &[u8]) -> bool,
) -> Option<Collection> {
    for info in api.device_list().filter(|d| d.vendor_id() == vid && d.product_id() == pid) {
        let Ok(dev) = api.open_path(info.path()) else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if !accept(info, &desc) {
            continue;
        }
        return Some(Collection {
            dev,
            desc,
            product: info.product_string().unwrap_or("").to_string(),
            page: info.usage_page(),
            usage: info.usage(),
        });
    }
    None
}

/// All matching collections, opened. Use this when a device exposes several vendor pages and a
/// driver has to try more than one.
pub fn open_all(
    api: &HidApi,
    vid: u16,
    pid: u16,
    accept: impl Fn(&DeviceInfo, &[u8]) -> bool,
) -> Vec<Collection> {
    let mut out = Vec::new();
    for info in api.device_list().filter(|d| d.vendor_id() == vid && d.product_id() == pid) {
        let Ok(dev) = api.open_path(info.path()) else { continue };
        let Some(desc) = probe::descriptor(&dev) else { continue };
        if !accept(info, &desc) {
            continue;
        }
        out.push(Collection {
            dev,
            desc,
            product: info.product_string().unwrap_or("").to_string(),
            page: info.usage_page(),
            usage: info.usage(),
        });
    }
    out
}

/// Drains reports that arrived before the current request. A zero-byte read ends the queue.
pub fn drain(dev: &HidDevice) {
    let mut buf = [0u8; 256];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
}

/// Same as [`drain`], but for collections whose reports are larger than 256 bytes.
pub fn drain_len(dev: &HidDevice, len: usize) {
    let mut buf = vec![0u8; len.max(1)];
    while let Ok(n) = dev.read_timeout(&mut buf, 0) {
        if n == 0 {
            break;
        }
    }
}

/// [`drain`] that propagates I/O errors to drivers written around `HidResult`.
pub fn drain_ok(dev: &HidDevice) -> HidResult<()> {
    let mut buf = [0u8; 256];
    while dev.read_timeout(&mut buf, 0)? > 0 {}
    Ok(())
}

/// Drains into a caller-provided buffer, preserving the `HidResult` for drivers that propagate
/// I/O errors.
pub fn drain_buf(dev: &HidDevice, buf: &mut [u8]) -> HidResult<()> {
    while dev.read_timeout(buf, 5)? > 0 {}
    Ok(())
}

/// The Compx/Kysona frame checksum: the bytes sum to `0x55`.
pub fn checksum55(frame: &[u8]) -> u8 {
    0x55u8.wrapping_sub(sum(frame))
}

/// The Keychron MG6 Pro frame checksum: the bytes sum to `0xA1`.
pub fn checksum_a1(frame: &[u8]) -> u8 {
    0xA1u8.wrapping_sub(sum(frame))
}

/// Byte sum, wrapping at 8 bits.
pub fn sum(frame: &[u8]) -> u8 {
    frame.iter().fold(0u8, |s, b| s.wrapping_add(*b))
}

/// Byte sum of `frame[from..to]`.
pub fn sum_range(frame: &[u8], from: usize, to: usize) -> u8 {
    sum(&frame[from..to])
}

/// CRC-16/XMODEM: poly 0x1021, init 0, no reflection.
pub fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// CRC-16/MODBUS: reflected poly 0xA001, init 0xFFFF.
pub fn crc16_modbus(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &b in data {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xA001 } else { crc >> 1 };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_are_the_documented_complements() {
        let frame = [0x08, 0x04, 0, 0, 0, 0, 0, 0];
        assert_eq!(checksum55(&frame), 0x55u8.wrapping_sub(0x0C));
        assert_eq!(checksum_a1(&frame), 0xA1u8.wrapping_sub(0x0C));
        assert_eq!(sum(&[1, 2, 3]), 6);
        assert_eq!(sum_range(&[9, 1, 2, 3, 9], 1, 4), 6);
    }

    #[test]
    fn crc16_variants_match_known_vectors() {
        assert_eq!(crc16_xmodem(b"123456789"), 0x31C3);
        assert_eq!(crc16_modbus(b"123456789"), 0x4B37);
    }
}
