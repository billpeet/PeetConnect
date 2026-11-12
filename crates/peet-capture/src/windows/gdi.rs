//! GDI screen capture: `BitBlt` from the screen device context.
//!
//! Slow compared with Desktop Duplication (the whole monitor is copied through system
//! memory every frame, with no dirty-rect information) but it works wherever a desktop can
//! be drawn, including when duplication is unavailable or silently delivers nothing. Frames
//! can be uploaded to a D3D11 texture so the GPU encode path still applies.

use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSORINFO_FLAGS};

use super::device::D3dDevice;
use crate::{CaptureError, CpuFrame, CursorState, FrameMeta, MonitorDesc};

/// Captures one monitor with GDI into a DIB section.
pub struct GdiCapturer {
    monitor: MonitorDesc,
    screen_dc: HDC,
    mem_dc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    bits: *mut u8,
    width: u32,
    height: u32,
}

// SAFETY: the GDI handles are only used from one thread at a time, and the DIB memory is
// owned by the bitmap which lives as long as `self`.
unsafe impl Send for GdiCapturer {}

impl std::fmt::Debug for GdiCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GdiCapturer")
            .field("monitor", &self.monitor.name)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl GdiCapturer {
    pub fn new(monitor: &MonitorDesc) -> Result<Self, CaptureError> {
        let width = monitor.bounds.width;
        let height = monitor.bounds.height;
        if width == 0 || height == 0 {
            return Err(CaptureError::Unsupported("monitor has zero size".into()));
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or(40),
                biWidth: i32::try_from(width).unwrap_or(i32::MAX),
                // Negative height: top-down rows.
                biHeight: -i32::try_from(height).unwrap_or(i32::MAX),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: standard GDI setup; every handle is released in Drop.
        unsafe {
            let screen_dc = GetDC(None);
            if screen_dc.is_invalid() {
                return Err(CaptureError::AccessDenied);
            }
            let mem_dc = CreateCompatibleDC(Some(screen_dc));
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap =
                match CreateDIBSection(Some(mem_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                    Ok(b) => b,
                    Err(err) => {
                        let _ = DeleteDC(mem_dc);
                        ReleaseDC(None, screen_dc);
                        return Err(err.into());
                    }
                };
            let old_bitmap = SelectObject(mem_dc, bitmap.into());
            Ok(Self {
                monitor: monitor.clone(),
                screen_dc,
                mem_dc,
                bitmap,
                old_bitmap,
                bits: bits.cast(),
                width,
                height,
            })
        }
    }

    pub fn monitor(&self) -> &MonitorDesc {
        &self.monitor
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Copies the monitor into the internal bitmap. Always reports the whole frame dirty.
    pub fn capture(&mut self) -> Result<FrameMeta, CaptureError> {
        // SAFETY: both DCs are valid for the lifetime of self.
        unsafe {
            BitBlt(
                self.mem_dc,
                0,
                0,
                i32::try_from(self.width).unwrap_or(i32::MAX),
                i32::try_from(self.height).unwrap_or(i32::MAX),
                Some(self.screen_dc),
                self.monitor.bounds.x,
                self.monitor.bounds.y,
                // No CAPTUREBLT: with DWM composition the screen DC already includes layered
                // windows, and CAPTUREBLT costs ~25 ms per frame at 1080p.
                SRCCOPY,
            )?;
        }
        let captured_at = Instant::now();
        Ok(FrameMeta {
            captured_at: Some(captured_at),
            desktop_updated: true,
            accumulated_frames: 1,
            dirty: vec![peet_proto::Rect {
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
            }],
            moved: Vec::new(),
            cursor: cursor_position(&self.monitor),
        })
    }

    /// The last captured pixels: BGRA8, top-down, tightly packed.
    pub fn pixels(&self) -> &[u8] {
        // SAFETY: the DIB section is width*height*4 bytes and lives as long as self.
        unsafe { std::slice::from_raw_parts(self.bits, (self.width * self.height * 4) as usize) }
    }

    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    pub fn read_cpu(&self, out: &mut CpuFrame) {
        out.width = self.width;
        out.height = self.height;
        out.stride = self.stride();
        out.bgra.clear();
        out.bgra.extend_from_slice(self.pixels());
    }

    /// Uploads the last captured pixels into a BGRA8 texture of the same size.
    pub fn upload(&self, device: &D3dDevice, texture: &ID3D11Texture2D) {
        // SAFETY: the source is width*height*4 bytes with the given pitch; the texture must
        // match in size and format, which the caller guarantees by creating it from `size()`.
        unsafe {
            device.context.UpdateSubresource(
                texture,
                0,
                None,
                self.bits.cast(),
                u32::try_from(self.stride()).unwrap_or(u32::MAX),
                0,
            );
        }
    }
}

impl Drop for GdiCapturer {
    fn drop(&mut self) {
        // SAFETY: handles were created in `new` and are released exactly once.
        unsafe {
            SelectObject(self.mem_dc, self.old_bitmap);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.mem_dc);
            ReleaseDC(None, self.screen_dc);
        }
    }
}

/// `CURSOR_SHOWING` from WinUser.h.
const CURSOR_SHOWING: CURSORINFO_FLAGS = CURSORINFO_FLAGS(1);

/// Current cursor position relative to `monitor`, if the cursor is showing.
fn cursor_position(monitor: &MonitorDesc) -> Option<CursorState> {
    let mut info = CURSORINFO {
        cbSize: u32::try_from(std::mem::size_of::<CURSORINFO>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: cbSize is set as required.
    unsafe { GetCursorInfo(&mut info) }.ok()?;
    Some(CursorState {
        x: info.ptScreenPos.x - monitor.bounds.x,
        y: info.ptScreenPos.y - monitor.bounds.y,
        visible: info.flags.0 & CURSOR_SHOWING.0 != 0,
        shape: None,
    })
}
