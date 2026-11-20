//! Low-battery toast notifications.
//!
//! Unpackaged apps can show toasts once their AppUserModelID is registered under
//! HKCU\Software\Classes\AppUserModelId\<id> with a DisplayName.

use crate::device::{Reading, Status};
use crate::render::LOW_BATTERY;
use crate::settings;
use std::collections::HashSet;
use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

pub const APP_ID: &str = "SETTMetter";
/// Re-arm an alert once the level is back above this (avoids repeats around the threshold).
const REARM_ABOVE: u8 = LOW_BATTERY + 5;

#[derive(Default)]
pub struct LowBatteryAlerts {
    alerted: HashSet<String>,
}

impl LowBatteryAlerts {
    pub fn check(&mut self, readings: &[Reading]) {
        for r in readings {
            let Status::Battery { percent, charging, stale } = r.status else { continue };
            if charging || percent > REARM_ABOVE {
                self.alerted.remove(&r.id);
            } else if percent <= LOW_BATTERY && !stale && !self.alerted.contains(&r.id) {
                self.alerted.insert(r.id.clone());
                if settings::alerts_enabled() {
                    let body = format!("{} is at {percent}%. Time to charge it.", r.name);
                    if let Err(e) = show("Low battery", &body) {
                        crate::diag::event("notify", format!("toast failed: {e}"));
                    }
                }
            }
        }
    }
}

/// Re-arm a temperature alert once it drops this many \u00b0C below the threshold, same
/// hysteresis idea as `REARM_ABOVE` for battery alerts, so it doesn't re-fire every sample
/// while hovering right at the limit.
const TEMP_REARM_MARGIN: f64 = 5.0;

#[derive(Default)]
pub struct TempAlerts {
    cpu_alerted: bool,
    gpu_alerted: bool,
}

/// Pure hysteresis decision for one sensor's temperature latch, kept free of `settings::` reads
/// and toast side effects so it can be unit-tested directly.
///
/// Returns `(next_alerted_state, should_fire_now)`:
/// - Crossing up through `limit` while not already latched fires once and latches.
/// - Staying at/above `limit` (already latched) doesn't refire.
/// - Dropping back below `limit - rearm_margin` unlatches, so it can fire again next time it
///   crosses `limit` instead of only once ever.
/// - Hovering between `limit - rearm_margin` and `limit` keeps whatever state it already had.
fn temp_alert_step(alerted: bool, temp: Option<f64>, limit: f64, rearm_margin: f64) -> (bool, bool) {
    match temp {
        Some(t) if t >= limit && !alerted => (true, true),
        Some(t) if t < limit - rearm_margin => (false, false),
        _ => (alerted, false),
    }
}

impl TempAlerts {
    /// `gpu_temps` is one reading per detected adapter; alerts if any of them crosses the
    /// threshold (TraySense doesn't have per-adapter alerting to copy, so this is new).
    pub fn check(&mut self, cpu_temp: Option<f64>, gpu_temps: &[Option<f64>]) {
        if !settings::alerts_enabled() {
            return;
        }
        let cpu_limit = settings::cpu_alert_threshold() as f64;
        let (next, fire) = temp_alert_step(self.cpu_alerted, cpu_temp, cpu_limit, TEMP_REARM_MARGIN);
        self.cpu_alerted = next;
        if fire {
            let t = cpu_temp.unwrap_or(cpu_limit);
            let body = format!("CPU is at {t:.0}\u{b0}C (limit {cpu_limit:.0}\u{b0}C).");
            if let Err(e) = show("High CPU temperature", &body) {
                crate::diag::event("notify", format!("toast failed: {e}"));
            }
        }

        let gpu_limit = settings::gpu_alert_threshold() as f64;
        let max_gpu = gpu_temps.iter().filter_map(|t| *t).fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b))));
        let (next, fire) = temp_alert_step(self.gpu_alerted, max_gpu, gpu_limit, TEMP_REARM_MARGIN);
        self.gpu_alerted = next;
        if fire {
            let t = max_gpu.unwrap_or(gpu_limit);
            let body = format!("GPU is at {t:.0}\u{b0}C (limit {gpu_limit:.0}\u{b0}C).");
            if let Err(e) = show("High GPU temperature", &body) {
                crate::diag::event("notify", format!("toast failed: {e}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_once_on_crossing_up() {
        let (alerted, fire) = temp_alert_step(false, Some(90.0), 85.0, 5.0);
        assert!(alerted);
        assert!(fire);
    }

    #[test]
    fn does_not_refire_while_still_above_limit() {
        let (alerted, fire) = temp_alert_step(true, Some(92.0), 85.0, 5.0);
        assert!(alerted);
        assert!(!fire);
    }

    #[test]
    fn stays_latched_inside_rearm_margin() {
        // 82 is below the 85 limit but above 85 - 5 = 80, so it shouldn't unlatch yet.
        let (alerted, fire) = temp_alert_step(true, Some(82.0), 85.0, 5.0);
        assert!(alerted);
        assert!(!fire);
    }

    #[test]
    fn unlatches_below_rearm_margin_and_can_refire() {
        let (alerted, _) = temp_alert_step(true, Some(79.0), 85.0, 5.0);
        assert!(!alerted);
        let (alerted, fire) = temp_alert_step(alerted, Some(86.0), 85.0, 5.0);
        assert!(alerted);
        assert!(fire);
    }

    #[test]
    fn missing_reading_keeps_current_state() {
        let (alerted, fire) = temp_alert_step(true, None, 85.0, 5.0);
        assert!(alerted);
        assert!(!fire);
        let (alerted, fire) = temp_alert_step(false, None, 85.0, 5.0);
        assert!(!alerted);
        assert!(!fire);
    }
}

pub fn show(title: &str, body: &str) -> windows::core::Result<()> {
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        escape(title),
        escape(body)
    );
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
