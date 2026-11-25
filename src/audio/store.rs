use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::audio::model::AppState;

static SAVE_LOCK: Mutex<()> = Mutex::new(());

pub fn state_file_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("SETTMetter").join("audio.json")
}

pub fn load_state() -> AppState {
    load_state_from(&state_file_path())
}

pub fn save_state(state: &AppState) -> io::Result<()> {
    save_state_to(&state_file_path(), state)
}

pub fn load_state_from(path: &Path) -> AppState {
    let Ok(bytes) = fs::read(path) else {
        return AppState::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save_state_to(path: &Path, state: &AppState) -> io::Result<()> {
    let _guard = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(state)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    fs::write(&tmp_path, json)?;

    replace_file(&tmp_path, path)
}

#[cfg(windows)]
fn replace_file(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(
            lp_existing_file_name: *const u16,
            lp_new_file_name: *const u16,
            dw_flags: u32,
        ) -> i32;
    }

    let src_wide: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst_wide: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: Both pointers are valid null-terminated UTF-16 strings for the call duration.
    let ok = unsafe {
        MoveFileExW(
            src_wide.as_ptr(),
            dst_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        let err = io::Error::last_os_error();
        let _ = fs::remove_file(src);
        return Err(err);
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(src: &Path, dst: &Path) -> io::Result<()> {
    fs::rename(src, dst)
}
