#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// The Win32 layer is written as `unsafe extern "system"` procs whose bodies call other unsafe
// functions; edition 2024 warns on every one of those calls.
#![allow(unsafe_op_in_unsafe_fn)]

mod areson;
mod asus;
mod audio;
mod bluetooth;
mod corsair;
mod corsair_slipstream;
mod delux;
mod device;
mod diag;
mod flyout;
mod glorious;
mod headsetcontrol;
mod hid_battery;
mod history;
mod hotkey;
mod logitech;
mod mchose;
mod mchose_mouse;
mod notify;
mod playstation;
mod probe;
mod razer;
mod render;
mod settings;
mod steelseries;
mod sysmonitor;
mod ui;
mod update;
mod widget;
mod xbox;

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Initializes COM (multithreaded) on the calling thread, needed for the Bluetooth APIs.
pub(crate) fn init_com() {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
}

fn main() {    match std::env::args().nth(1).as_deref() {
        // Debug helpers (they print only in debug builds; release builds have no console).
        Some("--test-toast") => {
            init_com();
            settings::register_app_id();
            if let Err(e) = notify::show("Low battery", "G Pro Wireless is at 12%. Time to charge it.") {
                eprintln!("toast failed: {e}");
            }
        }
        Some("--probe") => {
            let secs = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(probe::LISTEN.as_secs());
            match probe::run_to_file(std::time::Duration::from_secs(secs)) {
                Ok(path) => println!("{}", std::fs::read_to_string(&path).unwrap_or_default()),
                Err(e) => eprintln!("probe failed: {e}"),
            }
        }
        Some("--list") => logitech::hidpp::list(&hidapi::HidApi::new().expect("hidapi")),
        Some("--once") => {
            init_com();
            diag::start();
            let api = hidapi::HidApi::new().expect("hidapi");
            let mut obs = logitech::Logitech::new().scan(&api);
            obs.extend(playstation::scan(&api));
            let mut delux = delux::Delux::default();
            let _ = delux.scan(&api); // opens the dongle; its status report comes every ~2 s
            let _ = xbox::scan(); // first call only starts controller enumeration
            std::thread::sleep(std::time::Duration::from_millis(2500));
            obs.extend(delux.scan(&api));
            obs.extend(areson::Areson::default().scan(&api));
            obs.extend(asus::Asus::default().scan(&api));
            obs.extend(glorious::Glorious::default().scan(&api));
            obs.extend(mchose::Mchose::default().scan(&api));
            obs.extend(mchose_mouse::MchoseMice::default().scan(&api));
            obs.extend(corsair::Corsair::default().scan(&api));
            obs.extend(corsair_slipstream::CorsairSlipstream::default().scan(&api));
            obs.extend(razer::Razer::default().scan(&api));
            obs.extend(steelseries::SteelSeries::default().scan(&api));
            obs.extend(hid_battery::HidBattery::default().scan(&api));
            obs.extend(xbox::scan());
            obs.extend(bluetooth::scan());
            match headsetcontrol::find() {
                Some(exe) => {
                    eprintln!("[headsetcontrol] using {}", exe.display());
                    let hc = headsetcontrol::scan(&exe, |pid| device::read_natively(&obs, pid));
                    obs.extend(hc);
                }
                None => eprintln!("[headsetcontrol] not found (optional)"),
            }
            for r in device::Tracker::default().update(obs) {
                println!("{:<10} {:<36} {}  [{}]", format!("{:?}", r.kind), r.name, r.status, r.id);
            }
        }
        Some("--demo") if cfg!(debug_assertions) => {
            widget::DEMO.store(true, std::sync::atomic::Ordering::Relaxed);
            widget::run();
        }
        // Relaunch after installing the CPU-temperature driver: wait for the old instance to
        // release its single-instance mutex, then start normally.
        Some("--restart") => {
            if let Some(pid) = std::env::args().nth(2).and_then(|s| s.parse::<u32>().ok()) {
                wait_for_exit(pid);
            }
            widget::run();
        }
        _ => widget::run(),
    }
}

/// Waits up to 10 s for the previous instance to exit, so the relaunch doesn't race the
/// single-instance mutex and quit straight away.
fn wait_for_exit(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
    /// `SYNCHRONIZE` access right, enough to wait on the process handle.
    const SYNCHRONIZE: u32 = 0x0010_0000;
    unsafe {
        let handle = OpenProcess(SYNCHRONIZE, 0, pid);
        if !handle.is_null() {
            WaitForSingleObject(handle, 10_000);
            CloseHandle(handle);
        }
    }
}
