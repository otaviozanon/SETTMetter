//! Draws the widget into a per-pixel-alpha bitmap and pushes it with UpdateLayeredWindow,
//! so text blends cleanly over the (translucent) Windows 11 taskbar.

use crate::device::{Kind, Status};
use crate::sysmonitor::Snapshot;
use crate::wide;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{HWND, POINT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

pub const LOW_BATTERY: u8 = 15;
const BOLT: &str = "\u{E945}";
/// Opacity for the last known value of a sleeping device.
const STALE_OPACITY: u32 = 130;
/// Newer glyph (clearer at small sizes); older Segoe Fluent Icons versions only have the pair.
const EARBUD_SINGLE: &str = "\u{F27C}";
const EARBUD_PAIR: &str = "\u{F4C0}";

/// Segoe Fluent Icons glyph per device kind.
fn icon(kind: Kind) -> &'static str {
    match kind {
        Kind::Headset => "\u{E7F6}",
        Kind::Earbuds => EARBUD_SINGLE, // swapped for EARBUD_PAIR if the font lacks it
        Kind::Mouse => "\u{E962}",
        Kind::Keyboard => "\u{E765}",
        Kind::Gamepad => "\u{E7FC}",
        Kind::Other => "\u{E83F}", // battery
    }
}

/// Microphone glyph, appended after the sensors as its own clickable segment.
const MIC_GLYPH: &str = "\u{E720}";
/// Item index that never collides with a device or the sensor group.
const MIC_ITEM: usize = usize::MAX;

/// What a render produced, beyond the bitmap already pushed to the window.
pub struct Rendered {
    pub size: (i32, i32),
    /// Clickable microphone segment, in widget coordinates.
    pub mic: Option<(i32, i32, i32, i32)>,
}

struct Seg {
    text: Vec<u16>,
    icon: bool,
    color: u32, // 0xRRGGBB
    opacity: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    gap_after: i32,
    /// Index of the device this segment belongs to.
    item: usize,
}

/// How the widget is laid out on the taskbar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// Horizontal taskbar of this height: devices in a row.
    Row { height: i32 },
    /// Vertical (left/right) taskbar of this width: devices stacked, icon above the label.
    Column { width: i32 },
}

/// Segoe Fluent Icons glyph per system-monitor group, gated by `icon_mask` (see
/// `sysmonitor::ICON_*`). CPU/RAM/SSD match Microsoft's named glyphs for those concepts
/// (`\uE9D9` CPU, `\uEEA0` RAM, `\uEDA2` HardDrive); GPU/VRAM use the "Component"/"Memory"
/// glyphs since Fluent has no dedicated GPU/VRAM icon.
const ICON_CPU_GLYPH: &str = "\u{E9D9}";
const ICON_GPU_GLYPH: &str = "\u{E950}";
const ICON_RAM_GLYPH: &str = "\u{EEA0}";
const ICON_VRAM_GLYPH: &str = "\u{E964}";
const ICON_DISK_GLYPH: &str = "\u{EDA2}";

/// Renders the widget and returns its size plus the mic click target.
#[allow(clippy::too_many_arguments)]
pub unsafe fn draw(
    hwnd: HWND,
    items: &[(Kind, Status)],
    sys: Option<&Snapshot>,
    icon_mask: u32,
    dpi: u32,
    layout: Layout,
    light: bool,
    mic_muted: Option<bool>,
) -> Rendered {
    let s = |v: i32| v * dpi as i32 / 96;
    let fg = if light { 0x000000 } else { 0xFFFFFF };
    let red = if light { 0xC42B1C } else { 0xFF6B6B };
    let green = if light { 0x0F7B0F } else { 0x6CCB5F };

    let mut segs = Vec::new();
    let seg = |item: usize, text: &str, icon: bool, color: u32, opacity: u32, gap_after: i32| Seg {
        text: wide(text),
        icon,
        color,
        opacity,
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        gap_after,
        item,
    };
    for (i, (kind, status)) in items.iter().enumerate() {
        let gap = if i == items.len() - 1 { 0 } else { s(12) };
        let (label, color, opacity, charging) = match *status {
            Status::Battery { percent, charging, stale } => {
                let color = if percent <= LOW_BATTERY && !charging { red } else { fg };
                (format!("{percent}%"), color, if stale { STALE_OPACITY } else { 255 }, charging)
            }
            Status::NotConnected => ("—".to_string(), fg, 110, false),
        };
        segs.push(seg(i, icon(*kind), true, color, opacity, s(5)));
        if charging {
            segs.push(seg(i, &label, false, color, opacity, s(2)));
            segs.push(seg(i, BOLT, true, green, opacity, gap));
        } else {
            segs.push(seg(i, &label, false, color, opacity, gap));
        }
    }

    // System monitor stats: grouped by sensor (CPU/RAM/GPU/VRAM/SSD), each optionally preceded
    // by its glyph icon (toggle via `icon_mask`), appended after the devices as one pseudo-item.
    let sys_item = items.len();
    if let Some(snap) = sys {
        let hot = |t: Option<f64>| t.is_some_and(|t| t >= 85.0);
        // Only number GPU/VRAM groups (`GPU1`, `GPU2`, ...) when more than one discrete GPU was
        // detected; a single-GPU system keeps the plain "GPU"/"VRAM" labels.
        let multi_gpu = snap.gpus.len() > 1;
        // Temperature parts drop the "CPU"/"GPU" word prefix (just the degree value) since the
        // group's glyph/usage label already identifies which sensor it belongs to.
        // When a group's usage % is hidden, its temperature loses the context the usage label
        // would've given it, so it falls back to a "CPU 30\u00b0C" / "GPU 30\u00b0C" form instead of
        // the bare "30\u00b0C" shown right after a usage percentage.
        let cpu_temp_label = snap.cpu_temp.map(|t| {
            let text = if snap.cpu_pct.is_some() { format!("{t:.0}\u{b0}C") } else { format!("CPU {t:.0}\u{b0}C") };
            (text, if hot(Some(t)) { red } else { fg })
        });
        type Group<'a> = (&'a str, u32, Vec<(String, u32)>);
        let mut groups: Vec<Group> = vec![
            (ICON_CPU_GLYPH, crate::sysmonitor::ICON_CPU, [snap.cpu_pct.map(|p| (format!("CPU {p:.0}%"), fg)), cpu_temp_label].into_iter().flatten().collect()),
            (ICON_RAM_GLYPH, crate::sysmonitor::ICON_RAM, snap.ram_pct.map(|p| (format!("RAM {p:.0}%"), fg)).into_iter().collect()),
        ];
        for (i, g) in snap.gpus.iter().enumerate() {
            let label = if multi_gpu { format!("GPU{}", i + 1) } else { "GPU".to_string() };
            let temp_label = g.temp.map(|t| {
                let text = if g.pct.is_some() { format!("{t:.0}\u{b0}C") } else { format!("{label} {t:.0}\u{b0}C") };
                (text, if hot(Some(t)) { red } else { fg })
            });
            groups.push((ICON_GPU_GLYPH, crate::sysmonitor::ICON_GPU, [g.pct.map(|p| (format!("{label} {p:.0}%"), fg)), temp_label].into_iter().flatten().collect()));
        }
        for (i, g) in snap.gpus.iter().enumerate() {
            let label = if multi_gpu { format!("VRAM{}", i + 1) } else { "VRAM".to_string() };
            groups.push((ICON_VRAM_GLYPH, crate::sysmonitor::ICON_VRAM, g.vram_pct.map(|p| (format!("{label} {p:.0}%"), fg)).into_iter().collect()));
        }
        groups.push((
            ICON_DISK_GLYPH,
            crate::sysmonitor::ICON_DISK,
            snap.disk_temp.map(|t| (format!("SSD {t:.0}\u{b0}C"), if hot(Some(t)) { red } else { fg })).into_iter().collect(),
        ));
        let mut any = false;
        for (icon_glyph, icon_bit, parts) in groups.iter() {
            if parts.is_empty() {
                continue;
            }
            any = true;
            if icon_mask & icon_bit != 0 {
                segs.push(seg(sys_item, icon_glyph, true, fg, 255, s(4)));
            }
            for (i, (label, color)) in parts.iter().enumerate() {
                let gap = if i == parts.len() - 1 { s(10) } else { s(6) };
                segs.push(seg(sys_item, label, false, *color, 255, gap));
            }
        }
        if any {
            if let Some(last) = segs.iter_mut().filter(|s| s.item == sys_item).last() {
                last.gap_after = 0;
            }
            if !items.is_empty()
                && let Some(last_device) = segs.iter_mut().filter(|s| s.item < sys_item).last()
            {
                last_device.gap_after = s(12);
            }
        }
    }

    // Microphone: a mic glyph, red and slashed while every active mic is muted. Last segment, so
    // it sits right next to the clock and gets its own click target (see `mic_rect` below).
    if let Some(muted) = mic_muted {
        if let Some(last) = segs.last_mut() {
            last.gap_after = s(12);
        }
        segs.push(seg(MIC_ITEM, MIC_GLYPH, true, if muted { red } else { fg }, 255, 0));
    }

    let screen = GetDC(null_mut());
    let dc = CreateCompatibleDC(screen);
    ReleaseDC(null_mut(), screen);
    let text_font = font("Segoe UI", -s(12));
    let icon_font = font("Segoe Fluent Icons", -s(15));

    // Fall back to the earbud pair if this font version lacks the single earbud.
    SelectObject(dc, icon_font);
    let single = wide(EARBUD_SINGLE);
    let mut glyph = 0u16;
    if GetGlyphIndicesW(dc, single.as_ptr(), 1, &mut glyph, GGI_MARK_NONEXISTING_GLYPHS) == GDI_ERROR as u32 || glyph == 0xFFFF {
        for seg in segs.iter_mut().filter(|s| s.text == single) {
            seg.text = wide(EARBUD_PAIR);
        }
    }

    // Measure.
    for seg in &mut segs {
        SelectObject(dc, if seg.icon { icon_font } else { text_font });
        let mut size: SIZE = zeroed();
        GetTextExtentPoint32W(dc, seg.text.as_ptr(), seg.text.len() as i32 - 1, &mut size);
        seg.w = size.cx;
        seg.h = size.cy;
    }

    // Layout.
    let pad = s(6);
    let (width, height) = match layout {
        Layout::Row { height } => {
            let mut x = pad;
            for seg in &mut segs {
                seg.x = x;
                seg.y = (height - seg.h) / 2;
                x += seg.w + seg.gap_after;
            }
            (x + pad, height)
        }
        Layout::Column { width } => {
            // Per device: the icon on one line, the label (and charging bolt) centered below it.
            let mut y = pad;
            for i in 0..items.len() {
                let (first, rest) = {
                    let mut it = segs.iter_mut().filter(|s| s.item == i);
                    (it.next(), it.collect::<Vec<_>>())
                };
                if let Some(icon) = first {
                    icon.x = (width - icon.w) / 2;
                    icon.y = y;
                    y += icon.h + s(1);
                }
                let line_w: i32 = rest.iter().map(|s| s.w).sum::<i32>()
                    + rest.iter().rev().skip(1).map(|s| s.gap_after).sum::<i32>();
                let line_h = rest.iter().map(|s| s.h).max().unwrap_or(0);
                let mut x = (width - line_w) / 2;
                for seg in rest {
                    seg.x = x;
                    seg.y = y + (line_h - seg.h) / 2;
                    x += seg.w + seg.gap_after;
                }
                y += line_h;
                if i + 1 < items.len() {
                    y += s(8);
                }
            }
            let sys_segs: Vec<_> = segs.iter_mut().filter(|s| s.item == sys_item).collect();
            if !sys_segs.is_empty() {
                if !items.is_empty() {
                    y += s(8);
                }
                let line_w: i32 = sys_segs.iter().map(|s| s.w).sum::<i32>()
                    + sys_segs.iter().rev().skip(1).map(|s| s.gap_after).sum::<i32>();
                let line_h = sys_segs.iter().map(|s| s.h).max().unwrap_or(0);
                let mut x = (width - line_w) / 2;
                for seg in sys_segs {
                    seg.x = x;
                    seg.y = y + (line_h - seg.h) / 2;
                    x += seg.w + seg.gap_after;
                }
                y += line_h;
            }
            let mic_segs: Vec<_> = segs.iter_mut().filter(|s| s.item == MIC_ITEM).collect();
            if !mic_segs.is_empty() {
                if !items.is_empty() || sys.is_some() {
                    y += s(8);
                }
                let line_w: i32 = mic_segs.iter().map(|s| s.w).sum();
                let line_h = mic_segs.iter().map(|s| s.h).max().unwrap_or(0);
                let mut x = (width - line_w) / 2;
                for seg in mic_segs {
                    seg.x = x;
                    seg.y = y + (line_h - seg.h) / 2;
                    x += seg.w;
                }
                y += line_h;
            }
            (width, y + pad)
        }
    };

    // Click target: the mic segment, stretched to the full taskbar thickness so a near miss
    // still toggles the mic instead of opening the panel.
    let mic_rect = {
        let mut it = segs.iter().filter(|s| s.item == MIC_ITEM);
        it.next().map(|first| {
            let mut box_ = (first.x, first.y, first.x + first.w, first.y + first.h);
            for seg in it {
                box_.0 = box_.0.min(seg.x);
                box_.1 = box_.1.min(seg.y);
                box_.2 = box_.2.max(seg.x + seg.w);
                box_.3 = box_.3.max(seg.y + seg.h);
            }
            let (x0, x1) = (box_.0 - s(4), box_.2 + s(4));
            let (y0, y1) = (box_.1 - s(4), box_.3 + s(4));
            match layout {
                Layout::Row { height } => (x0, 0, x1 - x0, height),
                Layout::Column { width } => (0, y0, width, y1 - y0),
            }
        })
    };

    // White-on-black text into a 32bpp DIB; coverage becomes alpha below.
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height, // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB as _,
        ..zeroed()
    };
    let mut bits: *mut c_void = null_mut();
    let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if bmp.is_null() || bits.is_null() {
        DeleteObject(text_font);
        DeleteObject(icon_font);
        DeleteDC(dc);
        return Rendered { size: (width, height), mic: mic_rect };
    }
    let old_bmp = SelectObject(dc, bmp);
    SetBkMode(dc, TRANSPARENT as _);
    SetTextColor(dc, 0x00FF_FFFF);
    for seg in &segs {
        SelectObject(dc, if seg.icon { icon_font } else { text_font });
        TextOutW(dc, seg.x, seg.y, seg.text.as_ptr(), seg.text.len() as i32 - 1);
    }
    // Muted: slash the glyph itself, so the state reads at a glance without relying on colour.
    if mic_muted == Some(true)
        && let Some(seg) = segs.iter().find(|s| s.item == MIC_ITEM)
    {
        let pen = CreatePen(PS_SOLID, s(2).max(1), 0x00FF_FFFF);
        let old_pen = SelectObject(dc, pen);
        MoveToEx(dc, seg.x + s(1), seg.y + seg.h - s(1), null_mut());
        let _ = LineTo(dc, seg.x + seg.w - s(1), seg.y + s(1));
        SelectObject(dc, old_pen);
        DeleteObject(pen);
    }
    GdiFlush();

    // Per-pixel color/opacity from each segment's box, then convert coverage -> premultiplied BGRA.
    let mut paint = vec![(fg, 255u32); (width * height) as usize];
    for seg in &segs {
        let (x0, x1) = (seg.x.max(0), (seg.x + seg.w).min(width));
        let (y0, y1) = (seg.y.max(0), (seg.y + seg.h).min(height));
        for y in y0..y1 {
            for x in x0..x1 {
                paint[(y * width + x) as usize] = (seg.color, seg.opacity);
            }
        }
    }
    let px = std::slice::from_raw_parts_mut(bits as *mut u32, (width * height) as usize);
    for (i, p) in px.iter_mut().enumerate() {
        let (color, opacity) = paint[i];
        let cov = ((*p >> 16) & 0xFF).max((*p >> 8) & 0xFF).max(*p & 0xFF);
        let a = cov * opacity / 255;
        // Alpha 1 keeps the whole rect clickable while staying invisible.
        *p = if a == 0 {
            0x0100_0000
        } else {
            let ch = |shift: u32| ((color >> shift) & 0xFF) * a / 255;
            (a << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0)
        };
    }

    let size = SIZE { cx: width, cy: height };
    let origin = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    UpdateLayeredWindow(hwnd, null_mut(), null(), &size, dc, &origin, 0, &blend, ULW_ALPHA);

    SelectObject(dc, old_bmp);
    DeleteObject(bmp);
    DeleteObject(text_font);
    DeleteObject(icon_font);
    DeleteDC(dc);
    Rendered {
        size: (width, height),
        mic: mic_rect,
    }
}

unsafe fn font(face: &str, height: i32) -> HFONT {
    let mut lf: LOGFONTW = zeroed();
    lf.lfHeight = height;
    lf.lfWeight = FW_NORMAL as _;
    lf.lfQuality = ANTIALIASED_QUALITY as _; // grayscale AA: coverage maps cleanly to alpha
    for (dst, src) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *dst = src;
    }
    CreateFontIndirectW(&lf)
}
