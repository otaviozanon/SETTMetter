//! VGN mice and keyboards (VID 0x3554, 0x391D, 0x320F, 0x258A, 0x3151, 0xA8A4, 0x25AA, 0x1CA6,
//! 0x372E, 0x9981, 0x3837, 0x373B): read-only battery levels.
//!
//! Layouts, product ids, their HID collections and report ids are taken verbatim from VGN's own HUB
//! client (VGN VHUB 4.0.11, whose Electron renderer carries the device classes as plain JavaScript;
//! the same bundle is served at drive.vgnlab.com.cn). Every request is a "Get…" command from the
//! matching class: nothing here changes a mode, a profile, lighting or flash.
//!
//! Notes that matter: WebHID reports do not include the report id while hidapi keeps it, so each
//! decoder strips it explicitly. One vendor id exposes five to seven HID collections and only the
//! vendor page answers, so the table pins page/usage/report id. A sleeping 2.4 GHz keyboard answers
//! 0xFF where a length belongs; that is not 0%. These models report a level over the 2.4 GHz link
//! only, so a device on its cable answers nothing here.
//!
//! The reply voltages the Da family carries are logged, not shown.

use crate::device::{Kind, Observation, State};
use crate::diag;
use crate::probe;
use hidapi::{HidApi, HidDevice};
use std::collections::HashSet;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    /// F1 SE/MOBA, Y2 Ultra/Master/Pro Max, F2 Master.
    Da,
    /// Dragonfly King.
    King,
    /// Dragonfly 3 Turbo, F1 V5 Turbo, 3 MINI Turbo.
    Kc2,
    /// Falcon3 Extreme, F1 V2 Extreme.
    LdMs,
    /// Dragonfly 3 SE/Pro, F1 V2 Pro/SE, Y2 V2 Pro.
    Yongjiaxin,
    /// N87.
    Tjx,
    /// S99, V98Pro/V2, S87.
    Weisheng,
    /// V98Pro V3/V4, V87 V2, N75 V2, V108, VXE V87pro.
    Beiying,
    /// Neon75 / Air / He / Turbo / Extreme, Neon68 Extreme.
    Rongyuan,
}

impl Family {
    fn key(self) -> &'static str {
        match self {
            Family::Da => "da",
            Family::King => "king",
            Family::Kc2 => "kc2",
            Family::LdMs => "ldms",
            Family::Yongjiaxin => "yongjiaxin",
            Family::Tjx => "tjx",
            Family::Weisheng => "weisheng",
            Family::Beiying => "beiying",
            Family::Rongyuan => "rongyuan",
        }
    }

    fn keyboard(self) -> bool {
        matches!(self, Family::Tjx | Family::Weisheng | Family::Beiying | Family::Rongyuan)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    /// sendReport (a plain output report).
    Output,
    /// sendFeatureReport + receiveFeatureReport.
    Feature,
}

struct Entry {
    vid: u16,
    pid: u16,
    family: Family,
    channel: Channel,
    page: u16,
    usage: u16,
    /// 0 when the device does not number its reports.
    report: u8,
    /// This id is the 2.4 GHz dongle rather than the device itself.
    receiver: bool,
}

/// vid, pid, family, channel, usage page, usage, report id, dongle. Exported verbatim from VGN
/// VHUB 4.0.11's deviceList (see docs/vgn.md upstream).
const DEVICES: &[Entry] = &[
    Entry { vid: 0x258A, pid: 0x010C, family: Family::Beiying, channel: Channel::Feature, page: 0xFF00, usage: 1, report: 6, receiver: false }, // VXE V87pro
    Entry { vid: 0x258A, pid: 0x024E, family: Family::Beiying, channel: Channel::Feature, page: 0xFF00, usage: 1, report: 9, receiver: false }, // VGN V98pro V3
    Entry { vid: 0x258A, pid: 0x024F, family: Family::Beiying, channel: Channel::Output, page: 0xFF02, usage: 2, report: 19, receiver: true }, // VGN V98pro V3
    Entry { vid: 0x258A, pid: 0x026A, family: Family::Beiying, channel: Channel::Feature, page: 0xFF00, usage: 1, report: 9, receiver: false }, // VGN V87 V2
    Entry { vid: 0x258A, pid: 0x027B, family: Family::Beiying, channel: Channel::Output, page: 0xFF02, usage: 2, report: 19, receiver: true }, // VGN V87 V2
    Entry { vid: 0x258A, pid: 0x027E, family: Family::Beiying, channel: Channel::Feature, page: 0xFF00, usage: 1, report: 9, receiver: false }, // VGN V98pro V3
    Entry { vid: 0x258A, pid: 0x027F, family: Family::Beiying, channel: Channel::Output, page: 0xFF02, usage: 2, report: 19, receiver: true }, // VGN V98pro V3
    Entry { vid: 0x3151, pid: 0x502D, family: Family::Rongyuan, channel: Channel::Feature, page: 0xFFFF, usage: 2, report: 0, receiver: false }, // VGN Neon75 He
    Entry { vid: 0x3151, pid: 0x502F, family: Family::Rongyuan, channel: Channel::Feature, page: 0xFFFF, usage: 2, report: 0, receiver: false }, // VGN Neon75
    Entry { vid: 0x3151, pid: 0x5054, family: Family::Rongyuan, channel: Channel::Feature, page: 0xFFFF, usage: 2, report: 0, receiver: false }, // VGN Neon75 Extreme Tri-mode Keyboard
    Entry { vid: 0x3151, pid: 0x5055, family: Family::Rongyuan, channel: Channel::Feature, page: 0xFFFF, usage: 2, report: 0, receiver: true }, // VGN Neon75 Extreme 2.4G Dongle
    Entry { vid: 0x3554, pid: 0xF502, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false },
    Entry { vid: 0x3554, pid: 0xF503, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true },
    Entry { vid: 0x3554, pid: 0xF50A, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false },
    Entry { vid: 0x3554, pid: 0xF50B, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true },
    Entry { vid: 0x3554, pid: 0xF511, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // VGN F1 SE
    Entry { vid: 0x3554, pid: 0xFB3D, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // VGN Y2 Ultra
    Entry { vid: 0x3554, pid: 0xFB3E, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // VGN Y2 Ultra
    Entry { vid: 0x3554, pid: 0xFB49, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // VGN USB Receiver
    Entry { vid: 0x3554, pid: 0xFB4A, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // UFO Pro
    Entry { vid: 0x3554, pid: 0xFB4B, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // UFO Ultra
    Entry { vid: 0x3554, pid: 0xFB50, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // VGN F1 SE
    Entry { vid: 0x3554, pid: 0xFB51, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // VGN F1 SE
    Entry { vid: 0x3554, pid: 0xFB52, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // VGN Y2 Master
    Entry { vid: 0x3554, pid: 0xFB53, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // VGN Y2 Master
    Entry { vid: 0x3554, pid: 0xFB54, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // VGN Y2 Pro Max
    Entry { vid: 0x3554, pid: 0xFB55, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // VGN Y2 Pro Max
    Entry { vid: 0x3554, pid: 0xFB56, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // VGN F2 Master
    Entry { vid: 0x3554, pid: 0xFB57, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // VGN F2 Master
    Entry { vid: 0x3554, pid: 0xFB58, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // VGN USB Receiver
    Entry { vid: 0x3554, pid: 0xFB59, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // Dragonfly 3 Master
    Entry { vid: 0x372E, pid: 0x10AC, family: Family::King, channel: Channel::Output, page: 0xFF10, usage: 16, report: 32, receiver: false }, // VGN Dragonfly King
    Entry { vid: 0x372E, pid: 0x10AD, family: Family::King, channel: Channel::Output, page: 0xFF10, usage: 16, report: 32, receiver: true }, // VGN Dragonfly King
    Entry { vid: 0x373B, pid: 0x1028, family: Family::Weisheng, channel: Channel::Output, page: 0xFF1C, usage: 146, report: 4, receiver: false }, // VGN S87
    Entry { vid: 0x373B, pid: 0x1029, family: Family::Weisheng, channel: Channel::Output, page: 0xFF1C, usage: 146, report: 4, receiver: true }, // VGN S87 Dongle
    Entry { vid: 0x3837, pid: 0x4042, family: Family::Yongjiaxin, channel: Channel::Output, page: 0xFF01, usage: 16, report: 0, receiver: false }, // VGN Dragonfly3 SE
    Entry { vid: 0x391D, pid: 0x1001, family: Family::Yongjiaxin, channel: Channel::Output, page: 0xFF01, usage: 16, report: 0, receiver: false }, // VGN Dragonfly3 SE
    Entry { vid: 0x391D, pid: 0x1003, family: Family::Yongjiaxin, channel: Channel::Output, page: 0xFF01, usage: 16, report: 0, receiver: false }, // VGN Dragonfly3 Pro
    Entry { vid: 0x391D, pid: 0x1005, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // Dragonfly 3 Master
    Entry { vid: 0x391D, pid: 0x100A, family: Family::Yongjiaxin, channel: Channel::Output, page: 0xFF01, usage: 16, report: 0, receiver: false }, // Dragonfly Y2 V2 Pro
    Entry { vid: 0x391D, pid: 0x100B, family: Family::Yongjiaxin, channel: Channel::Output, page: 0xFF01, usage: 16, report: 0, receiver: false }, // Dragonfly Y2 V2 Plus
    Entry { vid: 0x391D, pid: 0x1017, family: Family::Yongjiaxin, channel: Channel::Output, page: 0xFF01, usage: 16, report: 0, receiver: false }, // Dragonfly F1 V2 Pro
    Entry { vid: 0x391D, pid: 0x1019, family: Family::Yongjiaxin, channel: Channel::Output, page: 0xFF01, usage: 16, report: 0, receiver: false }, // Dragonfly F1 V2 SE
    Entry { vid: 0x391D, pid: 0x101B, family: Family::LdMs, channel: Channel::Output, page: 0xFFA0, usage: 2, report: 160, receiver: false }, // Dragonfly F1 V2 Extreme
    Entry { vid: 0x391D, pid: 0x101D, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // Dragonfly 3 MasterGT
    Entry { vid: 0x391D, pid: 0x1023, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: false }, // Dragonfly 3 Turbo
    Entry { vid: 0x391D, pid: 0x1024, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: false }, // Dragonfly 3 Turbo+
    Entry { vid: 0x391D, pid: 0x1025, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: false }, // Dragonfly F1 V5 Turbo
    Entry { vid: 0x391D, pid: 0x1026, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: false }, // Dragonfly F1 V5 Turbo+
    Entry { vid: 0x391D, pid: 0x102D, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // Dragonfly F2 MasterGT
    Entry { vid: 0x391D, pid: 0x102E, family: Family::LdMs, channel: Channel::Output, page: 0xFFA0, usage: 2, report: 160, receiver: false }, // Falcon3 Extreme
    Entry { vid: 0x391D, pid: 0x1032, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // Dragonfly F2 MasterGT
    Entry { vid: 0x391D, pid: 0x1034, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // Dragonfly Y2 V2 MasterGT
    Entry { vid: 0x391D, pid: 0x1035, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // Dragonfly F2 Turbo
    Entry { vid: 0x391D, pid: 0x1036, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // Dragonfly F2 Turbo+
    Entry { vid: 0x391D, pid: 0x1037, family: Family::LdMs, channel: Channel::Output, page: 0xFFA0, usage: 2, report: 160, receiver: false }, // Falcon3 Extreme+
    Entry { vid: 0x391D, pid: 0x1038, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // Dragonfly F1 V2 SE+
    Entry { vid: 0x391D, pid: 0x1039, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // Dragonfly 3 MINI SE+
    Entry { vid: 0x391D, pid: 0x103C, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: false }, // Dragonfly 3 MINI Turbo
    Entry { vid: 0x391D, pid: 0x103D, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: false }, // Dragonfly 3 MINI Turbo+
    Entry { vid: 0x391D, pid: 0x1042, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: false }, // Dragonfly F1 V5 Air Turbo
    Entry { vid: 0x391D, pid: 0x1043, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: false }, // Dragonfly F1 V5 Air Turbo+
    Entry { vid: 0x391D, pid: 0x1045, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: false }, // Dragonfly 3 RT+
    Entry { vid: 0x391D, pid: 0x1A01, family: Family::Yongjiaxin, channel: Channel::Output, page: 0xFF01, usage: 16, report: 0, receiver: true }, // VGN Dragonfly3 SE
    Entry { vid: 0x391D, pid: 0x1A03, family: Family::Yongjiaxin, channel: Channel::Output, page: 0xFF01, usage: 16, report: 0, receiver: true }, // VGN Dragonfly3 Pro
    Entry { vid: 0x391D, pid: 0x1A05, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // VGN USB Receiver
    Entry { vid: 0x391D, pid: 0x1A09, family: Family::Yongjiaxin, channel: Channel::Output, page: 0xFF01, usage: 16, report: 0, receiver: true }, // 2.4G Wireless Mouse
    Entry { vid: 0x391D, pid: 0x1A0A, family: Family::Yongjiaxin, channel: Channel::Output, page: 0xFF01, usage: 16, report: 0, receiver: true }, // 2.4G Wireless Mouse
    Entry { vid: 0x391D, pid: 0x1A14, family: Family::Yongjiaxin, channel: Channel::Output, page: 0xFF01, usage: 16, report: 0, receiver: true }, // 2.4G Wireless Mouse
    Entry { vid: 0x391D, pid: 0x1A16, family: Family::Yongjiaxin, channel: Channel::Output, page: 0xFF01, usage: 16, report: 0, receiver: true }, // 2.4G Wireless Mouse
    Entry { vid: 0x391D, pid: 0x1A18, family: Family::LdMs, channel: Channel::Output, page: 0xFFA0, usage: 2, report: 160, receiver: true }, // Dragonfly F1 V2 Extreme 2.4G
    Entry { vid: 0x391D, pid: 0x1A1A, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // Dragonfly 3 MasterGT
    Entry { vid: 0x391D, pid: 0x1A20, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: true }, // Dragonfly 3 Turbo
    Entry { vid: 0x391D, pid: 0x1A21, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: true }, // Dragonfly 3 Turbo+
    Entry { vid: 0x391D, pid: 0x1A22, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: true }, // Dragonfly F1 V5 Turbo 2.4G
    Entry { vid: 0x391D, pid: 0x1A23, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: true }, // Dragonfly F1 V5 Turbo+ 2.4G
    Entry { vid: 0x391D, pid: 0x1A29, family: Family::LdMs, channel: Channel::Output, page: 0xFFA0, usage: 2, report: 160, receiver: true }, // Falcon3 Extreme+ 2.4G
    Entry { vid: 0x391D, pid: 0x1A2A, family: Family::LdMs, channel: Channel::Output, page: 0xFFA0, usage: 2, report: 160, receiver: true }, // Falcon3 Extreme 2.4G
    Entry { vid: 0x391D, pid: 0x1A2F, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // Dragonfly Y2 V2 MasterGT
    Entry { vid: 0x391D, pid: 0x1A30, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // Dragonfly F2 Turbo
    Entry { vid: 0x391D, pid: 0x1A31, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // Dragonfly F2 Turbo+
    Entry { vid: 0x391D, pid: 0x1A32, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: true }, // Dragonfly F1 V2 SE+
    Entry { vid: 0x391D, pid: 0x1A33, family: Family::Da, channel: Channel::Output, page: 0xFF02, usage: 2, report: 8, receiver: false }, // Dragonfly 3 MINI SE+
    Entry { vid: 0x391D, pid: 0x1A35, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: true }, // Dragonfly 3 MINI Turbo
    Entry { vid: 0x391D, pid: 0x1A36, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: true }, // Dragonfly 3 MINI Turbo+
    Entry { vid: 0x391D, pid: 0x1A3B, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: true }, // Dragonfly F1 V5 Air Turbo 2.4G
    Entry { vid: 0x391D, pid: 0x1A3C, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: true }, // Dragonfly F1 V5 Air Turbo+ 2.4G
    Entry { vid: 0x391D, pid: 0x1A3E, family: Family::Kc2, channel: Channel::Output, page: 0xFFC1, usage: 1, report: 0, receiver: true }, // Dragonfly 3 RT+
    Entry { vid: 0x391D, pid: 0x2001, family: Family::Rongyuan, channel: Channel::Feature, page: 0xFFFF, usage: 2, report: 0, receiver: false }, // VGN Neon68 Extreme
    Entry { vid: 0x391D, pid: 0x2003, family: Family::Beiying, channel: Channel::Feature, page: 0xFF00, usage: 1, report: 9, receiver: false }, // VGN N75 V2
    Entry { vid: 0x391D, pid: 0x2004, family: Family::Beiying, channel: Channel::Feature, page: 0xFF00, usage: 1, report: 9, receiver: false }, // VGN N75 V2
    Entry { vid: 0x391D, pid: 0x2005, family: Family::Beiying, channel: Channel::Feature, page: 0xFF00, usage: 1, report: 9, receiver: false }, // VGN V108
    Entry { vid: 0x391D, pid: 0x2006, family: Family::Beiying, channel: Channel::Feature, page: 0xFF00, usage: 1, report: 9, receiver: false }, // VGN V108
    Entry { vid: 0x391D, pid: 0x200D, family: Family::Rongyuan, channel: Channel::Feature, page: 0xFFFF, usage: 2, report: 0, receiver: false }, // VGN Neon75 Air
    Entry { vid: 0x391D, pid: 0x200F, family: Family::Rongyuan, channel: Channel::Feature, page: 0xFFFF, usage: 2, report: 0, receiver: false }, // VGN Neon75 Air
    Entry { vid: 0x391D, pid: 0x2016, family: Family::Rongyuan, channel: Channel::Feature, page: 0xFFFF, usage: 2, report: 0, receiver: false }, // VGN Neon75 Turbo
    Entry { vid: 0x391D, pid: 0x2A01, family: Family::Beiying, channel: Channel::Output, page: 0xFF02, usage: 2, report: 19, receiver: true }, // VGN N75 V2
    Entry { vid: 0x391D, pid: 0x2A02, family: Family::Beiying, channel: Channel::Output, page: 0xFF02, usage: 2, report: 19, receiver: true }, // VGN V108
    Entry { vid: 0x391D, pid: 0x2A05, family: Family::Rongyuan, channel: Channel::Feature, page: 0xFFFF, usage: 2, report: 0, receiver: true }, // VGN Neon75 Air 2.4G
    Entry { vid: 0x391D, pid: 0x2A07, family: Family::Rongyuan, channel: Channel::Feature, page: 0xFFFF, usage: 2, report: 0, receiver: true }, // VGN Neon75 Turbo2.4G
    Entry { vid: 0x9981, pid: 0x6002, family: Family::King, channel: Channel::Output, page: 0xFF10, usage: 16, report: 32, receiver: false }, // VGN Dragonfly King
    Entry { vid: 0x9981, pid: 0x7002, family: Family::King, channel: Channel::Output, page: 0xFF10, usage: 16, report: 32, receiver: true }, // VGN Dragonfly King
    Entry { vid: 0xA8A4, pid: 0x2355, family: Family::Tjx, channel: Channel::Output, page: 0xFF01, usage: 16, report: 240, receiver: false }, // VGN N87
    Entry { vid: 0xA8A5, pid: 0x2255, family: Family::Tjx, channel: Channel::Output, page: 0xFF01, usage: 16, report: 240, receiver: true }, // VGN N87
];

fn find(vid: u16, pid: u16) -> Option<&'static Entry> {
    DEVICES.iter().find(|entry| entry.vid == vid && entry.pid == pid)
}

/// The kc2 mice keep reportId 0 in the client's device list and pick 179/181 at runtime.
fn wire_report(entry: &Entry) -> u8 {
    if entry.family == Family::Kc2 {
        179
    } else {
        entry.report
    }
}

/// What a single reply told us.
struct Reading {
    percent: u8,
    charging: bool,
    /// The link is a cable, so the level is meaningless or the board charges.
    wired: bool,
    millivolts: u16,
}

#[derive(Default)]
pub struct VgnClient {
    /// Computed ids already answered, so a sleeping device stays visible.
    seen: HashSet<String>,
}

impl VgnClient {
    pub fn scan(&mut self, api: &HidApi) -> Vec<Observation> {
        // One attempt per distinct product id; the table holds both the wired and dongle ids.
        let mut products: Vec<(u16, u16)> = Vec::new();
        for info in api.device_list() {
            let pair = (info.vendor_id(), info.product_id());
            if find(pair.0, pair.1).is_some() && !products.contains(&pair) {
                products.push(pair);
            }
        }

        let mut out = Vec::new();
        for (vid, pid) in products {
            let Some(entry) = find(vid, pid) else { continue };
            if let Some(observation) = self.poll(api, entry) {
                out.push(observation);
            }
        }
        for key in &self.seen {
            if !out.iter().any(|o| &o.id == key) {
                out.push(Observation { id: key.clone(), name: None, kind: None, state: State::Unreachable });
            }
        }
        out
    }

    fn poll(&mut self, api: &HidApi, entry: &'static Entry) -> Option<Observation> {
        for info in api.device_list().filter(|d| {
            d.vendor_id() == entry.vid && d.product_id() == entry.pid && d.usage_page() == entry.page && d.usage() == entry.usage
        }) {
            let Ok(dev) = api.open_path(info.path()) else { continue };
            let Some(desc) = probe::descriptor(&dev) else { continue };
            let feature = entry.channel == Channel::Feature;
            if feature {
                if probe::feature_len(&desc, wire_report(entry)).unwrap_or(0) < 9 {
                    continue;
                }
            } else if probe::output_len(&desc, wire_report(entry)).unwrap_or(0) < 8
                || probe::input_len(&desc, wire_report(entry)).unwrap_or(0) < 8
            {
                continue;
            }

            let mut reading = if feature { query_feature(&dev, entry, &desc) } else { query_output(&dev, entry, &desc) };
            if reading.is_none() {
                if entry.family == Family::Da && !feature {
                    // A sleeping Da mouse ignores the first request: probe the online state, then retry.
                    da_online(&dev, entry, &desc);
                    reading = read_reading(&dev, entry, 300);
                } else if entry.family == Family::Rongyuan {
                    reading = rongyuan_fetch(&dev, &desc);
                }
                if reading.is_none() && entry.family != Family::Rongyuan {
                    reading = if feature { query_feature(&dev, entry, &desc) } else { query_output(&dev, entry, &desc) };
                }
            }
            let Some(reading) = reading else { continue };

            let key = format!("vgn:{:04x}:{:04x}:{}", entry.vid, entry.pid, entry.family.key());
            let product = info.product_string().unwrap_or("").trim();
            let name = if product.is_empty() {
                if entry.family.keyboard() { "VGN wireless keyboard" } else { "VGN wireless mouse" }
            } else {
                product
            };
            let kind = if entry.family.keyboard() { Kind::Keyboard } else { Kind::Mouse };
            let mut message = format!("{}% via {}", reading.percent, entry.family.key());
            if reading.millivolts > 0 {
                message.push_str(&format!(", {} mV", reading.millivolts));
            }
            if entry.receiver {
                message.push_str(", 2.4 GHz receiver");
            }
            if reading.wired {
                message.push_str(", cable");
            }
            diag::event(&key, message);
            self.seen.insert(key.clone());
            return Some(Observation {
                id: key,
                name: Some(name.into()),
                kind: Some(kind),
                state: State::Battery { percent: reading.percent, charging: reading.charging },
            });
        }
        None
    }
}

// --- Requests ---------------------------------------------------------------------------------

/// Request body without the report id; the caller prepends it and zero-pads to the report length.
fn request(entry: &Entry) -> Option<Vec<u8>> {
    let rid = wire_report(entry);
    match entry.family {
        Family::Da => Some(da_body(rid, 4)),
        Family::King => {
            // [11][00][len=1][01][00][01][00][command] -- the client always sends a one-byte body.
            let mut body = vec![0u8; 8];
            body[0] = 0x11;
            body[2] = 0x01;
            body[3] = 0x01;
            body[5] = 0x01;
            body[7] = 8; // GetUserMouseConfig (also carries the level)
            Some(body)
        }
        Family::Kc2 => {
            let mut body = vec![0u8; 32];
            body[0] = 6; // GetDeviceInfo
            Some(body)
        }
        Family::LdMs => {
            let mut body = vec![0u8; 32];
            body[0] = 2; // GetDeviceInfo
            body[1] = 37; // battery
            Some(body)
        }
        Family::Yongjiaxin => {
            let mut body = vec![0u8; 64];
            body[0] = 0x55;
            body[1] = 0x30; // GetBattery
            Some(body)
        }
        Family::Tjx => {
            let mut body = vec![0u8; 63];
            body[0] = 0x30; // GetBattery
            body[1] = 0x02; // protocol version marker
            body[3] = 2; // payload length; read requests leave the checksum at byte 2 as zero
            Some(body)
        }
        Family::Weisheng => {
            // The client sends exactly seven bytes here, not the 520 of the feature-report families.
            Some(vec![0, 0, 26, 6, 0, 0, 0]) // 26 = GetBatterLevel
        }
        Family::Beiying if entry.channel == Channel::Feature => {
            // 520-byte feature report: [cmd][0][0][1][0][2][0] then zero padding.
            let mut body = vec![0u8; 520];
            body[0] = 135; // GetBattery on the wired N75V2 / V87Pro
            body[3] = 0x01;
            body[5] = 0x02;
            Some(body)
        }
        Family::Beiying => {
            // 19-byte frame for the 2.4 GHz dongle channel, closed by (reportId + sum) & 0xFF.
            let mut body = vec![0u8; 19];
            body[0] = 0x4A; // battery
            body[1] = 0x01;
            let sum = body[..18].iter().fold(u16::from(rid), |sum, b| sum + u16::from(*b));
            body[18] = sum as u8;
            Some(body)
        }
        Family::Rongyuan => {
            let mut body = vec![0u8; 64];
            body[0] = 130; // GetBatterLevel
            let sum = body[..7].iter().fold(0u8, |sum, b| sum.wrapping_add(*b));
            body[7] = 255u8.wrapping_sub(sum);
            Some(body)
        }
    }
}

/// A 16-byte Da frame ends with 85 - sum(bytes 0..14), minus the report id.
fn da_body(rid: u8, cmd: u8) -> Vec<u8> {
    let mut body = vec![0u8; 16];
    body[0] = cmd; // 3 = DeviceOnLine, 4 = BatteryLevel
    let sum = body[..15].iter().fold(0u8, |sum, b| sum.wrapping_add(*b));
    body[15] = 85u8.wrapping_sub(sum).wrapping_sub(rid);
    body
}

fn with_id(rid: u8, body: &[u8], len: usize) -> Vec<u8> {
    let mut frame = vec![0u8; len.max(body.len() + 1)];
    frame[0] = rid;
    frame[1..1 + body.len()].copy_from_slice(body);
    frame
}

// --- Queries ----------------------------------------------------------------------------------

fn query_output(dev: &HidDevice, entry: &Entry, desc: &[u8]) -> Option<Reading> {
    let rid = wire_report(entry);
    let len = probe::output_len(desc, rid)?;
    let frame = with_id(rid, &request(entry)?, len);
    dev.write(&frame).ok()?;
    read_reading(dev, entry, 1000)
}

/// Reads input reports until one decodes, or the deadline passes. Unrelated frames (motion, key
/// presses) are expected and simply skipped.
fn read_reading(dev: &HidDevice, entry: &Entry, ms: u64) -> Option<Reading> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut buf = [0u8; 1024];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match dev.read_timeout(&mut buf, left.as_millis().clamp(1, 250) as i32) {
            Ok(0) => {}
            Ok(n) => {
                if let Some(reading) = decode(entry, &buf[..n]) {
                    return Some(reading);
                }
            }
            Err(_) => return None,
        }
    }
    None
}

/// Feature-report query: SetFeature carries the request, GetFeature returns the answer.
fn query_feature(dev: &HidDevice, entry: &Entry, desc: &[u8]) -> Option<Reading> {
    let rid = wire_report(entry);
    let len = probe::feature_len(desc, rid)?;
    let frame = with_id(rid, &request(entry)?, len);
    dev.send_feature_report(&frame).ok()?;
    // The keyboard families assemble the answer after the request arrives.
    std::thread::sleep(Duration::from_millis(100));
    for _ in 0..3 {
        let mut reply = vec![0u8; frame.len()];
        reply[0] = rid;
        let Ok(n) = dev.get_feature_report(&mut reply) else { return None };
        if let Some(reading) = decode(entry, &reply[..n]) {
            return Some(reading);
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    None
}

/// The Da online probe, sent before the retry so a slept mouse wakes.
fn da_online(dev: &HidDevice, entry: &Entry, desc: &[u8]) {
    let rid = wire_report(entry);
    let Some(len) = probe::output_len(desc, rid) else { return };
    let _ = dev.write(&with_id(rid, &da_body(rid, 3), len));
}

/// The Neon75 family parks device replies in the receiver until they are fetched with 0xF7 (read
/// the queue status, whose byte 1 is the receiver's own view of the level) and 0xFC (take the
/// device payload). Both are read-only; without them a 2.4 GHz keyboard never answers.
fn rongyuan_fetch(dev: &HidDevice, desc: &[u8]) -> Option<Reading> {
    let len = probe::feature_len(desc, 0)? + 1; // report id 0
    if len < 10 {
        return None;
    }
    let mut req = vec![0u8; len];
    req[1] = 0xF7;
    req[8] = 255u8.wrapping_sub(0xF7);
    dev.send_feature_report(&req).ok()?;
    std::thread::sleep(Duration::from_millis(150));
    let mut reply = vec![0u8; len];
    let n = dev.get_feature_report(&mut reply).ok()?;
    // reply[0] is the report id 0 hidapi keeps; the level follows it.
    let mut out = None;
    if n >= 2 && reply[1] <= 100 {
        out = Some(Reading { percent: reply[1], charging: false, wired: false, millivolts: 0 });
    }
    // Queue the device payload so the following GetFeature(130) can decode it.
    req[1] = 0xFC;
    req[8] = 255u8.wrapping_sub(0xFC);
    let _ = dev.send_feature_report(&req);
    std::thread::sleep(Duration::from_millis(30));
    out
}

// --- Replies ----------------------------------------------------------------------------------

fn plausible(level: u8) -> bool {
    level <= 100
}

/// Report ids a family may answer on (reads and writes use different ids on the kc2 mice).
fn reply_ids(entry: &Entry) -> (u8, u8) {
    match entry.family {
        Family::Kc2 => (179, 181),
        Family::Beiying if entry.channel == Channel::Feature => (6, 9),
        Family::Beiying => (19, 0),
        _ => (wire_report(entry), 0),
    }
}

fn decode(entry: &Entry, reply: &[u8]) -> Option<Reading> {
    let (first, second) = reply_ids(entry);
    pick(entry, reply, first).or_else(|| if second != 0 { pick(entry, reply, second) } else { None })
}

/// Drops the report id when the reply still carries it. A numbered report keeps its id at [0];
/// reports whose id is 0 lose that byte.
fn strip<'a>(reply: &'a [u8], ids: &[u8]) -> Option<&'a [u8]> {
    if reply.len() < 2 {
        return None;
    }
    let skip = ids.iter().any(|id| *id != 0 && reply[0] == *id);
    Some(&reply[usize::from(skip)..])
}

fn pick(entry: &Entry, reply: &[u8], report_id: u8) -> Option<Reading> {
    match entry.family {
        Family::Da => {
            let d = strip(reply, &[report_id])?;
            if d.len() < 9 || d[0] != 4 {
                return None; // not a BatteryLevel reply
            }
            let percent = d[5];
            plausible(percent).then(|| Reading {
                percent,
                charging: d[6] == 1,
                wired: false,
                millivolts: (u16::from(d[7]) << 8) | u16::from(d[8]),
            })
        }
        Family::King => {
            let d = strip(reply, &[report_id])?;
            if d.len() < 17 {
                return None;
            }
            // The client slices the reply at byte 7 and reads the config block from there.
            let d = &d[7..];
            if d[0] != 8 {
                return None; // GetUserMouseConfig
            }
            let percent = d[9];
            plausible(percent).then(|| {
                let wired = d[8] & 0x02 == 0;
                Reading { percent, charging: wired, wired, millivolts: 0 }
            })
        }
        Family::Kc2 => {
            let d = strip(reply, &[report_id])?;
            if d.len() < 20 || d[0] != 6 {
                return None;
            }
            let percent = d[19] & 0x7F;
            plausible(percent).then(|| Reading { percent, charging: false, wired: false, millivolts: 0 })
        }
        Family::LdMs => {
            let d = strip(reply, &[report_id])?;
            // Either the solicited GetDeviceInfo reply (code 2, sub 37) or the unsolicited 0x50 status.
            if d.len() >= 3 && d[0] == 2 && d[1] == 37 {
                let percent = d[2];
                return plausible(percent).then(|| Reading { percent, charging: false, wired: false, millivolts: 0 });
            }
            if d.len() >= 8 && d[0] == 0x50 {
                let percent = d[7];
                return plausible(percent).then(|| {
                    let wired = d[1] == 0 && d[2] == 0;
                    Reading { percent, charging: wired, wired, millivolts: 0 }
                });
            }
            None
        }
        Family::Yongjiaxin => {
            let d = strip(reply, &[report_id])?;
            if d.len() < 10 || d[0] != 0x55 || d[1] != 0x30 {
                return None;
            }
            // The client jumps to byte 8: [0] is the level, [1] is set on a cable link.
            let percent = d[8];
            plausible(percent).then(|| {
                let wired = d[9] == 1;
                Reading { percent, charging: wired, wired, millivolts: 0 }
            })
        }
        Family::Tjx => {
            let d = strip(reply, &[report_id])?;
            if d.len() < 9 || d[0] != 0x30 || d[1] != 0x02 || d[3] < 2 {
                return None; // a zero-length payload is not a battery answer
            }
            let percent = d[7];
            plausible(percent).then(|| Reading { percent, charging: d[8] == 1, wired: false, millivolts: 0 })
        }
        Family::Weisheng => {
            let d = strip(reply, &[report_id])?;
            if d.len() < 9 || d[2] != 26 || d[3] == 0xFF {
                return None; // not the echo, or refused because the board is asleep
            }
            let percent = d[7];
            plausible(percent).then(|| Reading { percent, charging: false, wired: false, millivolts: 0 })
        }
        Family::Beiying if entry.channel == Channel::Feature => {
            let d = strip(reply, &[6, 9])?;
            if d.len() < 9 {
                return None;
            }
            // A feature report has no command echo at [0]: the level is simply at +8.
            let percent = d[8];
            plausible(percent).then(|| Reading { percent, charging: false, wired: false, millivolts: 0 })
        }
        Family::Beiying => {
            let d = strip(reply, &[19])?;
            if d.len() < 5 || d[0] != 0x4A {
                return None; // battery reply marker
            }
            let percent = d[4];
            plausible(percent).then(|| Reading { percent, charging: false, wired: false, millivolts: 0 })
        }
        Family::Rongyuan => {
            if reply.is_empty() {
                return None;
            }
            // GetBatterLevel(130) is normally echoed in front of the level; some SKUs send the bare
            // level, accepted only while it is a percentage.
            if reply.len() >= 2 && reply[0] == 130 && plausible(reply[1]) {
                return Some(Reading { percent: reply[1], charging: false, wired: false, millivolts: 0 });
            }
            if plausible(reply[0]) && reply[0] != 130 {
                return Some(Reading { percent: reply[0], charging: false, wired: false, millivolts: 0 });
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(vid: u16, pid: u16) -> &'static Entry {
        find(vid, pid).unwrap()
    }

    #[test]
    fn table_matches_the_exported_device_list() {
        assert_eq!(DEVICES.len(), 104);
        assert!(find(0x3554, 0xFB3D).is_some()); // Y2 Ultra cable
        assert!(find(0x391D, 0x1A20).is_some()); // Dragonfly 3 Turbo 2.4G
        assert!(find(0x258A, 0x024F).is_some()); // V98pro V3 dongle
        assert!(find(0x3554, 0xF505).is_none()); // claimed by kysona
        // Only the kc2 mice use a different report id on the wire.
        assert_eq!(wire_report(entry(0x391D, 0x1023)), 179);
        assert_eq!(wire_report(entry(0x391D, 0x102D)), 8);
    }

    #[test]
    fn request_checksums_cover_the_payload() {
        // Da: 85 - sum - report id.
        let da = da_body(8, 4);
        assert_eq!(da[15], 0x49);
        // Rongyuan: 255 - sum(bytes 0..6) at [7].
        let body = request(entry(0x391D, 0x2A05)).unwrap();
        assert_eq!(body[7], 0x7D);
        // Beiying output over the 2.4 GHz channel: (reportId + sum) & 0xFF.
        let body = request(entry(0x258A, 0x024F)).unwrap();
        assert_eq!(body[18], 0x4A + 0x01 + 19);
        // King and Tjx carry fixed headers.
        let body = request(entry(0x372E, 0x10AC)).unwrap();
        assert_eq!(&body[..8], &[0x11, 0, 1, 1, 0, 1, 0, 8]);
        let body = request(entry(0xA8A4, 0x2355)).unwrap();
        assert_eq!((body[0], body[1], body[3]), (0x30, 0x02, 2));
    }

    #[test]
    fn da_reply_layout() {
        // 08 (report id) 04 … [5] level, [6] charging, [7..9] millivolts.
        let reply = [0x08, 0x04, 0x00, 0x00, 0x00, 0x00, 0x55, 0x01, 0x13, 0x9C, 0x00];
        let reading = decode(entry(0x3554, 0xFB3D), &reply).unwrap();
        assert_eq!((reading.percent, reading.charging, reading.millivolts), (85, true, 0x139C));
        // A different command echo is not a battery reply.
        let mut other = reply;
        other[1] = 3;
        assert!(decode(entry(0x3554, 0xFB3D), &other).is_none());
    }

    #[test]
    fn king_reply_skips_seven_bytes_and_reads_the_config_block() {
        let mut reply = vec![0u8; 20];
        reply[0] = 0x20; // report id 32
        // After the report id, the client slices the reply at byte 7 and reads the config block.
        reply[8] = 8;
        reply[8 + 8] = 0x02; // bit 1 set: not a cable
        reply[8 + 9] = 77;
        let reading = decode(entry(0x372E, 0x10AC), &reply).unwrap();
        assert_eq!((reading.percent, reading.charging), (77, false));
        // Bit 1 clear means the link is a cable, which charges.
        reply[8 + 8] = 0x00;
        let reading = decode(entry(0x372E, 0x10AC), &reply).unwrap();
        assert_eq!((reading.percent, reading.charging), (77, true));
    }

    #[test]
    fn kc2_keeps_its_runtime_report_id() {
        let mut reply = vec![0u8; 21];
        reply[0] = 179;
        reply[1] = 6;
        reply[20] = 0x80 | 64;
        let reading = decode(entry(0x391D, 0x1023), &reply).unwrap();
        assert_eq!(reading.percent, 64);
        // The same reply on the second id.
        reply[0] = 181;
        assert_eq!(decode(entry(0x391D, 0x1023), &reply).unwrap().percent, 64);
    }

    #[test]
    fn ldms_answers_the_query_and_pushes_status() {
        let solicited = [160, 2, 37, 55];
        let reading = decode(entry(0x391D, 0x101B), &solicited).unwrap();
        assert_eq!((reading.percent, reading.charging), (55, false));
        let mut status = [0u8; 9];
        status[0] = 160;
        status[1] = 0x50;
        status[8] = 88;
        let reading = decode(entry(0x391D, 0x101B), &status).unwrap();
        assert_eq!((reading.percent, reading.charging, reading.wired), (88, true, true));
    }

    #[test]
    fn yongjiaxin_tjx_and_weisheng_offsets() {
        let mut reply = [0u8; 12];
        reply[0] = 0x55;
        reply[1] = 0x30;
        reply[8] = 66;
        reply[9] = 1;
        let reading = decode(entry(0x391D, 0x1001), &reply).unwrap();
        assert_eq!((reading.percent, reading.charging), (66, true));

        let mut reply = [0u8; 10];
        reply[0] = 240; // N87 report id
        reply[1] = 0x30;
        reply[2] = 0x02;
        reply[4] = 2;
        reply[8] = 42;
        let reading = decode(entry(0xA8A4, 0x2355), &reply).unwrap();
        assert_eq!(reading.percent, 42);

        let mut reply = [0u8; 10];
        reply[0] = 4; // S87 report id
        reply[3] = 26;
        reply[4] = 6;
        reply[8] = 63;
        let reading = decode(entry(0x373B, 0x1028), &reply).unwrap();
        assert_eq!(reading.percent, 63);
        // 0xFF where the length belongs: the board is asleep, not 0%.
        reply[4] = 0xFF;
        assert!(decode(entry(0x373B, 0x1028), &reply).is_none());
    }

    #[test]
    fn beiying_feature_and_output_replies() {
        let mut feature = [0u8; 10];
        feature[0] = 9;
        feature[9] = 37;
        let reading = decode(entry(0x391D, 0x2003), &feature).unwrap();
        assert_eq!(reading.percent, 37);
        // The table's wired N75V2 answer comes on report id 6.
        feature[0] = 6;
        assert_eq!(decode(entry(0x258A, 0x010C), &feature).unwrap().percent, 37);

        let mut output = [0u8; 6];
        output[0] = 19;
        output[1] = 0x4A;
        output[2] = 0x01;
        output[5] = 81;
        let reading = decode(entry(0x258A, 0x024F), &output).unwrap();
        assert_eq!(reading.percent, 81);
        output[1] = 0x30;
        assert!(decode(entry(0x258A, 0x024F), &output).is_none());
    }

    #[test]
    fn rongyuan_accepts_the_echo_or_the_bare_level() {
        assert_eq!(decode(entry(0x391D, 0x2A05), &[130, 81]).unwrap().percent, 81);
        assert_eq!(decode(entry(0x391D, 0x2A05), &[81]).unwrap().percent, 81);
        // Nothing usable: no reply, or an out-of-range value.
        assert!(decode(entry(0x391D, 0x2A05), &[]).is_none());
        assert!(decode(entry(0x391D, 0x2A05), &[130]).is_none());
        assert!(decode(entry(0x391D, 0x2A05), &[130, 101]).is_none());
        assert!(decode(entry(0x391D, 0x2A05), &[101]).is_none());
    }

    #[test]
    fn implausible_levels_are_rejected() {
        let mut reply = [0u8; 12];
        reply[0] = 0x55;
        reply[1] = 0x30;
        reply[8] = 101;
        assert!(decode(entry(0x391D, 0x1001), &reply).is_none());
    }
}
