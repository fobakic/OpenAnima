//! Region screen capture via GDI.
//!
//! The capture device and its DIB are allocated once and reused for every
//! grab. This matters: the Python implementation created a fresh `mss.mss()`
//! instance for every region on every tick, which dominated the cost of the
//! trigger loop.

use std::ffi::c_void;

use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CAPTUREBLT, DIB_RGB_COLORS, HDC, HBITMAP,
    HGDIOBJ, ROP_CODE, SRCCOPY,
};

/// Reusable screen grabber.
pub struct ScreenCapture {
    screen_dc: HDC,
    mem_dc: HDC,
    dib: windows::Win32::Graphics::Gdi::HBITMAP,
    old_obj: windows::Win32::Graphics::Gdi::HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
    row_bytes: usize,
}

impl ScreenCapture {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let screen_dc = GetDC(None);
            let mem_dc = CreateCompatibleDC(screen_dc);
            if mem_dc.is_invalid() {
                return Err("CreateCompatibleDC failed".into());
            }
            Ok(ScreenCapture {
                screen_dc,
                mem_dc,
                dib: HBITMAP(std::ptr::null_mut()),
                old_obj: HGDIOBJ(std::ptr::null_mut()),
                bits: std::ptr::null_mut(),
                width: 0,
                height: 0,
                row_bytes: 0,
            })
        }
    }

    /// Captures a screen region into the internal buffer.
    ///
    /// Coordinates are in virtual-screen space, matching the convention used
    /// by the Python `mss` backend.
    pub fn grab(&mut self, x: i32, y: i32, width: i32, height: i32) -> Result<(), String> {
        if width <= 0 || height <= 0 {
            return Err("region must have a positive size".into());
        }
        if width != self.width || height != self.height {
            self.resize(width, height)?;
        }

        unsafe {
            let ok = BitBlt(
                self.mem_dc,
                0,
                0,
                width,
                height,
                self.screen_dc,
                x,
                y,
                ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
            );
            if ok.is_err() {
                return Err(format!("BitBlt failed: {}", windows::core::Error::from_win32()));
            }
        }
        Ok(())
    }

    fn resize(&mut self, width: i32, height: i32) -> Result<(), String> {
        unsafe {
            if !self.dib.is_invalid() {
                let _ = SelectObject(self.mem_dc, self.old_obj);
                let _ = DeleteObject(self.dib);
            }

            let row_bytes = (width as usize) * 4;
            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [Default::default(); 1],
            };

            let mut bits: *mut c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(
                self.mem_dc,
                &mut bmi as *mut BITMAPINFO,
                DIB_RGB_COLORS,
                &mut bits as *mut *mut c_void,
                None,
                0,
            )
            .map_err(|e| format!("CreateDIBSection: {e}"))?;

            let old_obj = SelectObject(self.mem_dc, dib);
            self.dib = dib;
            self.old_obj = old_obj;
            self.bits = bits as *mut u8;
            self.width = width;
            self.height = height;
            self.row_bytes = row_bytes;
        }
        Ok(())
    }

    /// Returns the captured pixels as straight RGB, top-down.
    #[allow(dead_code)]
    pub fn to_rgb(&self) -> Vec<u8> {
        let pixels = (self.width as usize) * (self.height as usize);
        let mut out = vec![0u8; pixels * 3];
        if self.bits.is_null() {
            return out;
        }
        let src = unsafe { std::slice::from_raw_parts(self.bits, self.row_bytes * self.height as usize) };
        for i in 0..pixels {
            let s = i * 4;
            let d = i * 3;
            if s + 4 > src.len() || d + 3 > out.len() {
                break;
            }
            out[d] = src[s + 2];
            out[d + 1] = src[s + 1];
            out[d + 2] = src[s];
        }
        out
    }

    /// Returns the captured pixels as grayscale, top-down.
    pub fn to_gray(&self) -> Vec<u8> {
        let pixels = (self.width as usize) * (self.height as usize);
        let mut out = vec![0u8; pixels];
        if self.bits.is_null() {
            return out;
        }
        let src = unsafe { std::slice::from_raw_parts(self.bits, self.row_bytes * self.height as usize) };
        for i in 0..pixels {
            let s = i * 4;
            if s + 4 > src.len() {
                break;
            }
            // BT.601 luma, matching OpenCV's default for 8-bit RGB.
            let luma = (src[s + 2] as u32 * 77 + src[s + 1] as u32 * 150 + src[s] as u32 * 29) >> 8;
            out[i] = luma.min(255) as u8;
        }
        out
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        unsafe {
            if !self.dib.is_invalid() {
                let _ = SelectObject(self.mem_dc, self.old_obj);
                let _ = DeleteObject(self.dib);
            }
            let _ = DeleteDC(self.mem_dc);
            let _ = ReleaseDC(None, self.screen_dc);
        }
    }
}
