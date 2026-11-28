#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// The Win32 layer is written as `unsafe extern "system"` procs whose bodies call other unsafe
// functions; edition 2024 warns on every one of those calls.
#![allow(unsafe_op_in_unsafe_fn)]
// The ported vendor drivers intentionally keep the upstream C++ shape: nested protocol checks,
// fixed report arrays and one file per protocol. These are style-only lints, not correctness.
#![allow(
    clippy::bool_assert_comparison,
    clippy::chunks_exact_to_as_chunks,
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::derivable_impls,
    clippy::doc_lazy_continuation,
    clippy::empty_line_after_doc_comments,
    clippy::enum_variant_names,
    clippy::get_first,
    clippy::identity_op,
    clippy::if_same_then_else,
    clippy::manual_clamp,
    clippy::manual_range_patterns,
    clippy::manual_c_str_literals,
    clippy::match_like_matches_macro,
    clippy::needless_as_bytes,
    clippy::needless_question_mark,
    clippy::needless_return,
    clippy::no_effect,
    clippy::question_mark,
    clippy::redundant_closure,
    clippy::redundant_pattern_matching,
    clippy::redundant_static_lifetimes,
    clippy::single_match,
    clippy::unnecessary_lazy_evaluations,
    clippy::unnecessary_unwrap,
    clippy::unusual_byte_groupings,
    clippy::useless_vec
)]

mod airpods;
mod airoha;
mod akko;
mod alienware;
mod arbiter;
mod armorx;
mod areson;
mod astro;
mod asus;
mod asus_rog;
mod atk_bitmouse;
mod atk_mercury;
mod atk_serial;
mod audio;
mod aula_headset;
mod aula_keyboard;
mod aula_mouse;
mod barracuda_pro;
mod ble_adverts;
mod bluetooth;
mod cherry;
mod chimera;
mod coolermaster;
mod corsair;
mod corsair_compx;
mod corsair_slipstream;
mod crociris;
mod dareu;
mod darkrim;
mod darmoshark;
mod delux;
mod device;
mod diag;
mod eightbitdo;
mod endgame;
mod evision;
mod eweadn;
mod fastpair;
mod finalmouse;
mod flyout;
mod fractal;
mod galaxy_buds;
mod gamepads;
mod gamesir_ble;
mod gamesir_sapjoy;
mod glorious;
mod headsetcontrol;
mod hid_battery;
mod hidutil;
mod history;
mod hotkey;
mod hyperx;
mod jabra;
mod jbl;
mod jbl_speaker;
mod keychron;
mod kysona;
mod lamzu;
mod logitech;
mod logitech_g435;
mod logitech_g435_jieli;
mod logitech_g735;
mod machenike;
mod mchose;
mod mchose_extra;
mod mchose_keyboard;
mod mchose_mouse;
mod maono;
mod marshall;
mod misc_mice;
mod misc_mice_extra;
mod moondrop;
mod msi;
mod niceboy;
mod ninjutso;
mod nothing_ear;
mod notify;
mod nuphy;
mod nuphy_halo96;
mod nzxt;
mod phonelink;
mod piao;
mod plantronics;
mod playstation;
mod playstation_link;
mod pressplay;
mod probe;
mod pulsar;
mod raigor;
mod rapoo;
mod rawm;
mod razer;
mod razer_headset;
mod render;
mod roccat;
mod royal_kludge;
mod royuan;
mod scyrox;
mod semico;
mod settings;
mod sinowealth;
mod sonix;
mod sony_headphones;
mod sony_inzone;
mod sony_inzone_serial;
mod soundcore;
mod sprime;
mod steelseries;
mod steelseries_elite;
mod steelseries_headset;
mod steam_controller;
mod switch_pro;
mod sysmonitor;
mod turtlebeach;
mod ui;
mod update;
mod vaxee;
mod vgn;
mod vgn_client;
mod via_dongle;
mod widget;
mod wobkey;
mod xbox;
mod zebronics;
mod zenith;
mod zowie;

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
            let mut obs = logitech_g435::LogitechG435::default().scan(&api);
            obs.extend(logitech_g435_jieli::G435Jieli::default().scan(&api));
            obs.extend(logitech_g735::LogitechG735::default().scan(&api));
            obs.extend(logitech::Logitech::new().scan(&api));
            obs.extend(playstation::scan(&api));
            let mut delux = delux::Delux::default();
            let _ = delux.scan(&api); // opens the dongle; its status report comes every ~2 s
            let _ = xbox::scan(); // first call only starts controller enumeration
            std::thread::sleep(std::time::Duration::from_millis(2500));
            obs.extend(delux.scan(&api));
            obs.extend(areson::Areson::default().scan(&api));
            obs.extend(asus::Asus::default().scan(&api));
            obs.extend(asus_rog::AsusRog::default().scan(&api));
            obs.extend(arbiter::Arbiter::default().scan(&api));
            obs.extend(lamzu::Lamzu::default().scan(&api));
            obs.extend(endgame::Endgame::default().scan(&api));
            obs.extend(vaxee::Vaxee::default().scan(&api));
            obs.extend(scyrox::Scyrox::default().scan(&api));
            obs.extend(ninjutso::Ninjutso.scan(&api));
            obs.extend(sprime::Sprime.scan(&api));
            obs.extend(finalmouse::Finalmouse::default().scan(&api));
            obs.extend(pulsar::Pulsar::default().scan(&api));
            obs.extend(atk_bitmouse::AtkBitmouse::default().scan(&api));
            obs.extend(msi::Msi::default().scan(&api));
            obs.extend(misc_mice::MiscMice::default().scan(&api));
            obs.extend(misc_mice_extra::MiscMiceExtra::default().scan(&api));
            obs.extend(misc_mice_extra::FantechExtra::default().scan(&api));
            obs.extend(armorx::Armorx::default().scan(&api));
            obs.extend(atk_serial::AtkSerial.scan(&api));
            obs.extend(nuphy_halo96::NuphyHalo96::default().scan(&api));
            obs.extend(coolermaster::CoolerMaster::default().scan(&api));
            obs.extend(zowie::Zowie::default().scan(&api));
            obs.extend(evision::Evision::default().scan(&api));
            obs.extend(eweadn::Eweadn::default().scan(&api));
            obs.extend(royal_kludge::RoyalKludge::default().scan(&api));
            obs.extend(sonix::Sonix.scan(&api));
            obs.extend(royuan::Royuan::default().scan(&api));
            obs.extend(dareu::Dareu.scan(&api));
            obs.extend(keychron::Keychron.scan(&api));
            obs.extend(cherry::Cherry.scan(&api));
            obs.extend(wobkey::Wobkey::default().scan(&api));
            obs.extend(nuphy::Nuphy::default().scan(&api));
            obs.extend(via_dongle::ViaDongle::default().scan(&api));
            obs.extend(glorious::Glorious::default().scan(&api));
            obs.extend(akko::Akko::default().scan(&api));
            obs.extend(chimera::Chimera::default().scan(&api));
            obs.extend(darmoshark::Darmoshark::default().scan(&api));
            obs.extend(fractal::Fractal::default().scan(&api));
            obs.extend(gamepads::Gamepads::default().scan(&api));
            obs.extend(gamesir_sapjoy::GameSirSapjoy::default().scan(&api));
            obs.extend(gamesir_ble::GameSirBle::default().scan(&api));
            obs.extend(eightbitdo::EightBitDo::default().scan(&api));
            obs.extend(rapoo::Rapoo::default().scan(&api));
            obs.extend(vgn_client::VgnClient::default().scan(&api));
            obs.extend(jabra::Jabra.scan(&api));
            obs.extend(jbl::Jbl.scan(&api));
            obs.extend(moondrop::Moondrop.scan(&api));
            obs.extend(machenike::Machenike::default().scan(&api));
            obs.extend(niceboy::Niceboy::default().scan(&api));
            obs.extend(piao::Piao::default().scan(&api));
            obs.extend(raigor::Raigor::default().scan(&api));
            obs.extend(rawm::Rawm::default().scan(&api));
            obs.extend(semico::Semico::default().scan(&api));
            obs.extend(sinowealth::Sinowealth::default().scan(&api));
            obs.extend(vgn::Vgn::default().scan(&api));
            obs.extend(zebronics::Zebronics::default().scan(&api));
            obs.extend(zenith::Zenith::default().scan(&api));
            obs.extend(kysona::Kysona::default().scan(&api));
            obs.extend(airoha::Airoha::default().scan(&api));
            obs.extend(hyperx::Hyperx::default().scan(&api));
            obs.extend(roccat::Roccat::default().scan(&api));
            obs.extend(astro::Astro::default().scan(&api));
            obs.extend(nzxt::Nzxt::default().scan(&api));
            obs.extend(plantronics::Plantronics::default().scan(&api));
            obs.extend(alienware::Alienware::default().scan(&api));
            obs.extend(maono::Maono::default().scan(&api));
            obs.extend(crociris::Crociris::default().scan(&api));
            obs.extend(darkrim::Darkrim::default().scan(&api));
            obs.extend(aula_headset::AulaHeadset::default().scan(&api));
            obs.extend(aula_keyboard::AulaKeyboard::default().scan(&api));
            obs.extend(aula_mouse::AulaMouse::default().scan(&api));
            obs.extend(pressplay::Pressplay::default().scan(&api));
            obs.extend(turtlebeach::Turtlebeach::default().scan(&api));
            obs.extend(sony_inzone::SonyInzone::default().scan(&api));
            obs.extend(sony_inzone_serial::SonyInzoneSerial::default().scan(&api));
            obs.extend(playstation_link::PlayStationLink::default().scan(&api));
            obs.extend(mchose::Mchose::default().scan(&api));
            obs.extend(mchose_extra::MchoseExtra::default().scan(&api));
            obs.extend(mchose_keyboard::MchoseKeyboard::default().scan(&api));
            obs.extend(atk_mercury::AtkMercury::default().scan(&api));
            obs.extend(mchose_mouse::MchoseMice::default().scan(&api));
            obs.extend(corsair::Corsair::default().scan(&api));
            obs.extend(corsair_compx::CorsairCompx::default().scan(&api));
            obs.extend(corsair_slipstream::CorsairSlipstream::default().scan(&api));
            obs.extend(razer_headset::RazerHeadset::default().scan(&api));
            obs.extend(barracuda_pro::BarracudaPro::default().scan(&api));
            obs.extend(razer::Razer::default().scan(&api));
            obs.extend(steelseries_elite::SteelSeriesElite::default().scan(&api));
            obs.extend(steelseries_headset::SteelSeriesHeadset::default().scan(&api));
            obs.extend(steelseries::SteelSeries::default().scan(&api));
            obs.extend(steam_controller::SteamController::default().scan(&api));
            obs.extend(switch_pro::SwitchPro::default().scan(&api));
            obs.extend(hid_battery::HidBattery::default().scan(&api));
            obs.extend(xbox::scan());
            obs.extend(bluetooth::scan());
            obs.extend(airpods::AirPods.scan());
            obs.extend(galaxy_buds::GalaxyBuds.scan());
            obs.extend(nothing_ear::NothingEar.scan());
            obs.extend(fastpair::FastPair.scan());
            obs.extend(jbl_speaker::JblSpeaker.scan());
            obs.extend(marshall::Marshall.scan());
            obs.extend(phonelink::PhoneLink.scan());
            obs.extend(soundcore::Soundcore.scan());
            obs.extend(sony_headphones::SonyHeadphones.scan());
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
