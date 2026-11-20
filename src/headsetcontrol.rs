//! Optional integration with HeadsetControl (https://github.com/Sapd/HeadsetControl), which
//! supports many headsets (SteelSeries, HyperX, Corsair, Logitech, Audeze…).
//!
//! It runs as a separate program (`headsetcontrol -o json`), so it isn't bundled and its
//! GPL license doesn't apply to this project. It's used only if found next to our exe or
//! on PATH.

use crate::device::{Kind, Observation, State};
use crate::diag;
use serde::Deserialize;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const EXE_NAMES: &[&str] = &["headsetcontrol.exe", "headsetcontrol-windows-x86_64.exe"];
const TIMEOUT: Duration = Duration::from_secs(5);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Deserialize)]
struct Output {
    #[serde(default)]
    version: String,
    #[serde(default)]
    devices: Vec<Device>,
}

#[derive(Deserialize)]
struct Device {
    #[serde(default)]
    device: String,
    #[serde(default)]
    product: String,
    #[serde(default)]
    id_vendor: String,
    #[serde(default)]
    id_product: String,
    battery: Option<Battery>,
}

#[derive(Deserialize)]
struct Battery {
    status: String,
    level: i32,
}

/// Locates the HeadsetControl executable, next to our exe or on PATH.
pub fn find() -> Option<PathBuf> {
    let beside = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let path_dirs = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    let exe = beside
        .into_iter()
        .chain(path_dirs)
        .flat_map(|dir| EXE_NAMES.iter().map(move |n| dir.join(n)))
        .find(|p| p.is_file());
    // Not the path: it usually contains the user name.
    diag::event("headsetcontrol", if exe.is_some() { "found" } else { "not installed (optional)" });
    exe
}

/// Runs HeadsetControl and returns its headsets. `skip_pid` filters out devices another
/// source already reports (e.g. Logitech headsets read over HID++, MCHOSE dongles).
pub fn scan(exe: &Path, skip_pid: impl Fn(u16) -> bool) -> Vec<Observation> {
    let json = match run(exe) {
        Ok(json) => json,
        Err(why) => {
            diag::event("headsetcontrol run", why);
            return Vec::new();
        }
    };
    let output: Output = match serde_json::from_str(&json) {
        Ok(o) => o,
        Err(e) => {
            diag::event("headsetcontrol run", format!("unexpected output: {e}"));
            return Vec::new();
        }
    };
    diag::event("headsetcontrol run", format!("version {}, {} device(s)", output.version, output.devices.len()));
    output
        .devices
        .into_iter()
        .filter_map(|d| {
            let pid = parse_hex(&d.id_product)?;
            let key = format!("hc:{}:{pid:04x}", d.id_vendor.trim_start_matches("0x"));
            if skip_pid(pid) {
                diag::event(&key, "skipped: already read natively");
                return None;
            }
            let Some(battery) = d.battery else {
                diag::event(&key, format!("\"{}\": no battery info", d.device));
                return None;
            };
            diag::event(&key, format!("\"{}\": {}", d.device, battery.status));
            let state = match battery.status.as_str() {
                "BATTERY_AVAILABLE" => State::Battery { percent: battery.level.clamp(0, 100) as u8, charging: false },
                "BATTERY_CHARGING" => State::Battery { percent: battery.level.clamp(0, 100) as u8, charging: true },
                _ => State::Off, // unavailable: headset off
            };
            if matches!(state, State::Battery { .. }) && battery.level < 0 {
                return None;
            }
            let name = if d.device.is_empty() { d.product } else { d.device };
            Some(Observation {
                id: format!("hc:{}:{pid:04x}", d.id_vendor.trim_start_matches("0x")),
                kind: Some(match Kind::from_name(&name) {
                    Kind::Earbuds => Kind::Earbuds,
                    _ => Kind::Headset,
                }),
                name: Some(name),
                state,
            })
        })
        .collect()
}

fn run(exe: &Path) -> Result<String, String> {
    let mut child = Command::new(exe)
        .args(["-o", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("failed to start: {e}"))?;
    let deadline = Instant::now() + TIMEOUT;
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err(format!("no answer within {} s", TIMEOUT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    String::from_utf8(out.stdout).map_err(|_| "output isn't UTF-8".into())
}

fn parse_hex(s: &str) -> Option<u16> {
    u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headsetcontrol_json() {
        let json = r#"{"name":"HeadsetControl","version":"4.3.0","device_count":2,"devices":[
            {"status":"success","device":"SteelSeries Arctis Nova 7","vendor":"SteelSeries","product":"Arctis Nova 7",
             "id_vendor":"0x1038","id_product":"0x2202","battery":{"status":"BATTERY_CHARGING","level":64}},
            {"status":"success","device":"Logitech PRO X","vendor":"Logitech","product":"PRO X",
             "id_vendor":"0x046d","id_product":"0x0aba","battery":{"status":"BATTERY_AVAILABLE","level":90}}]}"#;
        let output: Output = serde_json::from_str(json).unwrap();
        assert_eq!(output.devices.len(), 2);
        assert_eq!(parse_hex(&output.devices[0].id_product), Some(0x2202));
        assert_eq!(output.devices[0].battery.as_ref().unwrap().level, 64);
    }
}
