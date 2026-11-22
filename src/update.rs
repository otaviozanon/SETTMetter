//! Update check and self-update against the GitHub releases page.
//!
//! `curl.exe` (ships with Windows 11) does the HTTPS, so the app itself carries no network
//! stack. The release asset is not signed: the only integrity here is GitHub over TLS, which
//! is why the check is passive and the swap only happens when the user picks the menu item.

use crate::{diag, notify};
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

const API: &str = "https://api.github.com/repos/otaviozanon/SETTMetter/releases/latest";
const ASSET: &str = "SETTMetter.exe";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Guard against an HTML error page or a truncated download being swapped in.
const MIN_EXE: u64 = 300 * 1024;

#[derive(Clone)]
struct Update {
    version: String,
    url: String,
}

static FOUND: Mutex<Option<Update>> = Mutex::new(None);

/// Version of the update a check already found, for the menu label.
pub fn available() -> Option<String> {
    FOUND.lock().unwrap().as_ref().map(|u| u.version.clone())
}

/// Checks the releases page. `manual` is the menu click: it reports "up to date" and failures
/// in a toast, while the silent check at startup only speaks up when there is an update.
pub fn check(manual: bool) {
    match fetch() {
        Ok(Some(u)) => {
            let version = u.version.clone();
            *FOUND.lock().unwrap() = Some(u);
            if let Err(e) = notify::show(
                "SETTMetter update available",
                &format!("Version {version} is ready. Right-click the widget and choose \u{201c}Update to v{version}\u{201d}."),
            ) {
                diag::event("update", format!("toast failed: {e}"));
            }
        }
        Ok(None) if manual => {
            let _ = notify::show("SETTMetter", &format!("You are up to date (v{}).", env!("CARGO_PKG_VERSION")));
        }
        Ok(None) => {}
        Err(e) => {
            diag::event("update", &e);
            if manual {
                let _ = notify::show("Update check failed", &e);
            }
        }
    }
}

fn fetch() -> Result<Option<Update>, String> {
    let json = curl(&["-fsSL", "--max-time", "10", "-H", "Accept: application/vnd.github+json", API])?;
    let release: serde_json::Value = serde_json::from_str(&json).map_err(|e| format!("bad release json: {e}"))?;
    let tag = release.get("tag_name").and_then(|v| v.as_str()).ok_or("release has no tag_name")?;
    let version = tag.trim_start_matches('v').to_string();
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let url = release
        .get("assets")
        .and_then(|v| v.as_array())
        .and_then(|assets| assets.iter().find(|a| a.get("name").and_then(|n| n.as_str()) == Some(ASSET)))
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|v| v.as_str())
        .ok_or("release has no SETTMetter.exe asset")?;
    Ok(Some(Update { version, url: url.to_string() }))
}

/// `1.10.0` > `1.9.9`. Pre-release suffixes count as zero, so they never win over a release.
fn newer(candidate: &str, current: &str) -> bool {
    let parts = |s: &str| {
        let mut out = [0u64; 3];
        for (i, part) in s.split(['.', '-', '+']).take(3).enumerate() {
            out[i] = part.parse().unwrap_or(0);
        }
        out
    };
    parts(candidate) > parts(current)
}

/// Downloads the release next to the running exe and puts it in place. The running image can
/// be renamed (Windows allows it), which is the whole trick: the exe path is the new file by
/// the time the caller relaunches it.
pub fn install() -> Result<(), String> {
    let update = FOUND.lock().unwrap().clone().ok_or("no update available")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("exe has no directory")?.to_path_buf();
    let staged = dir.join("SETTMetter.new.exe");
    let old = dir.join("SETTMetter.old.exe");
    let _ = std::fs::remove_file(&staged);
    let _ = std::fs::remove_file(&old);
    download(&update.url, &staged)?;
    if !is_exe(&staged) {
        let _ = std::fs::remove_file(&staged);
        return Err("downloaded file is not an executable".into());
    }
    std::fs::rename(&exe, &old).map_err(|e| format!("cannot move the running exe: {e}"))?;
    if let Err(e) = std::fs::rename(&staged, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("cannot put the new exe in place: {e}"));
    }
    Ok(())
}

/// Removes the files a previous self-update left behind.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["SETTMetter.old.exe", "SETTMetter.new.exe"] {
                let _ = std::fs::remove_file(dir.join(name));
            }
        }
    }
}

fn download(url: &str, path: &Path) -> Result<(), String> {
    let target = path.to_string_lossy().into_owned();
    curl(&["-fsSL", "--max-time", "120", "-o", &target, url]).map(|_| ())
}

fn curl(args: &[&str]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("curl.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("curl unavailable: {e}"))?;
    if !out.status.success() {
        return Err(format!("curl exited with {}", out.status.code().unwrap_or(-1)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn is_exe(path: &Path) -> bool {
    let mut head = [0u8; 2];
    std::fs::metadata(path).is_ok_and(|m| m.len() > MIN_EXE)
        && std::fs::File::open(path).is_ok_and(|mut f| f.read_exact(&mut head).is_ok())
        && &head == b"MZ"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_order_is_numeric() {
        assert!(newer("1.10.0", "1.9.9"));
        assert!(newer("2.0.0", "1.99.99"));
        assert!(newer("1.2.1", "1.2.0"));
        assert!(!newer("1.2.0", "1.2.0"));
        assert!(!newer("1.2", "1.2.0"));
        assert!(!newer("1.2.0-beta", "1.2.0"));
        assert!(!newer("garbage", "1.0.0"));
    }
}
