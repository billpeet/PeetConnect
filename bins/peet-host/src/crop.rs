//! Window capture without Windows.Graphics.Capture: the window's rectangle cut out of its
//! monitor's capture.
//!
//! WGC is unavailable to the SYSTEM account. Run as SYSTEM in the console session, the way the
//! service worker runs, `GraphicsCaptureSession::IsSupported` returns false. Desktop
//! Duplication and GDI keep working there (measured 2026-11-14: monitor stream at 44 fps). So
//! when WGC is unavailable, a window stream captures the monitor under the window's centre and
//! copies out the window's visible rectangle.
//!
//! The cost is that this shows the screen, not the window: anything covering the window is in
//! the picture, and so is any part of it hanging off its monitor. Input brings the window to
//! the front, which uncovers it. A minimised window delivers no frames, as with WGC.

use std::sync::OnceLock;
use std::time::Duration;

use peet_capture::windows::windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
    D3D11_CPU_ACCESS_FLAG, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use peet_capture::windows::{
    describe_window, enumerate_monitors, wgc_supported, Backend, D3dDevice, FrameSource,
    GdiCapturer,
};
use peet_capture::{CaptureError, FrameMeta, MonitorDesc};
use peet_proto::Rect;

/// Whether WGC works in this process. Checked once: it depends on the account, not the window.
pub fn wgc_usable() -> bool {
    static USABLE: OnceLock<bool> = OnceLock::new();
    *USABLE.get_or_init(|| {
        let usable = wgc_supported();
        if !usable {
            tracing::info!(
                "Windows.Graphics.Capture is unavailable (as SYSTEM it always is); window \
                 streams will be cut out of the monitor capture"
            );
        }
        usable
    })
}

/// The monitor under the centre of `bounds`, else the one it overlaps most.
pub fn monitor_for(bounds: Rect) -> Option<MonitorDesc> {
    let monitors = enumerate_monitors().ok()?;
    let cx = i64::from(bounds.x) + i64::from(bounds.width / 2);
    let cy = i64::from(bounds.y) + i64::from(bounds.height / 2);
    if let Some(m) = monitors.iter().find(|m| contains(m.bounds, cx, cy)) {
        return Some(m.clone());
    }
    monitors
        .into_iter()
        .map(|m| (area(clip(bounds, m.bounds)), m))
        .filter(|(a, _)| *a > 0)
        .max_by_key(|(a, _)| *a)
        .map(|(_, m)| m)
}

fn contains(r: Rect, x: i64, y: i64) -> bool {
    x >= i64::from(r.x)
        && y >= i64::from(r.y)
        && x < i64::from(r.x) + i64::from(r.width)
        && y < i64::from(r.y) + i64::from(r.height)
}

fn area(r: Rect) -> u64 {
    u64::from(r.width) * u64::from(r.height)
}

/// The part of `r` inside `within`; zero-sized when they do not overlap.
pub fn clip(r: Rect, within: Rect) -> Rect {
    let right = |r: Rect| i64::from(r.x) + i64::from(r.width);
    let bottom = |r: Rect| i64::from(r.y) + i64::from(r.height);
    let x = r.x.max(within.x);
    let y = r.y.max(within.y);
    let w = (right(r).min(right(within)) - i64::from(x)).max(0);
    let h = (bottom(r).min(bottom(within)) - i64::from(y)).max(0);
    Rect {
        x,
        y,
        width: u32::try_from(w).unwrap_or(0),
        height: u32::try_from(h).unwrap_or(0),
    }
}

/// Even dimensions for the encoder, at least 2x2.
fn even(r: Rect) -> (u32, u32) {
    ((r.width & !1).max(2), (r.height & !1).max(2))
}

/// A window stream cut out of its monitor's capture.
pub struct CropCapture {
    window: u64,
    monitor: MonitorDesc,
    source: FrameSource,
    device: D3dDevice,
    texture: ID3D11Texture2D,
    size: (u32, u32),
    /// The window's bounds at the last acquire, in desktop coordinates.
    bounds: Rect,
}

impl CropCapture {
    /// `device` must belong to the adapter of the monitor under the window (see
    /// [`monitor_for`]). WGC is left out of the monitor backend, since it is what is missing.
    pub fn new(
        device: &D3dDevice,
        window: u64,
        backend: Backend,
        fps: u32,
    ) -> Result<Self, CaptureError> {
        let desc = describe_window(window).ok_or(CaptureError::Closed)?;
        let monitor = monitor_for(desc.bounds)
            .ok_or_else(|| CaptureError::Unsupported("the window is on no monitor".into()))?;
        let backend = match backend {
            Backend::Auto | Backend::Wgc => Backend::AutoNoWgc,
            other => other,
        };
        // A process can duplicate each output only once, so a second stream on this monitor
        // (its monitor stream, or another window on it) gets E_INVALIDARG from DXGI. GDI has
        // no such limit.
        let source = match FrameSource::new(device.clone(), &monitor, backend, fps) {
            Ok(source) => source,
            Err(err) if backend != Backend::Gdi => {
                tracing::info!(
                    monitor = %monitor.name, %err,
                    "monitor capture busy or unavailable; cutting the window out of GDI capture"
                );
                FrameSource::new(device.clone(), &monitor, Backend::Gdi, fps)?
            }
            Err(err) => return Err(err),
        };
        let size = even(clip(desc.bounds, monitor.bounds));
        let texture = cutout_texture(device, &source, size)?;
        Ok(Self {
            window,
            monitor,
            source,
            device: device.clone(),
            texture,
            size,
            bounds: desc.bounds,
        })
    }

    /// Follows a resize on the same monitor: a new cut-out texture at the window's size,
    /// keeping the monitor capture (a second duplication of the output would be refused).
    pub fn resize(&mut self) -> Result<(), CaptureError> {
        let desc = describe_window(self.window).ok_or(CaptureError::Closed)?;
        self.bounds = desc.bounds;
        self.size = even(clip(desc.bounds, self.monitor.bounds));
        self.texture = cutout_texture(&self.device, &self.source, self.size)?;
        self.copy();
        Ok(())
    }

    pub fn name(&self) -> &'static str {
        match self.source.name() {
            "dxgi" => "dxgi-crop",
            "gdi" => "gdi-crop",
            _ => "crop",
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    /// The visible size the window would have if rebuilt now (before rounding to even).
    pub fn content_size(&self) -> (u32, u32) {
        let r = clip(self.bounds, self.monitor.bounds);
        (r.width, r.height)
    }

    /// True once the window's centre has left the monitor being captured, so the capture must
    /// be rebuilt on the monitor it moved to.
    pub fn left_monitor(&self) -> bool {
        let b = self.bounds;
        !contains(
            self.monitor.bounds,
            i64::from(b.x) + i64::from(b.width / 2),
            i64::from(b.y) + i64::from(b.height / 2),
        )
    }

    pub fn wait_for_slot(&mut self) {
        self.source.wait_for_slot();
    }

    pub fn acquire(&mut self, timeout: Duration) -> Result<FrameMeta, CaptureError> {
        let desc = describe_window(self.window).ok_or(CaptureError::Closed)?;
        let result = self.source.acquire(timeout);
        if desc.minimized {
            // Parked off-screen; the monitor shows whatever was behind it.
            return match result {
                Err(CaptureError::AccessLost) => Err(CaptureError::AccessLost),
                _ => Err(CaptureError::Timeout),
            };
        }
        let moved = desc.bounds != self.bounds;
        self.bounds = desc.bounds;
        let mut meta = match result {
            Ok(meta) => meta,
            // A move redraws the desktop, so DXGI reports it; this covers a still capture.
            Err(CaptureError::Timeout) if moved => FrameMeta {
                desktop_updated: true,
                ..FrameMeta::default()
            },
            Err(err) => return Err(err),
        };
        if meta.desktop_updated {
            self.copy();
        }
        // The backend reports the cursor relative to the monitor; the stream wants it
        // relative to the window.
        if let Some(cursor) = &mut meta.cursor {
            cursor.x -= self.bounds.x - self.monitor.bounds.x;
            cursor.y -= self.bounds.y - self.monitor.bounds.y;
        }
        Ok(meta)
    }

    fn copy(&self) {
        let mon = self.monitor.bounds;
        let v = clip(self.bounds, mon);
        let width = v.width.min(self.size.0);
        let height = v.height.min(self.size.1);
        if width == 0 || height == 0 {
            return;
        }
        let left = u32::try_from(v.x - mon.x).unwrap_or(0);
        let top = u32::try_from(v.y - mon.y).unwrap_or(0);
        let region = D3D11_BOX {
            left,
            top,
            front: 0,
            right: left + width,
            bottom: top + height,
            back: 1,
        };
        // SAFETY: both textures live on this device and share a format; the box lies inside
        // the monitor texture because it was clipped to the monitor's bounds.
        unsafe {
            self.device.context.CopySubresourceRegion(
                &self.texture,
                0,
                0,
                0,
                0,
                self.source.texture(),
                0,
                Some(&raw const region),
            );
        }
    }
}

/// A texture for the cut-out in the monitor capture's format, so the copy is a plain region
/// copy.
fn cutout_texture(
    device: &D3dDevice,
    source: &FrameSource,
    size: (u32, u32),
) -> Result<ID3D11Texture2D, CaptureError> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: GetDesc fills a plain struct from a live texture.
    unsafe { source.texture().GetDesc(&raw mut desc) };
    device.create_texture(
        size.0,
        size.1,
        desc.Format,
        D3D11_USAGE_DEFAULT,
        D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
        D3D11_CPU_ACCESS_FLAG(0),
    )
}

/// A still of a window's visible rectangle through GDI, for when WGC is unavailable. Returns
/// tightly packed BGRA8 rows with their width and height.
pub fn still(window: u64) -> Result<(Vec<u8>, u32, u32), String> {
    let desc = describe_window(window).ok_or_else(|| format!("window {window:#x} not found"))?;
    if desc.minimized {
        return Err("the window is minimised; focus it first".into());
    }
    let monitor = monitor_for(desc.bounds).ok_or("the window is on no monitor")?;
    let v = clip(desc.bounds, monitor.bounds);
    if v.width == 0 || v.height == 0 {
        return Err("the window is off screen".into());
    }
    let mut cap = GdiCapturer::new(&monitor).map_err(|e| e.to_string())?;
    cap.capture().map_err(|e| e.to_string())?;
    let stride = cap.stride();
    let pixels = cap.pixels();
    let left = usize::try_from(v.x - monitor.bounds.x).unwrap_or(0) * 4;
    let top = usize::try_from(v.y - monitor.bounds.y).unwrap_or(0);
    let row_bytes = v.width as usize * 4;
    let mut out = Vec::with_capacity(row_bytes * v.height as usize);
    for row in pixels
        .chunks_exact(stride)
        .skip(top)
        .take(v.height as usize)
    {
        out.extend_from_slice(&row[left..left + row_bytes]);
    }
    Ok((out, v.width, v.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn clip_inside_partial_and_outside() {
        let mon = r(0, 0, 1920, 1080);
        assert_eq!(clip(r(100, 100, 800, 600), mon), r(100, 100, 800, 600));
        assert_eq!(clip(r(-50, 1000, 200, 200), mon), r(0, 1000, 150, 80));
        assert_eq!(clip(r(2000, 0, 100, 100), mon).width, 0);
        // A monitor left of the primary.
        assert_eq!(
            clip(r(-500, 10, 1000, 100), r(-1920, 0, 1920, 1080)),
            r(-500, 10, 500, 100)
        );
    }

    #[test]
    fn even_rounds_down_with_a_floor() {
        assert_eq!(even(r(0, 0, 783, 591)), (782, 590));
        assert_eq!(even(r(0, 0, 1, 0)), (2, 2));
    }
}
