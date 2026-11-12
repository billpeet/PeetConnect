//! Cursor shape extraction for the GDI backend.
//!
//! Desktop Duplication hands us the pointer shape for free; GDI does not, so we ask the
//! window manager for the current cursor handle and rasterise it with `GetIconInfo` and
//! `GetDIBits`. Shapes are cached by handle because most cursors never change.

use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, HCURSOR, ICONINFO};

use crate::{CursorShapeData, CursorShapeKind};

/// Remembers the last cursor handle so a shape is only rasterised when it changes.
#[derive(Debug, Default)]
pub struct CursorTracker {
    last: isize,
}

impl CursorTracker {
    /// Returns the new shape if `cursor` differs from the previous call, else `None`.
    pub fn shape_if_changed(&mut self, cursor: HCURSOR) -> Option<CursorShapeData> {
        let handle = cursor.0 as isize;
        if handle == self.last || cursor.is_invalid() {
            return None;
        }
        let shape = shape_from_handle(cursor);
        if shape.is_some() {
            self.last = handle;
        }
        shape
    }

    /// Forget the cached handle so the next call re-sends the shape.
    pub fn reset(&mut self) {
        self.last = 0;
    }
}

fn shape_from_handle(cursor: HCURSOR) -> Option<CursorShapeData> {
    let mut info = ICONINFO::default();
    // SAFETY: GetIconInfo fills `info`; the bitmaps it returns are deleted below.
    unsafe { GetIconInfo(cursor.into(), &mut info) }.ok()?;
    let result = rasterise(&info);
    // SAFETY: handles came from GetIconInfo and are owned by us.
    unsafe {
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(info.hbmColor.into());
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(info.hbmMask.into());
        }
    }
    result
}

fn rasterise(info: &ICONINFO) -> Option<CursorShapeData> {
    let (mask_w, mask_h) = bitmap_size(info.hbmMask)?;
    if mask_w == 0 || mask_h == 0 {
        return None;
    }
    let mask = read_bgra(info.hbmMask, mask_w, mask_h)?;
    let (width, height, bgra) = if info.hbmColor.is_invalid() {
        // Monochrome: the mask bitmap holds the AND plane above the XOR plane.
        let height = mask_h / 2;
        let row = mask_w as usize * 4;
        let (and_plane, xor_plane) = mask.split_at(row * height as usize);
        let mut out = vec![0u8; row * height as usize];
        for (i, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let and = and_plane[i * 4] != 0;
            let xor = xor_plane[i * 4] != 0;
            let (b, g, r, a) = match (and, xor) {
                (false, false) => (0, 0, 0, 255),
                (false, true) => (255, 255, 255, 255),
                (true, false) => (0, 0, 0, 0),
                // Inverted: not representable; mid-grey keeps it visible on any background.
                (true, true) => (128, 128, 128, 255),
            };
            px.copy_from_slice(&[b, g, r, a]);
        }
        (mask_w, height, out)
    } else {
        let (w, h) = bitmap_size(info.hbmColor)?;
        let mut color = read_bgra(info.hbmColor, w, h)?;
        let has_alpha = color.as_chunks::<4>().0.iter().any(|px| px[3] != 0);
        if has_alpha {
            premultiply(&mut color);
        } else if mask_h >= h && mask_w >= w {
            // No alpha channel: the AND mask says where the cursor is transparent.
            let row_mask = mask_w as usize * 4;
            for (y, row) in color.chunks_exact_mut(w as usize * 4).enumerate() {
                for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let transparent = mask[y * row_mask + x * 4] != 0;
                    if transparent {
                        px.copy_from_slice(&[0, 0, 0, 0]);
                    } else {
                        px[3] = 255;
                    }
                }
            }
        } else {
            for px in color.as_chunks_mut::<4>().0 {
                px[3] = 255;
            }
        }
        (w, h, color)
    };
    Some(CursorShapeData {
        width,
        height,
        hotspot_x: info.xHotspot,
        hotspot_y: info.yHotspot,
        kind: CursorShapeKind::Color,
        pitch: width * 4,
        data: bgra,
    })
}

fn bitmap_size(bitmap: HBITMAP) -> Option<(u32, u32)> {
    let mut bm = BITMAP::default();
    let size = i32::try_from(std::mem::size_of::<BITMAP>()).ok()?;
    // SAFETY: `bm` is a BITMAP and `size` matches it.
    let written = unsafe { GetObjectW(bitmap.into(), size, Some((&raw mut bm).cast())) };
    if written == 0 {
        return None;
    }
    Some((
        u32::try_from(bm.bmWidth).ok()?,
        u32::try_from(bm.bmHeight).ok()?,
    ))
}

/// Reads a bitmap as top-down 32 bpp BGRA. Monochrome bitmaps come back as black/white.
fn read_bgra(bitmap: HBITMAP, width: u32, height: u32) -> Option<Vec<u8>> {
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or(40),
            biWidth: i32::try_from(width).ok()?,
            biHeight: -i32::try_from(height).ok()?,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut out = vec![0u8; width as usize * height as usize * 4];
    // SAFETY: the buffer is sized for width*height 32 bpp pixels as described by `info`.
    let lines = unsafe {
        let dc = GetDC(None);
        let lines = GetDIBits(
            dc,
            bitmap,
            0,
            height,
            Some(out.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, dc);
        lines
    };
    (lines > 0).then_some(out)
}

fn premultiply(bgra: &mut [u8]) {
    for px in bgra.as_chunks_mut::<4>().0 {
        let a = u16::from(px[3]);
        if a != 255 {
            for c in &mut px[..3] {
                *c = ((u16::from(*c) * a + 127) / 255) as u8;
            }
        }
    }
}
