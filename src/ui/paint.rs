use std::f32::consts::PI;

use winaudio::Win32::Foundation::{RECT, SIZE};
use winaudio::Win32::Graphics::Gdi::{
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH,
    DRAW_TEXT_FORMAT, DT_CALCRECT, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT,
    DT_SINGLELINE, DT_TOP, DT_VCENTER, DT_WORDBREAK, DeleteObject, DrawTextW, FF_SWISS, FW_MEDIUM,
    FW_NORMAL, FW_SEMIBOLD, GetTextExtentPoint32W, HDC, HFONT, HGDIOBJ, OUT_TT_PRECIS,
    SelectObject, SetTextColor,
};
use winaudio::core::PCWSTR;

use crate::ui::theme::Color;
use crate::audio::win32;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RectPx {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl RectPx {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub const fn from_xywh(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        }
    }

    pub const fn contains(self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    pub const fn width(self) -> i32 {
        if self.right > self.left {
            self.right - self.left
        } else {
            0
        }
    }

    pub const fn height(self) -> i32 {
        if self.bottom > self.top {
            self.bottom - self.top
        } else {
            0
        }
    }

    pub fn intersect(self, other: Self) -> Self {
        let left = self.left.max(other.left);
        let top = self.top.max(other.top);
        let right = self.right.min(other.right).max(left);
        let bottom = self.bottom.min(other.bottom).max(top);
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub const fn is_empty(self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    pub const fn to_win32(self) -> RECT {
        RECT {
            left: self.left,
            top: self.top,
            right: self.right,
            bottom: self.bottom,
        }
    }
}

pub fn px(logical: f32, scale: f32) -> i32 {
    (logical * scale).round() as i32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontRole {
    Title14Semi,
    Name13Semi,
    Body12Semi,
    Body11_5Reg,
    Body11_5Med,
    Body11_5Semi,
    Body11Med,
    Body11Semi,
    Sub10_5Reg,
    Sub10_5Med,
    Sub10_5Semi,
    Tiny10Reg,
    Tiny10Italic,
    Tiny10Med,
    Tiny10Semi,
    Micro9_5Med,
}

pub struct FontCache {
    title_14_semi: HFONT,
    name_13_semi: HFONT,
    body_12_semi: HFONT,
    body_11_5_reg: HFONT,
    body_11_5_med: HFONT,
    body_11_5_semi: HFONT,
    body_11_med: HFONT,
    body_11_semi: HFONT,
    sub_10_5_reg: HFONT,
    sub_10_5_med: HFONT,
    sub_10_5_semi: HFONT,
    tiny_10_reg: HFONT,
    tiny_10_italic: HFONT,
    tiny_10_med: HFONT,
    tiny_10_semi: HFONT,
    micro_9_5_med: HFONT,
}

impl FontCache {
    pub fn new(scale: f32) -> Self {
        Self {
            title_14_semi: create_ui_font(14.0, scale, FW_SEMIBOLD.0 as i32, false),
            name_13_semi: create_ui_font(13.0, scale, FW_SEMIBOLD.0 as i32, false),
            body_12_semi: create_ui_font(12.0, scale, FW_SEMIBOLD.0 as i32, false),
            body_11_5_reg: create_ui_font(11.5, scale, FW_NORMAL.0 as i32, false),
            body_11_5_med: create_ui_font(11.5, scale, FW_MEDIUM.0 as i32, false),
            body_11_5_semi: create_ui_font(11.5, scale, FW_SEMIBOLD.0 as i32, false),
            body_11_med: create_ui_font(11.0, scale, FW_MEDIUM.0 as i32, false),
            body_11_semi: create_ui_font(11.0, scale, FW_SEMIBOLD.0 as i32, false),
            sub_10_5_reg: create_ui_font(10.5, scale, FW_NORMAL.0 as i32, false),
            sub_10_5_med: create_ui_font(10.5, scale, FW_MEDIUM.0 as i32, false),
            sub_10_5_semi: create_ui_font(10.5, scale, FW_SEMIBOLD.0 as i32, false),
            tiny_10_reg: create_ui_font(10.0, scale, FW_NORMAL.0 as i32, false),
            tiny_10_italic: create_ui_font(10.0, scale, FW_NORMAL.0 as i32, true),
            tiny_10_med: create_ui_font(10.0, scale, FW_MEDIUM.0 as i32, false),
            tiny_10_semi: create_ui_font(10.0, scale, FW_SEMIBOLD.0 as i32, false),
            micro_9_5_med: create_ui_font(9.5, scale, FW_MEDIUM.0 as i32, false),
        }
    }

    pub fn get(&self, role: FontRole) -> HFONT {
        match role {
            FontRole::Title14Semi => self.title_14_semi,
            FontRole::Name13Semi => self.name_13_semi,
            FontRole::Body12Semi => self.body_12_semi,
            FontRole::Body11_5Reg => self.body_11_5_reg,
            FontRole::Body11_5Med => self.body_11_5_med,
            FontRole::Body11_5Semi => self.body_11_5_semi,
            FontRole::Body11Med => self.body_11_med,
            FontRole::Body11Semi => self.body_11_semi,
            FontRole::Sub10_5Reg => self.sub_10_5_reg,
            FontRole::Sub10_5Med => self.sub_10_5_med,
            FontRole::Sub10_5Semi => self.sub_10_5_semi,
            FontRole::Tiny10Reg => self.tiny_10_reg,
            FontRole::Tiny10Italic => self.tiny_10_italic,
            FontRole::Tiny10Med => self.tiny_10_med,
            FontRole::Tiny10Semi => self.tiny_10_semi,
            FontRole::Micro9_5Med => self.micro_9_5_med,
        }
    }

    fn all(&self) -> [HFONT; 16] {
        [
            self.title_14_semi,
            self.name_13_semi,
            self.body_12_semi,
            self.body_11_5_reg,
            self.body_11_5_med,
            self.body_11_5_semi,
            self.body_11_med,
            self.body_11_semi,
            self.sub_10_5_reg,
            self.sub_10_5_med,
            self.sub_10_5_semi,
            self.tiny_10_reg,
            self.tiny_10_italic,
            self.tiny_10_med,
            self.tiny_10_semi,
            self.micro_9_5_med,
        ]
    }
}

impl Drop for FontCache {
    fn drop(&mut self) {
        for font in self.all() {
            if !font.is_invalid() {
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(font.0));
                }
            }
        }
    }
}

fn create_ui_font(logical_px: f32, scale: f32, weight: i32, italic: bool) -> HFONT {
    create_font("Segoe UI", logical_px, scale, weight, italic)
}

/// Icons come from Segoe Fluent Icons, the same face the taskbar widget uses.
pub fn create_glyph_font(face: &str, logical_px: f32, scale: f32) -> HFONT {
    create_font(face, logical_px, scale, FW_NORMAL.0 as i32, false)
}

fn create_font(face: &str, logical_px: f32, scale: f32, weight: i32, italic: bool) -> HFONT {
    let height = -((logical_px * scale).round() as i32).max(1);
    let face = win32::to_wide(face);
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            u32::from(italic),
            0,
            0,
            DEFAULT_CHARSET,
            OUT_TT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
            PCWSTR(face.as_ptr()),
        )
    }
}

pub fn measure_text_width(hdc: HDC, font: HFONT, text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE::default();
    unsafe {
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);
        SelectObject(hdc, old);
    }
    size.cx.max(0)
}

pub fn draw_text(
    hdc: HDC,
    font: HFONT,
    text: &str,
    rect: RectPx,
    color: Color,
    align: TextAlign,
    ellipsis: bool,
) {
    if rect.is_empty() || text.is_empty() {
        return;
    }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut win_rect = rect.to_win32();
    let align_flag = match align {
        TextAlign::Left => DT_LEFT,
        TextAlign::Center => DT_CENTER,
        TextAlign::Right => DT_RIGHT,
    };
    let mut flags: DRAW_TEXT_FORMAT = align_flag | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
    if ellipsis {
        flags |= DT_END_ELLIPSIS;
    }
    unsafe {
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        SetTextColor(hdc, color.colorref());
        let _ = DrawTextW(hdc, &mut wide, &mut win_rect, flags);
        SelectObject(hdc, old);
    }
}

pub fn measure_wrapped_height(hdc: HDC, font: HFONT, text: &str, max_width: i32) -> i32 {
    if text.is_empty() || max_width <= 0 {
        return 0;
    }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: max_width,
        bottom: 0,
    };
    unsafe {
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        let _ = DrawTextW(
            hdc,
            &mut wide,
            &mut rect,
            DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX | DT_CALCRECT,
        );
        SelectObject(hdc, old);
    }
    (rect.bottom - rect.top).max(0)
}

pub fn draw_wrapped_text(hdc: HDC, font: HFONT, text: &str, rect: RectPx, color: Color) {
    if rect.is_empty() || text.is_empty() {
        return;
    }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut win_rect = rect.to_win32();
    unsafe {
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        SetTextColor(hdc, color.colorref());
        let _ = DrawTextW(
            hdc,
            &mut wide,
            &mut win_rect,
            DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
        );
        SelectObject(hdc, old);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconKind {
    Lock,
    Unlock,
    Pin,
    PinSolid,
    Settings,
    Search,
    CloseX,
    Volume,
    VolumeMuted,
    Mic,
    MicMuted,
    Check,
    ChevronDown,
    Plus,
    Power,
}

pub struct Canvas<'a> {
    pixels: &'a mut [u8],
    width: i32,
    height: i32,
    clip_stack: Vec<RectPx>,
}

impl<'a> Canvas<'a> {
    pub fn new(pixels: &'a mut [u8], width: i32, height: i32) -> Self {
        Self {
            pixels,
            width,
            height,
            clip_stack: vec![RectPx::new(0, 0, width, height)],
        }
    }

    pub fn current_clip(&self) -> RectPx {
        self.clip_stack
            .last()
            .copied()
            .unwrap_or_else(|| RectPx::new(0, 0, self.width, self.height))
    }

    pub fn push_clip(&mut self, rect: RectPx) {
        let clipped = self.current_clip().intersect(rect);
        self.clip_stack.push(clipped);
    }

    pub fn pop_clip(&mut self) {
        if self.clip_stack.len() > 1 {
            self.clip_stack.pop();
        }
    }

    pub fn fill_all(&mut self, color: Color) {
        let (chunks, _) = self.pixels.as_chunks_mut::<4>();
        for chunk in chunks {
            chunk[0] = color.b;
            chunk[1] = color.g;
            chunk[2] = color.r;
            chunk[3] = 255;
        }
    }

    #[inline]
    fn blend_pixel_unchecked(&mut self, x: i32, y: i32, color: Color, coverage: f32) {
        let alpha = coverage * color.alpha_f32();
        if alpha <= 0.0 {
            return;
        }
        let idx = ((y * self.width + x) * 4) as usize;
        if alpha >= 1.0 {
            self.pixels[idx] = color.b;
            self.pixels[idx + 1] = color.g;
            self.pixels[idx + 2] = color.r;
            self.pixels[idx + 3] = 255;
            return;
        }
        let inv = 1.0 - alpha;
        self.pixels[idx] = (f32::from(color.b) * alpha + f32::from(self.pixels[idx]) * inv)
            .round() as u8;
        self.pixels[idx + 1] = (f32::from(color.g) * alpha
            + f32::from(self.pixels[idx + 1]) * inv)
            .round() as u8;
        self.pixels[idx + 2] = (f32::from(color.r) * alpha
            + f32::from(self.pixels[idx + 2]) * inv)
            .round() as u8;
        self.pixels[idx + 3] = 255;
    }

    pub fn fill_rect(&mut self, rect: RectPx, color: Color) {
        let clipped = rect.intersect(self.current_clip());
        if clipped.is_empty() || color.a == 0 {
            return;
        }
        if color.a == 255 {
            for y in clipped.top..clipped.bottom {
                let row_start = (y * self.width) as usize;
                for x in clipped.left..clipped.right {
                    let idx = (row_start + x as usize) * 4;
                    self.pixels[idx] = color.b;
                    self.pixels[idx + 1] = color.g;
                    self.pixels[idx + 2] = color.r;
                    self.pixels[idx + 3] = 255;
                }
            }
        } else {
            for y in clipped.top..clipped.bottom {
                for x in clipped.left..clipped.right {
                    self.blend_pixel_unchecked(x, y, color, 1.0);
                }
            }
        }
    }

    pub fn hline(&mut self, x0: i32, x1: i32, y: i32, thickness: i32, color: Color) {
        self.fill_rect(RectPx::new(x0, y, x1, y + thickness.max(1)), color);
    }

    pub fn vline(&mut self, x: i32, y0: i32, y1: i32, thickness: i32, color: Color) {
        self.fill_rect(RectPx::new(x, y0, x + thickness.max(1), y1), color);
    }

    pub fn stroke_rect_1px(&mut self, rect: RectPx, color: Color) {
        self.hline(rect.left, rect.right, rect.top, 1, color);
        self.hline(rect.left, rect.right, rect.bottom - 1, 1, color);
        self.vline(rect.left, rect.top, rect.bottom, 1, color);
        self.vline(rect.right - 1, rect.top, rect.bottom, 1, color);
    }

    pub fn fill_round_rect(
        &mut self,
        rect: RectPx,
        radius: f32,
        fill: Option<Color>,
        border: Option<(Color, f32)>,
    ) {
        self.fill_round_rect_corners(rect, [radius; 4], fill, border);
    }

    pub fn stroke_round_rect(&mut self, rect: RectPx, radius: f32, color: Color, width: f32) {
        self.fill_round_rect_corners(rect, [radius; 4], None, Some((color, width)));
    }

    /// Per-corner radii in `[top_left, top_right, bottom_right, bottom_left]` order.
    pub fn fill_round_rect_corners(
        &mut self,
        rect: RectPx,
        radii: [f32; 4],
        fill: Option<Color>,
        border: Option<(Color, f32)>,
    ) {
        let w = rect.width() as f32;
        let h = rect.height() as f32;
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let clipped = rect.intersect(self.current_clip());
        if clipped.is_empty() {
            return;
        }

        let cx = rect.left as f32 + w * 0.5;
        let cy = rect.top as f32 + h * 0.5;
        let hx = w * 0.5;
        let hy = h * 0.5;
        let max_r = hx.min(hy);
        let r_tl = radii[0].clamp(0.0, max_r);
        let r_tr = radii[1].clamp(0.0, max_r);
        let r_br = radii[2].clamp(0.0, max_r);
        let r_bl = radii[3].clamp(0.0, max_r);

        for y in clipped.top..clipped.bottom {
            let py = y as f32 + 0.5;
            let is_top = py < cy;
            for x in clipped.left..clipped.right {
                let px_f = x as f32 + 0.5;
                let r = if px_f < cx {
                    if is_top { r_tl } else { r_bl }
                } else if is_top {
                    r_tr
                } else {
                    r_br
                };
                let dist = rounded_box_sdf(px_f, py, cx, cy, hx, hy, r);
                let outer = (0.5 - dist).clamp(0.0, 1.0);
                if outer <= 0.0 {
                    continue;
                }
                match (fill, border) {
                    (Some(fill_col), Some((border_col, bw))) => {
                        let inner = (0.5 - (dist + bw)).clamp(0.0, 1.0);
                        let ring = (outer - inner).clamp(0.0, 1.0);
                        if inner > 0.0 {
                            self.blend_pixel_unchecked(x, y, fill_col, inner);
                        }
                        if ring > 0.0 {
                            self.blend_pixel_unchecked(x, y, border_col, ring);
                        }
                    }
                    (Some(fill_col), None) => {
                        self.blend_pixel_unchecked(x, y, fill_col, outer);
                    }
                    (None, Some((border_col, bw))) => {
                        let inner = (0.5 - (dist + bw)).clamp(0.0, 1.0);
                        let ring = (outer - inner).clamp(0.0, 1.0);
                        if ring > 0.0 {
                            self.blend_pixel_unchecked(x, y, border_col, ring);
                        }
                    }
                    (None, None) => {}
                }
            }
        }
    }

    /// Paints a filled rounded bar masked inside an outer rounded container (used for C2 tactile volume track).
    pub fn fill_bar_inside_round_rect(
        &mut self,
        container: RectPx,
        radius: f32,
        border_w: f32,
        fill_width: i32,
        fill_color: Color,
    ) {
        if fill_width <= 0 {
            return;
        }
        let w = container.width() as f32;
        let h = container.height() as f32;
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let bar_rect = RectPx::new(
            container.left,
            container.top,
            (container.left + fill_width).min(container.right),
            container.bottom,
        );
        let clipped = bar_rect.intersect(self.current_clip());
        if clipped.is_empty() {
            return;
        }

        let cx = container.left as f32 + w * 0.5;
        let cy = container.top as f32 + h * 0.5;
        let hx = w * 0.5;
        let hy = h * 0.5;
        let r = radius.clamp(0.0, hx.min(hy));

        for y in clipped.top..clipped.bottom {
            let py = y as f32 + 0.5;
            for x in clipped.left..clipped.right {
                let px_f = x as f32 + 0.5;
                let dist = rounded_box_sdf(px_f, py, cx, cy, hx, hy, r);
                let inner = (0.5 - (dist + border_w)).clamp(0.0, 1.0);
                if inner > 0.0 {
                    self.blend_pixel_unchecked(x, y, fill_color, inner);
                }
            }
        }
    }

    pub fn draw_shadow_round_rect(
        &mut self,
        rect: RectPx,
        radius: f32,
        spread: f32,
        offset_y: f32,
        max_alpha: f32,
    ) {
        let pad = spread.ceil() as i32;
        let oy = offset_y.round() as i32;
        let shadow_bounds = RectPx::new(
            rect.left - pad,
            rect.top - pad + oy,
            rect.right + pad,
            rect.bottom + pad + oy,
        );
        let clipped = shadow_bounds.intersect(self.current_clip());
        if clipped.is_empty() || spread <= 0.0 || max_alpha <= 0.0 {
            return;
        }

        let w = rect.width() as f32;
        let h = rect.height() as f32;
        let cx = rect.left as f32 + w * 0.5;
        let cy = rect.top as f32 + h * 0.5 + offset_y;
        let hx = w * 0.5;
        let hy = h * 0.5;
        let r = radius.clamp(0.0, hx.min(hy));
        let black = Color::rgb(0x000000);

        for y in clipped.top..clipped.bottom {
            let py = y as f32 + 0.5;
            for x in clipped.left..clipped.right {
                let px_f = x as f32 + 0.5;
                let dist = rounded_box_sdf(px_f, py, cx, cy, hx, hy, r);
                if dist <= 0.0 || dist >= spread {
                    continue;
                }
                let t = 1.0 - dist / spread;
                let alpha = t * t * max_alpha;
                if alpha > 0.004 {
                    self.blend_pixel_unchecked(x, y, black, alpha);
                }
            }
        }
    }

    pub fn draw_icon(&mut self, kind: IconKind, ox: f32, oy: f32, size: f32, color: Color) {
        if size <= 0.0 || color.a == 0 {
            return;
        }
        let clip = self.current_clip();
        let x0 = (ox.floor() as i32).max(clip.left);
        let y0 = (oy.floor() as i32).max(clip.top);
        let x1 = ((ox + size).ceil() as i32).min(clip.right);
        let y1 = ((oy + size).ceil() as i32).min(clip.bottom);
        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let inv_size = 16.0 / size;
        let samples = 4usize;
        let total = (samples * samples) as f32;

        for y in y0..y1 {
            for x in x0..x1 {
                let mut hits = 0.0f32;
                for sy in 0..samples {
                    let wy = (y as f32 + (sy as f32 + 0.5) / samples as f32 - oy) * inv_size;
                    for sx in 0..samples {
                        let wx = (x as f32 + (sx as f32 + 0.5) / samples as f32 - ox) * inv_size;
                        if sample_icon(kind, wx, wy) {
                            hits += 1.0;
                        }
                    }
                }
                if hits > 0.0 {
                    self.blend_pixel_unchecked(x, y, color, hits / total);
                }
            }
        }
    }

    pub fn draw_icon_centered(&mut self, kind: IconKind, rect: RectPx, size: f32, color: Color) {
        let ox = rect.left as f32 + (rect.width() as f32 - size) * 0.5;
        let oy = rect.top as f32 + (rect.height() as f32 - size) * 0.5;
        self.draw_icon(kind, ox, oy, size, color);
    }
}

fn rounded_box_sdf(px: f32, py: f32, cx: f32, cy: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let qx = (px - cx).abs() - (hx - r);
    let qy = (py - cy).abs() - (hy - r);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
}

fn dist_to_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let abx = bx - ax;
    let aby = by - ay;
    let len_sq = abx * abx + aby * aby;
    let t = if len_sq > 0.0 {
        ((px - ax) * abx + (py - ay) * aby) / len_sq
    } else {
        0.0
    }
    .clamp(0.0, 1.0);
    (px - (ax + t * abx)).hypot(py - (ay + t * aby))
}

fn point_in_polygon(px: f32, py: f32, pts: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let n = pts.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = pts[i];
        let (xj, yj) = pts[j];
        let intersect = ((yi > py) != (yj > py))
            && (px < (xj - xi) * (py - yi) / (yj - yi + f32::EPSILON) + xi);
        if intersect {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn dist_to_polygon_edges(px: f32, py: f32, pts: &[(f32, f32)], closed: bool) -> f32 {
    let mut min_d = f32::MAX;
    let n = pts.len();
    let count = if closed { n } else { n.saturating_sub(1) };
    for i in 0..count {
        let (ax, ay) = pts[i];
        let (bx, by) = pts[(i + 1) % n];
        min_d = min_d.min(dist_to_segment(px, py, ax, ay, bx, by));
    }
    min_d
}

fn sample_icon(kind: IconKind, x: f32, y: f32) -> bool {
    match kind {
        IconKind::Lock => {
            let d_body = rounded_box_sdf(x, y, 8.0, 10.5, 5.0, 3.5, 1.8).abs();
            let d_left = dist_to_segment(x, y, 5.2, 5.0, 5.2, 7.0);
            let d_right = dist_to_segment(x, y, 10.8, 5.0, 10.8, 7.0);
            let d_arc = if y <= 5.0 {
                ((x - 8.0).hypot(y - 5.0) - 2.8).abs()
            } else {
                (x - 5.2)
                    .hypot(y - 5.0)
                    .min((x - 10.8).hypot(y - 5.0))
            };
            d_body.min(d_left).min(d_right).min(d_arc) <= 0.78
        }
        IconKind::Unlock => {
            let d_body = rounded_box_sdf(x, y, 8.0, 10.5, 5.0, 3.5, 1.8).abs();
            let d_left = dist_to_segment(x, y, 5.2, 5.0, 5.2, 7.0);
            let end_angle = -0.45f32;
            let end_x = 8.0 + 2.8 * end_angle.cos();
            let end_y = 5.0 + 2.8 * end_angle.sin();
            let angle = (y - 5.0).atan2(x - 8.0);
            let d_arc = if angle <= end_angle && angle >= -PI {
                ((x - 8.0).hypot(y - 5.0) - 2.8).abs()
            } else {
                (x - 5.2)
                    .hypot(y - 5.0)
                    .min((x - end_x).hypot(y - end_y))
            };
            d_body.min(d_left).min(d_arc) <= 0.78
        }
        IconKind::Pin => {
            const HEAD: &[(f32, f32)] = &[
                (9.7, 2.3),
                (13.7, 6.3),
                (12.0, 8.0),
                (10.7, 7.7),
                (8.1, 10.3),
                (8.6, 12.2),
                (7.4, 13.4),
                (2.6, 8.6),
                (3.8, 7.4),
                (5.7, 7.9),
                (8.3, 5.3),
                (8.0, 4.0),
            ];
            let d_head = dist_to_polygon_edges(x, y, HEAD, true);
            let d_needle = dist_to_segment(x, y, 5.0, 11.0, 2.3, 13.7);
            d_head.min(d_needle) <= 0.74
        }
        IconKind::PinSolid => {
            const HEAD: &[(f32, f32)] = &[
                (9.7, 2.3),
                (13.7, 6.3),
                (12.0, 8.0),
                (10.7, 7.7),
                (8.1, 10.3),
                (8.6, 12.2),
                (7.4, 13.4),
                (2.6, 8.6),
                (3.8, 7.4),
                (5.7, 7.9),
                (8.3, 5.3),
                (8.0, 4.0),
            ];
            let in_head = point_in_polygon(x, y, HEAD)
                || dist_to_polygon_edges(x, y, HEAD, true) <= 0.56;
            let d_needle = dist_to_segment(x, y, 5.0, 11.0, 2.3, 13.7);
            in_head || d_needle <= 0.78
        }
        IconKind::Settings => {
            let dx = x - 8.0;
            let dy = y - 8.0;
            let r = dx.hypot(dy);
            let angle = dy.atan2(dx);
            let tooth = (angle * 6.0).cos().clamp(-0.65, 0.65) / 0.65;
            let outer_r = 5.25 + 1.05 * tooth;
            let d_outer = (r - outer_r).abs();
            let d_inner = (r - 2.1).abs();
            d_outer.min(d_inner) <= 0.76
        }
        IconKind::Search => {
            let d_ring = ((x - 7.1).hypot(y - 7.1) - 4.5).abs();
            let d_handle = dist_to_segment(x, y, 10.6, 10.6, 13.5, 13.5);
            d_ring.min(d_handle) <= 0.78
        }
        IconKind::CloseX => {
            let d1 = dist_to_segment(x, y, 4.0, 4.0, 12.0, 12.0);
            let d2 = dist_to_segment(x, y, 12.0, 4.0, 4.0, 12.0);
            d1.min(d2) <= 0.85
        }
        IconKind::Volume => {
            const SPEAKER: &[(f32, f32)] = &[
                (2.5, 6.2),
                (4.9, 6.2),
                (8.2, 3.4),
                (8.2, 12.6),
                (4.9, 9.8),
                (2.5, 9.8),
            ];
            let d_spk = dist_to_polygon_edges(x, y, SPEAKER, true);
            let angle = (y - 8.0).atan2(x - 8.6);
            let in_arc = angle.abs() <= 0.82;
            let r = (x - 8.6).hypot(y - 8.0);
            let d_w1 = if in_arc {
                (r - 2.9).abs()
            } else {
                f32::MAX
            };
            let d_w2 = if in_arc {
                (r - 5.1).abs()
            } else {
                f32::MAX
            };
            d_spk.min(d_w1).min(d_w2) <= 0.78
        }
        IconKind::VolumeMuted => {
            const SPEAKER: &[(f32, f32)] = &[
                (2.5, 6.2),
                (4.9, 6.2),
                (8.2, 3.4),
                (8.2, 12.6),
                (4.9, 9.8),
                (2.5, 9.8),
            ];
            let d_spk = dist_to_polygon_edges(x, y, SPEAKER, true);
            let d_x1 = dist_to_segment(x, y, 10.8, 6.0, 14.0, 10.0);
            let d_x2 = dist_to_segment(x, y, 14.0, 6.0, 10.8, 10.0);
            d_spk.min(d_x1).min(d_x2) <= 0.78
        }
        IconKind::Mic => {
            let d_cap = rounded_box_sdf(x, y, 8.0, 5.6, 2.2, 3.4, 2.2).abs();
            let d_u = if y >= 7.5 {
                ((x - 8.0).hypot(y - 7.5) - 4.2).abs()
            } else {
                (x - 3.8)
                    .hypot(y - 7.5)
                    .min((x - 12.2).hypot(y - 7.5))
            };
            let d_stem = dist_to_segment(x, y, 8.0, 11.7, 8.0, 14.0);
            let d_base = dist_to_segment(x, y, 5.8, 14.0, 10.2, 14.0);
            d_cap.min(d_u).min(d_stem).min(d_base) <= 0.78
        }
        IconKind::MicMuted => {
            let d_slash = dist_to_segment(x, y, 2.4, 2.4, 13.6, 13.6);
            if d_slash <= 0.78 {
                return true;
            }
            if d_slash <= 1.75 {
                return false;
            }
            let d_cap = rounded_box_sdf(x, y, 8.0, 5.6, 2.2, 3.4, 2.2).abs();
            let d_u = if y >= 7.5 {
                ((x - 8.0).hypot(y - 7.5) - 4.2).abs()
            } else {
                (x - 3.8)
                    .hypot(y - 7.5)
                    .min((x - 12.2).hypot(y - 7.5))
            };
            let d_stem = dist_to_segment(x, y, 8.0, 11.7, 8.0, 14.0);
            let d_base = dist_to_segment(x, y, 5.8, 14.0, 10.2, 14.0);
            d_cap.min(d_u).min(d_stem).min(d_base) <= 0.78
        }
        IconKind::Check => {
            let d1 = dist_to_segment(x, y, 3.2, 8.4, 6.5, 11.7);
            let d2 = dist_to_segment(x, y, 6.5, 11.7, 13.0, 4.8);
            d1.min(d2) <= 1.0
        }
        IconKind::ChevronDown => {
            let d1 = dist_to_segment(x, y, 4.0, 6.0, 8.0, 10.0);
            let d2 = dist_to_segment(x, y, 8.0, 10.0, 12.0, 6.0);
            d1.min(d2) <= 0.82
        }
        IconKind::Plus => {
            let d1 = dist_to_segment(x, y, 8.0, 3.2, 8.0, 12.8);
            let d2 = dist_to_segment(x, y, 3.2, 8.0, 12.8, 8.0);
            d1.min(d2) <= 0.82
        }
        IconKind::Power => {
            let d_stem = dist_to_segment(x, y, 8.0, 2.4, 8.0, 7.2);
            let cx = 8.0;
            let cy = 8.2;
            let r = 5.0;
            let angle = (y - cy).atan2(x - cx);
            let gap = 0.72;
            let in_top_gap = angle > (-PI * 0.5 - gap) && angle < (-PI * 0.5 + gap);
            let d_ring = if in_top_gap {
                let a1 = -PI * 0.5 - gap;
                let a2 = -PI * 0.5 + gap;
                (x - (cx + r * a1.cos()))
                    .hypot(y - (cy + r * a1.sin()))
                    .min((x - (cx + r * a2.cos())).hypot(y - (cy + r * a2.sin())))
            } else {
                ((x - cx).hypot(y - cy) - r).abs()
            };
            d_stem.min(d_ring) <= 0.78
        }
    }
}
