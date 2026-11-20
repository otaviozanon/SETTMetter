//! Separate popup window with a CPU/RAM/per-GPU usage history graph, opened on demand from the
//! right-click menu ("Show usage history graph"). Kept out of the tray tooltip, which stays
//! plain text, and out of `render.rs`'s layered taskbar widget, which is sized to the bar.

use crate::sysmonitor::Snapshot;
use crate::wide;
use std::collections::VecDeque;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// ~5 minutes of samples at the fastest (1 s) refresh rate; older samples just scroll off.
const CAPACITY: usize = 300;
/// Offscreen supersampling factor: GDI's line/text drawing has no antialiasing of its own, so
/// the scene is rendered this many times larger and stretched back down with `HALFTONE`, which
/// gives a cheap but noticeably smoother result.
const SUPERSAMPLE: i32 = 2;

/// One row's worth of plottable percentages, captured regardless of the menu's show/hide
/// sensor checkboxes, so graphing isn't limited by what's currently drawn on the taskbar.
#[derive(Clone, Default)]
struct Point {
    cpu: Option<f64>,
    ram: Option<f64>,
    gpus: Vec<Option<f64>>, // one entry per detected adapter
}

static HISTORY: Mutex<VecDeque<Point>> = Mutex::new(VecDeque::new());
/// Handle of the open history window, stored as a raw `usize` (HWND is `*mut c_void`, which
/// isn't `Send`/`Sync`, so it can't live directly in a `static`). Zero means "no window open".
static WINDOW: AtomicUsize = AtomicUsize::new(0);
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Called once per sysmonitor sample (see `widget::sysmonitor_worker`).
pub fn push(snap: &Snapshot) {
    let gpus = snap.gpus.iter().map(|g| g.pct).collect();
    let point = Point { cpu: snap.cpu_pct, ram: snap.ram_pct, gpus };
    let mut h = HISTORY.lock().unwrap();
    h.push_back(point);
    if h.len() > CAPACITY {
        h.pop_front();
    }
    drop(h);
    let w = WINDOW.load(Ordering::Relaxed) as HWND;
    if !w.is_null() {
        unsafe { InvalidateRect(w, null(), 0) };
    }
}

/// Opens the graph window, or brings the existing one to front.
pub fn toggle_open() {
    unsafe {
        let existing = WINDOW.load(Ordering::Relaxed) as HWND;
        if !existing.is_null() && IsWindow(existing) != 0 {
            SetForegroundWindow(existing);
            return;
        }
        let hinst = GetModuleHandleW(null());
        let class = wide("SETTMetterHistory");
        if !CLASS_REGISTERED.swap(true, Ordering::Relaxed) {
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: null_mut(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc);
        }
        let title = wide("SETTMetter \u{2014} Usage history");
        let w = CreateWindowExW(
            WS_EX_APPWINDOW,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT, CW_USEDEFAULT, 640, 380,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        WINDOW.store(w as usize, Ordering::Relaxed);
        ShowWindow(w, SW_SHOW);
    }
}

/// Packs a human RGB triplet (as in a CSS hex color, e.g. `#00D7FF`) into a `COLORREF`
/// (`0x00bbggrr`), since GDI's byte order is the reverse of the usual web convention.
const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (b as u32) << 16 | (g as u32) << 8 | r as u32
}

/// Cycles through a small, readable palette for however many GPUs are detected.
const GPU_PALETTE: [u32; 4] = [rgb(0x4F, 0x8F, 0xFF), rgb(0xFF, 0xB4, 0x54), rgb(0x7C, 0xE3, 0x8B), rgb(0xD1, 0x8C, 0xFF)];

/// A plotted line: display name, color, and an accessor into a `Point`.
struct Series {
    name: String,
    color: u32,
    get: Box<dyn Fn(&Point) -> Option<f64>>,
}

fn series_list(gpu_count: usize) -> Vec<Series> {
    let mut v: Vec<Series> = vec![
        Series { name: "CPU".into(), color: rgb(0x00, 0xD7, 0xFF), get: Box::new(|p: &Point| p.cpu) },
        Series { name: "RAM".into(), color: rgb(0xFF, 0x4F, 0xA3), get: Box::new(|p: &Point| p.ram) },
    ];
    for i in 0..gpu_count {
        let name = if gpu_count > 1 { format!("GPU {}", i + 1) } else { "GPU".to_string() };
        let color = GPU_PALETTE[i % GPU_PALETTE.len()];
        v.push(Series { name, color, get: Box::new(move |p: &Point| p.gpus.get(i).copied().flatten()) });
    }
    v
}

unsafe fn paint(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let (w, h) = ((rc.right - rc.left).max(1), (rc.bottom - rc.top).max(1));

    // Draw at SUPERSAMPLE x into an offscreen bitmap, then stretch down with HALFTONE.
    let (bw, bh) = (w * SUPERSAMPLE, h * SUPERSAMPLE);
    let buf_dc = CreateCompatibleDC(hdc);
    let buf_bmp = CreateCompatibleBitmap(hdc, bw, bh);
    let old_buf = SelectObject(buf_dc, buf_bmp);

    draw_scene(buf_dc, bw, bh);

    SetStretchBltMode(hdc, HALFTONE as i32);
    SetBrushOrgEx(hdc, 0, 0, null_mut());
    StretchBlt(hdc, 0, 0, w, h, buf_dc, 0, 0, bw, bh, SRCCOPY);

    SelectObject(buf_dc, old_buf);
    DeleteObject(buf_bmp);
    DeleteDC(buf_dc);
    EndPaint(hwnd, &ps);
}

unsafe fn fill_ellipse(dc: HDC, rc: &RECT, color: u32) {
    let brush = CreateSolidBrush(color);
    let pen = CreatePen(PS_SOLID as i32, 1, color);
    let old_brush = SelectObject(dc, brush);
    let old_pen = SelectObject(dc, pen);
    Ellipse(dc, rc.left, rc.top, rc.right, rc.bottom);
    SelectObject(dc, old_pen);
    SelectObject(dc, old_brush);
    DeleteObject(pen);
    DeleteObject(brush);
}

unsafe fn text_font(height: i32) -> HFONT {
    let mut lf: LOGFONTW = std::mem::zeroed();
    lf.lfHeight = -height;
    lf.lfWeight = FW_NORMAL as _;
    lf.lfQuality = ANTIALIASED_QUALITY as _;
    for (dst, src) in lf.lfFaceName.iter_mut().zip("Segoe UI".encode_utf16().take(31)) {
        *dst = src;
    }
    CreateFontIndirectW(&lf)
}

/// Background, legend, gridlines and curves, all drawn at supersampled resolution.
unsafe fn draw_scene(dc: HDC, w: i32, h: i32) {
    let s = SUPERSAMPLE;
    let rc = RECT { left: 0, top: 0, right: w, bottom: h };
    let bg = CreateSolidBrush(rgb(0x16, 0x18, 0x1F));
    FillRect(dc, &rc, bg);
    DeleteObject(bg);
    SetBkMode(dc, TRANSPARENT as i32);

    let history = HISTORY.lock().unwrap().clone();
    let gpu_count = history.iter().map(|p| p.gpus.len()).max().unwrap_or(0);
    let series = series_list(gpu_count);

    let font = text_font(13 * s);
    let old_font = SelectObject(dc, font);

    // Legend row across the top: colored dot + name + live value.
    let legend_h = 26 * s;
    let mut lx = 14 * s;
    for sr in &series {
        let dot = 8 * s;
        let dot_rc = RECT { left: lx, top: legend_h / 2 - dot / 2, right: lx + dot, bottom: legend_h / 2 + dot / 2 };
        fill_ellipse(dc, &dot_rc, sr.color);
        lx += dot + 6 * s;
        let text = match history.back().and_then(|p| (sr.get)(p)) {
            Some(v) => format!("{} {:.0}%", sr.name, v),
            None => format!("{} \u{2014}", sr.name),
        };
        let wtext = wide(&text);
        let wlen = wtext.len() as i32 - 1; // exclude the null terminator wide() appends
        SetTextColor(dc, sr.color);
        let mut size: SIZE = std::mem::zeroed();
        GetTextExtentPoint32W(dc, wtext.as_ptr(), wlen, &mut size);
        TextOutW(dc, lx, legend_h / 2 - size.cy / 2, wtext.as_ptr(), wlen);
        lx += size.cx + 20 * s;
    }

    // Plot area, leaving room for the left axis labels and the legend above.
    let margin_left = 36 * s;
    let plot = RECT { left: margin_left, top: legend_h + 8 * s, right: w - 10 * s, bottom: h - 10 * s };
    let (pw, ph) = ((plot.right - plot.left).max(1), (plot.bottom - plot.top).max(1));

    // Gridlines at 0/25/50/75/100%, with a label on the left.
    let grid_pen = CreatePen(PS_SOLID as i32, s.max(1), rgb(0x2C, 0x30, 0x3B));
    let old_pen = SelectObject(dc, grid_pen);
    SetTextColor(dc, rgb(0x8A, 0x90, 0x9C));
    for step in 0..=4 {
        let pct = step * 25;
        let y = plot.bottom - (pct as f64 / 100.0 * ph as f64) as i32;
        MoveToEx(dc, plot.left, y, null_mut());
        LineTo(dc, plot.right, y);
        let label = format!("{pct}%");
        let wtext = wide(&label);
        let wlen = wtext.len() as i32 - 1;
        let mut size: SIZE = std::mem::zeroed();
        GetTextExtentPoint32W(dc, wtext.as_ptr(), wlen, &mut size);
        TextOutW(dc, plot.left - size.cx - 6 * s, y - size.cy / 2, wtext.as_ptr(), wlen);
    }
    SelectObject(dc, old_pen);
    DeleteObject(grid_pen);

    if history.len() < 2 {
        let msg = wide("Collecting samples\u{2026}");
        SetTextColor(dc, rgb(0x8A, 0x90, 0x9C));
        TextOutW(dc, plot.left + 8 * s, plot.top + 8 * s, msg.as_ptr(), msg.len() as i32 - 1); // -1: exclude null terminator
        SelectObject(dc, old_font);
        DeleteObject(font);
        return;
    }

    let n = history.len();
    let step_x = pw as f64 / (n - 1).max(1) as f64;
    let y_of = |v: f64| plot.bottom - (v.clamp(0.0, 100.0) / 100.0 * ph as f64) as i32;
    let x_of = |i: usize| plot.left + (i as f64 * step_x) as i32;

    for sr in &series {
        let mut pts: Vec<POINT> = Vec::with_capacity(n);
        for (i, p) in history.iter().enumerate() {
            let Some(v) = (sr.get)(p) else { continue };
            pts.push(POINT { x: x_of(i), y: y_of(v) });
        }
        if pts.len() < 2 {
            continue;
        }

        // Soft fill under the curve, composited at low opacity so overlapping series stay readable.
        let mut poly = pts.clone();
        poly.push(POINT { x: poly.last().unwrap().x, y: plot.bottom });
        poly.insert(0, POINT { x: poly[0].x, y: plot.bottom });
        let fill_dc = CreateCompatibleDC(dc);
        let fill_bmp = CreateCompatibleBitmap(dc, w, h);
        let old_fill = SelectObject(fill_dc, fill_bmp);
        let black = CreateSolidBrush(0x000000);
        FillRect(fill_dc, &rc, black);
        DeleteObject(black);
        let area_brush = CreateSolidBrush(sr.color);
        let area_pen = CreatePen(PS_SOLID as i32, 1, sr.color);
        let old_brush = SelectObject(fill_dc, area_brush);
        let old_area_pen = SelectObject(fill_dc, area_pen);
        Polygon(fill_dc, poly.as_ptr(), poly.len() as i32);
        SelectObject(fill_dc, old_brush);
        SelectObject(fill_dc, old_area_pen);
        DeleteObject(area_brush);
        DeleteObject(area_pen);
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 28, AlphaFormat: 0 };
        AlphaBlend(dc, 0, 0, w, h, fill_dc, 0, 0, w, h, blend);
        SelectObject(fill_dc, old_fill);
        DeleteObject(fill_bmp);
        DeleteDC(fill_dc);

        // The line itself, on top of its own fill.
        let pen = CreatePen(PS_SOLID as i32, (2 * s).max(1), sr.color);
        let old = SelectObject(dc, pen);
        Polyline(dc, pts.as_ptr(), pts.len() as i32);
        SelectObject(dc, old);
        DeleteObject(pen);

        // A small dot marking the latest sample.
        let last = *pts.last().unwrap();
        let r = 3 * s;
        let dot_rc = RECT { left: last.x - r, top: last.y - r, right: last.x + r, bottom: last.y + r };
        fill_ellipse(dc, &dot_rc, sr.color);
    }

    SelectObject(dc, old_font);
    DeleteObject(font);
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => paint(hwnd),
        WM_SIZE => {
            InvalidateRect(hwnd, null(), 0);
        }
        WM_ERASEBKGND => return 1,
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lp as *mut MINMAXINFO);
            mmi.ptMinTrackSize = POINT { x: 420, y: 240 };
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
        }
        WM_DESTROY => {
            WINDOW.store(0, Ordering::Relaxed);
        }
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    0
}
