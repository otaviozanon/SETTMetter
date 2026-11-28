//! NuPhy Halo96 V2 with battery firmware, on its 2.4 GHz receiver (19F5:3247, interface 1).
//!
//! Custom firmware sends the level as a passive Consumer Control burst: report 03, three bytes,
//! where bytes 1-2 are one usage, in successive reports:
//!
//! ```text
//! 0F          marker, optional "charging" prefix follows
//! 07          optional charging flag (only counted right after 0F)
//! 08..0E      high digit 0..6 -> high nibble 0x00..0x60
//! 10..1F      low digit 0..15
//! 00          release; the level is valid only after the digits
//! ```
//!
//! Frames come about every 15 seconds, so a background reader keeps the last sample for 35 s
//! instead of making the scan wait for the next burst. The stock receiver forwards the reports.
//! Only "NuPhy Halo96 V2 Dongle" is read; other NuPhy models share the receiver vid/pid.
//!
//! Protocol and evidence from Wireless-Device-Battery-Monitor (drivers/nuphy_halo96.cpp,
//! nuphy_protocol.h). Read-only; nothing is written to the receiver.

use crate::device::{Kind, Observation, State};
use crate::diag;
use hidapi::{HidApi, HidDevice};
use std::ffi::CString;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const VID: u16 = 0x19F5;
const PID: u16 = 0x3247;
const PRODUCT: &str = "NuPhy Halo96 V2 Dongle";
const KEY: &str = "nuphy:3247";
const NAME: &str = "NuPhy Halo96 V2";
const TIMEOUT: Duration = Duration::from_millis(250);
const SAMPLE_LIFETIME: Duration = Duration::from_secs(35);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sample {
    percent: u8,
    charging: bool,
}

#[derive(Default)]
pub struct NuphyHalo96 {
    listener: Option<Listener>,
    /// Whether a sample ever arrived; before that a silent receiver isn't listed.
    seen: bool,
}

impl NuphyHalo96 {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        let Some(info) = api.device_list().find(|d| {
            d.vendor_id() == VID
                && d.product_id() == PID
                && d.interface_number() == 1
                && d.usage_page() == 0x000C
                && d.usage() == 0x0001
                && d.product_string() == Some(PRODUCT)
        }) else {
            self.listener = None;
            self.seen = false;
            return Vec::new();
        };
        let path = info.path().to_owned();
        if self.listener.as_ref().is_none_or(|l| l.path != path) {
            self.listener = Listener::spawn(path);
        }
        let Some(listener) = &self.listener else { return Vec::new() };
        let sample = *listener.latest.lock().unwrap_or_else(|e| e.into_inner());
        match sample.filter(|(at, _)| at.elapsed() <= SAMPLE_LIFETIME) {
            Some((_, sample)) => {
                self.seen = true;
                vec![observation(State::Battery { percent: sample.percent, charging: sample.charging })]
            }
            None if self.seen => vec![observation(State::Unreachable)],
            None => Vec::new(),
        }
    }
}

fn observation(state: State) -> Observation {
    Observation { id: KEY.into(), name: Some(NAME.into()), kind: Some(Kind::Keyboard), state }
}

/// Reader thread for one receiver path; stops when the scan drops it.
struct Listener {
    path: CString,
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<(Instant, Sample)>>>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    fn spawn(path: CString) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Mutex::new(None));
        let thread = {
            let (path, stop, latest) = (path.clone(), Arc::clone(&stop), Arc::clone(&latest));
            std::thread::Builder::new()
                .name("nuphy-halo96".into())
                .spawn(move || listen(&path, &stop, &latest))
                .ok()?
        };
        Some(Self { path, stop, latest, thread: Some(thread) })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn listen(path: &CString, stop: &AtomicBool, latest: &Mutex<Option<(Instant, Sample)>>) {
    let api = HidApi::new().ok();
    let mut dev: Option<HidDevice> = None;
    let mut frame = BatteryFrame::default();
    let start = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if dev.is_none() {
            dev = api.as_ref().and_then(|api| api.open_path(path).ok());
            if dev.is_none() {
                std::thread::sleep(TIMEOUT);
                continue;
            }
        }
        let device = dev.as_ref().expect("opened above");
        let started = Instant::now();
        let mut buf = [0u8; 8];
        match device.read_timeout(&mut buf, TIMEOUT.as_millis() as i32) {
            Ok(0) => {}
            Ok(n) => {
                if let Some(sample) = frame.consume(&buf[..n], start.elapsed().as_millis() as u64) {
                    *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), sample));
                }
            }
            Err(e) => {
                // A rapid failure means the handle may be stale after a USB reconnect.
                if started.elapsed() < TIMEOUT / 2 {
                    diag::event(KEY, format!("read failed: {e}"));
                    *latest.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
                dev = None;
                frame = BatteryFrame::default();
                std::thread::sleep(TIMEOUT);
            }
        }
    }
}

/// The burst state machine: start marker, optional charging flag, two digits, release.
#[derive(Default)]
struct BatteryFrame {
    next: Next,
    started: u64,
    percent: u8,
    charging: bool,
}

#[derive(Default, PartialEq)]
enum Next {
    #[default]
    Start,
    ChargingStart,
    High,
    Low,
    Release,
}

impl BatteryFrame {
    fn consume(&mut self, report: &[u8], now: u64) -> Option<Sample> {
        if report.len() < 3 || report[0] != 0x03 {
            return None;
        }
        let usage = u16::from(report[1]) | u16::from(report[2]) << 8;
        if usage == 0x0F {
            self.next = Next::ChargingStart;
            self.started = now;
        } else if usage == 0x07 {
            self.charging = self.next == Next::ChargingStart && now.saturating_sub(self.started) <= 500;
            self.next = Next::High;
            self.started = now;
        } else if now.saturating_sub(self.started) > 500 {
            self.next = Next::Start;
        } else if self.next == Next::High && (0x08..=0x0E).contains(&usage) {
            self.percent = ((usage - 0x08) << 4) as u8;
            self.next = Next::Low;
        } else if self.next == Next::Low && (0x10..=0x1F).contains(&usage) {
            self.percent |= (usage - 0x10) as u8;
            self.next = Next::Release;
        } else {
            let valid = self.next == Next::Release && usage == 0 && self.percent <= 100;
            self.next = Next::Start;
            return valid.then_some(Sample { percent: self.percent, charging: self.charging });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(usage: u16) -> [u8; 3] {
        [0x03, usage as u8, (usage >> 8) as u8]
    }

    /// Marker, charging flag, two digits, release. The flag counts as charging only within
    /// 500 ms of the marker.
    fn burst(charging: bool, high: u16, low: u16) -> Option<Sample> {
        let mut f = BatteryFrame::default();
        let mut now = 1000;
        let _ = f.consume(&frame(0x0F), now);
        now += if charging { 20 } else { 600 };
        let _ = f.consume(&frame(0x07), now);
        now += 20;
        let _ = f.consume(&frame(high), now);
        now += 20;
        let _ = f.consume(&frame(low), now);
        now += 20;
        f.consume(&frame(0x00), now)
    }

    #[test]
    fn decodes_the_charging_burst() {
        // 87 = high 5 (0x0D) and low 7 (0x17).
        let s = burst(true, 0x0D, 0x17).expect("a full burst");
        assert_eq!((s.percent, s.charging), (87, true));
        // 64 = high 4 (0x0C) and low 0 (0x10).
        let s = burst(false, 0x0C, 0x10).expect("a full burst");
        assert_eq!((s.percent, s.charging), (64, false));
    }

    #[test]
    fn release_without_digits_or_over_100_is_ignored() {
        let mut f = BatteryFrame::default();
        assert_eq!(f.consume(&frame(0x0F), 0), None);
        assert_eq!(f.consume(&frame(0x07), 10), None);
        assert_eq!(f.consume(&frame(0x00), 20), None); // no digits yet

        // 111% (0x6F) fails the release check.
        let mut g = BatteryFrame::default();
        assert_eq!(g.consume(&frame(0x0F), 0), None);
        assert_eq!(g.consume(&frame(0x07), 10), None);
        assert_eq!(g.consume(&frame(0x0E), 20), None); // high 6
        assert_eq!(g.consume(&frame(0x1F), 30), None); // low 15
        assert_eq!(g.consume(&frame(0x00), 40), None);
    }

    #[test]
    fn stale_digits_restart_the_frame() {
        let mut f = BatteryFrame::default();
        assert_eq!(f.consume(&frame(0x0F), 0), None);
        assert_eq!(f.consume(&frame(0x07), 10), None);
        // A second of silence drops the sequence; a digit alone is not a level.
        assert_eq!(f.consume(&frame(0x0D), 1500), None);
        assert_eq!(f.consume(&frame(0x17), 1510), None);
        assert_eq!(f.consume(&frame(0x00), 1520), None);
    }

    #[test]
    fn ignores_other_reports() {
        let mut f = BatteryFrame::default();
        assert_eq!(f.consume(&[0x01, 0x0F, 0x00], 0), None);
        assert_eq!(f.consume(&[0x03, 0x0F], 0), None);
    }
}
