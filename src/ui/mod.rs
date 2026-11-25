//! Drawing toolkit shared by the custom-painted windows (audio flyout and settings window).

pub(crate) mod paint;
pub(crate) mod theme;

use std::ffi::c_void;
use std::mem::size_of;

use winaudio::Win32::Foundation::{COLORREF, HWND};
use winaudio::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DwmSetWindowAttribute,
};
use winaudio::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, HBITMAP, HDC, HGDIOBJ, HGDIOBJ as GdiObject, SelectObject, SetBkMode,
    TRANSPARENT,
};

use crate::ui::theme::Color;

/// 32-bit top-down DIB with its own memory DC; the painting surface for both custom windows.
pub(crate) struct DibBackbuffer {
    pub mem_dc: HDC,
    dib: HBITMAP,
    old_obj: HGDIOBJ,
    pub pixels: *mut u32,
    buf_w: i32,
    buf_h: i32,
}

impl DibBackbuffer {
    pub fn empty() -> Self {
        Self {
            mem_dc: HDC::default(),
            dib: HBITMAP::default(),
            old_obj: HGDIOBJ::default(),
            pixels: std::ptr::null_mut(),
            buf_w: 0,
            buf_h: 0,
        }
    }

    pub fn ensure(&mut self, hdc: HDC, width: i32, height: i32) -> bool {
        if width <= 0 || height <= 0 {
            return false;
        }
        if !self.mem_dc.is_invalid()
            && !self.dib.0.is_null()
            && !self.pixels.is_null()
            && self.buf_w == width
            && self.buf_h == height
        {
            return true;
        }

        self.release();

        let mem_dc = unsafe { CreateCompatibleDC(Some(hdc)) };
        if mem_dc.is_invalid() {
            return false;
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(dib) =
            (unsafe { CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) })
        else {
            unsafe {
                let _ = DeleteDC(mem_dc);
            }
            return false;
        };

        if bits.is_null() {
            unsafe {
                let _ = DeleteObject(GdiObject(dib.0));
                let _ = DeleteDC(mem_dc);
            }
            return false;
        }

        let old_obj = unsafe { SelectObject(mem_dc, GdiObject(dib.0)) };
        unsafe {
            SetBkMode(mem_dc, TRANSPARENT);
        }

        self.mem_dc = mem_dc;
        self.dib = dib;
        self.old_obj = old_obj;
        self.pixels = bits.cast::<u32>();
        self.buf_w = width;
        self.buf_h = height;
        true
    }

    fn release(&mut self) {
        if !self.mem_dc.is_invalid() {
            unsafe {
                if !self.old_obj.0.is_null() {
                    SelectObject(self.mem_dc, self.old_obj);
                }
                if !self.dib.0.is_null() {
                    let _ = DeleteObject(GdiObject(self.dib.0));
                }
                let _ = DeleteDC(self.mem_dc);
            }
        } else if !self.dib.0.is_null() {
            unsafe {
                let _ = DeleteObject(GdiObject(self.dib.0));
            }
        }
        self.mem_dc = HDC::default();
        self.dib = HBITMAP::default();
        self.old_obj = HGDIOBJ::default();
        self.pixels = std::ptr::null_mut();
        self.buf_w = 0;
        self.buf_h = 0;
    }
}

impl Drop for DibBackbuffer {
    fn drop(&mut self) {
        self.release();
    }
}

/// Rounded corners and a 1px border in the panel's own border colour.
pub(crate) fn apply_dwm_chrome(hwnd: HWND, border_color: Color) -> bool {
    let corner = DWMWCP_ROUND;
    let border = border_color.colorref();
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const corner).cast::<c_void>(),
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&raw const border).cast::<c_void>(),
            size_of::<COLORREF>() as u32,
        )
        .is_ok()
    }
}
