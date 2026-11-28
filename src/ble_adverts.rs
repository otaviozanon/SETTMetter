//! One shared BLE advertisement watcher and cache, mirroring upstream `ble_adverts.cpp`.
//!
//! A single passive `BluetoothLEAdvertisementWatcher` feeds two parsers: Apple manufacturer data
//! (company 0x004C, see `airpods`) and Google Fast Pair service data (16-bit UUID 0xFE2C, see
//! `fastpair`). It only listens; nothing is ever sent. The watcher lives on its own thread for the
//! lifetime of the process, and the handlers run on a WinRT thread pool thread, so the caches are
//! behind a `Mutex`.
#![allow(dead_code)] // the feed entry points are only called by the watcher thread

use crate::diag;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher, BluetoothLEAdvertisementWatcherStatus,
    BluetoothLEScanningMode,
};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::{DataReader, IBuffer};

/// AirPods rotate their (random) BLE address, so old entries are dropped rather than trusted.
const KEEP: Duration = Duration::from_secs(10 * 60);
/// A Fast Pair broadcast older than this is no longer shown (upstream's lookup window).
const FAST_PAIR_FRESH: Duration = Duration::from_secs(5 * 60);
/// Apple's company id in manufacturer data.
const APPLE_COMPANY_ID: u16 = 0x004C;
/// "Service Data - 16-bit UUID" advertisement section.
const SERVICE_DATA_16: u8 = 0x16;

/// Apple manufacturer data (after company id 0x004C), raw: decoded by `airpods::Advert::parse`.
#[derive(Clone)]
pub struct AppleAdvert {
    pub addr: u64,
    pub rssi: i8,
    pub at: Instant,
    pub data: Vec<u8>,
}

/// Fast Pair service data (after the 0xFE2C UUID), already decoded.
#[derive(Clone, Copy)]
pub struct FastPairAdvert {
    pub addr: u64,
    pub at: Instant,
    pub battery: crate::fastpair::Battery,
}

static APPLE: Mutex<Vec<AppleAdvert>> = Mutex::new(Vec::new());
static FAST_PAIR: Mutex<Vec<FastPairAdvert>> = Mutex::new(Vec::new());
/// 0 = not started, 1 = starting, 2 = running.
static STATE: AtomicU8 = AtomicU8::new(0);

/// Starts the watcher if it isn't running. True only on the call that just started it, so the
/// caller can give the devices a moment (they broadcast about every 1-2 s).
pub fn ensure() -> bool {
    if STATE.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return false;
    }
    std::thread::spawn(watch);
    true
}

fn watch() {
    crate::init_com(); // WinRT activation needs an initialized apartment on this thread
    let Some(watcher) = start() else {
        STATE.store(0, Ordering::Release); // radio off: try again on a later poll
        return;
    };
    STATE.store(2, Ordering::Release);
    loop {
        std::thread::sleep(Duration::from_secs(5));
        // Switching the radio off stops the watcher; start it again once it is back.
        if watcher.Status().is_ok_and(|s| {
            matches!(s, BluetoothLEAdvertisementWatcherStatus::Stopped | BluetoothLEAdvertisementWatcherStatus::Aborted)
        }) {
            let _ = watcher.Start();
        }
    }
}

fn start() -> Option<BluetoothLEAdvertisementWatcher> {
    let watcher = match BluetoothLEAdvertisementWatcher::new() {
        Ok(watcher) => watcher,
        Err(e) => {
            diag::event("ble:adverts", format!("watcher unavailable: {e}"));
            return None;
        }
    };
    if let Err(e) = watcher.SetScanningMode(BluetoothLEScanningMode::Passive) {
        diag::event("ble:adverts", format!("passive scan unavailable: {e}"));
    }
    let handler: TypedEventHandler<BluetoothLEAdvertisementWatcher, BluetoothLEAdvertisementReceivedEventArgs> =
        TypedEventHandler::new(|_watcher, args| {
            if let Some(args) = args {
                received(args);
            }
            Ok(())
        });
    if let Err(e) = watcher.Received(&handler) {
        diag::event("ble:adverts", format!("cannot subscribe to advertisements: {e}"));
        return None;
    }
    if let Err(e) = watcher.Start() {
        diag::event("ble:adverts", format!("scan start failed: {e}"));
        return None;
    }
    diag::event("ble:adverts", "passive advertisement scan started");
    Some(watcher)
}

fn received(args: &BluetoothLEAdvertisementReceivedEventArgs) {
    let Ok(addr) = args.BluetoothAddress() else { return };
    let rssi = args.RawSignalStrengthInDBm().unwrap_or(-127).clamp(-127, 20) as i8;
    let Ok(adv) = args.Advertisement() else { return };

    if let Ok(list) = adv.GetManufacturerDataByCompanyId(APPLE_COMPANY_ID) {
        for i in 0..list.Size().unwrap_or(0) {
            let Ok(data) = list.GetAt(i) else { continue };
            if let Ok(buf) = data.Data() {
                feed_apple(addr, rssi, &bytes(&buf));
            }
        }
    }
    if let Ok(list) = adv.GetSectionsByType(SERVICE_DATA_16) {
        for i in 0..list.Size().unwrap_or(0) {
            let Ok(section) = list.GetAt(i) else { continue };
            let Ok(buf) = section.Data() else { continue };
            let data = bytes(&buf);
            // Service data starts with the 16-bit UUID, little-endian: 2C FE = Fast Pair.
            if data.len() > 2 && data[0] == 0x2C && data[1] == 0xFE {
                feed_fast_pair(addr, &data[2..]);
            }
        }
    }
}

fn bytes(buf: &IBuffer) -> Vec<u8> {
    let mut out = vec![0u8; buf.Length().unwrap_or(0) as usize];
    if let Ok(reader) = DataReader::FromBuffer(buf) {
        let _ = reader.ReadBytes(&mut out);
    }
    out
}

/// Called for manufacturer data of company 0x004C.
pub fn feed_apple(addr: u64, rssi: i8, data: &[u8]) {
    let now = Instant::now();
    let mut list = APPLE.lock().unwrap();
    list.retain(|a| now.saturating_duration_since(a.at) < KEEP);
    match list.iter().position(|a| a.addr == addr) {
        Some(i) => {
            let a = &mut list[i];
            a.rssi = rssi;
            a.at = now;
            a.data = data.to_vec();
        }
        None => list.push(AppleAdvert { addr, rssi, at: now, data: data.to_vec() }),
    }
}

/// Called for service data of 16-bit UUID 0xFE2C (bytes after the UUID).
pub fn feed_fast_pair(addr: u64, data: &[u8]) {
    let Some(battery) = crate::fastpair::parse(data) else { return };
    let now = Instant::now();
    let mut list = FAST_PAIR.lock().unwrap();
    list.retain(|a| now.saturating_duration_since(a.at) < FAST_PAIR_FRESH);
    match list.iter().position(|a| a.addr == addr) {
        Some(i) => {
            let a = &mut list[i];
            a.at = now;
            a.battery = battery;
        }
        None => list.push(FastPairAdvert { addr, at: now, battery }),
    }
}

/// Every Apple advertisement heard in the last 10 minutes; the caller filters by age.
pub fn apple() -> Vec<AppleAdvert> {
    APPLE.lock().unwrap().clone()
}

/// The Fast Pair broadcasts still fresh (5 minutes), one per Bluetooth address.
pub fn fast_pair() -> Vec<FastPairAdvert> {
    let now = Instant::now();
    let mut list = FAST_PAIR.lock().unwrap();
    list.retain(|a| now.saturating_duration_since(a.at) < FAST_PAIR_FRESH);
    list.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_apple_advertisements_per_address() {
        feed_apple(0x112233445566, -50, &[0x07, 0x19]);
        feed_apple(0x112233445566, -60, &[0x07, 0x20]);
        let list = apple();
        let ours: Vec<_> = list.iter().filter(|a| a.addr == 0x112233445566).collect();
        assert_eq!(ours.len(), 1);
        assert_eq!(ours[0].rssi, -60);
        assert_eq!(ours[0].data, [0x07, 0x20]);
    }

    #[test]
    fn drops_unparsable_fast_pair_data() {
        // Discoverable-mode advertisement: only a model id, no version byte.
        feed_fast_pair(0xAABBCCDDEEFF, &[0x00, 0x1A, 0x0B]);
        assert!(fast_pair().iter().all(|a| a.addr != 0xAABBCCDDEEFF));
        // Version 0, header 0x33 (3 components, type 3): left 80, right 75 charging, case unknown.
        feed_fast_pair(0xAABBCCDDEEFF, &[0x00, 0x33, 0x50, 0xCB, 0x7F]);
        let found: Vec<_> = fast_pair().into_iter().filter(|a| a.addr == 0xAABBCCDDEEFF).collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].battery.left, Some(80));
        assert!(found[0].battery.right_charging);
    }
}
