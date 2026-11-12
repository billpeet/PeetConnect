//! A frame source that picks a backend and keeps the latest frame in a GPU texture.
//!
//! Wraps [`DxgiCapturer`] and [`GdiCapturer`] behind one interface, with a watchdog: in
//! `Auto` mode, if Desktop Duplication produces no desktop frame within a grace period the
//! source switches to GDI. Desktop Duplication can fail silently in exactly this way.

use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_FLAG,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

use super::{D3dDevice, DxgiCapturer, GdiCapturer};
use crate::{CaptureError, CpuFrame, FrameMeta, MonitorDesc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// DXGI first; fall back to GDI if it delivers nothing.
    #[default]
    Auto,
    Dxgi,
    Gdi,
}

/// How long `Auto` waits for DXGI to produce a desktop frame before switching to GDI.
pub const DXGI_GRACE: Duration = Duration::from_millis(1500);

enum Inner {
    Dxgi(DxgiCapturer),
    Gdi {
        cap: GdiCapturer,
        texture: ID3D11Texture2D,
        interval: Duration,
        next: Instant,
    },
}

pub struct FrameSource {
    device: D3dDevice,
    monitor: MonitorDesc,
    backend: Backend,
    fps: u32,
    inner: Inner,
    started: Instant,
    got_frame: bool,
    fell_back: bool,
}

impl std::fmt::Debug for FrameSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameSource")
            .field("monitor", &self.monitor.name)
            .field("backend", &self.name())
            .finish_non_exhaustive()
    }
}

impl FrameSource {
    /// `fps` paces the GDI backend; DXGI is paced by the compositor.
    pub fn new(
        device: D3dDevice,
        monitor: &MonitorDesc,
        backend: Backend,
        fps: u32,
    ) -> Result<Self, CaptureError> {
        let inner = match backend {
            Backend::Gdi => gdi(&device, monitor, fps)?,
            Backend::Dxgi | Backend::Auto => {
                Inner::Dxgi(DxgiCapturer::new(device.clone(), monitor)?)
            }
        };
        Ok(Self {
            device,
            monitor: monitor.clone(),
            backend,
            fps,
            inner,
            started: Instant::now(),
            got_frame: false,
            fell_back: false,
        })
    }

    pub fn name(&self) -> &'static str {
        match self.inner {
            Inner::Dxgi(_) => "dxgi",
            Inner::Gdi { .. } => "gdi",
        }
    }

    /// True once the watchdog has switched from DXGI to GDI.
    pub fn fell_back(&self) -> bool {
        self.fell_back
    }

    pub fn device(&self) -> &D3dDevice {
        &self.device
    }

    pub fn size(&self) -> (u32, u32) {
        match &self.inner {
            Inner::Dxgi(c) => c.size(),
            Inner::Gdi { cap, .. } => cap.size(),
        }
    }

    /// Blocks until the next frame slot for paced backends. Call before timing a capture.
    pub fn wait_for_slot(&mut self) {
        if let Inner::Gdi { interval, next, .. } = &mut self.inner {
            let now = Instant::now();
            if *next > now {
                std::thread::sleep(*next - now);
            }
            let floor = Instant::now().checked_sub(*interval).unwrap_or(now);
            *next = (*next + *interval).max(floor);
        }
    }

    /// Acquires the next frame into [`texture`](Self::texture).
    ///
    /// Returns [`CaptureError::Timeout`] when nothing changed. On
    /// [`CaptureError::AccessLost`] the DXGI session is recreated transparently and the
    /// error is still returned so the caller can account for it.
    pub fn acquire(&mut self, timeout: Duration) -> Result<FrameMeta, CaptureError> {
        if self.backend == Backend::Auto
            && !self.got_frame
            && matches!(self.inner, Inner::Dxgi(_))
            && self.started.elapsed() > DXGI_GRACE
        {
            tracing::warn!(
                monitor = %self.monitor.name,
                "DXGI delivered no frames in {DXGI_GRACE:?}; switching to GDI capture"
            );
            self.inner = gdi(&self.device, &self.monitor, self.fps)?;
            self.fell_back = true;
        }
        let result = match &mut self.inner {
            Inner::Dxgi(c) => c.acquire(timeout),
            Inner::Gdi { cap, texture, .. } => {
                let meta = cap.capture()?;
                cap.upload(&self.device, texture);
                Ok(meta)
            }
        };
        match &result {
            Ok(meta) if meta.desktop_updated => self.got_frame = true,
            Err(CaptureError::AccessLost) => {
                self.inner = Inner::Dxgi(DxgiCapturer::new(self.device.clone(), &self.monitor)?);
            }
            _ => {}
        }
        result
    }

    /// Latest frame, BGRA8 on the GPU (DXGI may report another format via
    /// [`DxgiCapturer::format`]).
    pub fn texture(&self) -> &ID3D11Texture2D {
        match &self.inner {
            Inner::Dxgi(c) => c.texture(),
            Inner::Gdi { texture, .. } => texture,
        }
    }

    pub fn read_cpu(&mut self, out: &mut CpuFrame) -> Result<(), CaptureError> {
        match &mut self.inner {
            Inner::Dxgi(c) => c.read_cpu(out),
            Inner::Gdi { cap, .. } => {
                cap.read_cpu(out);
                Ok(())
            }
        }
    }
}

fn gdi(device: &D3dDevice, monitor: &MonitorDesc, fps: u32) -> Result<Inner, CaptureError> {
    let cap = GdiCapturer::new(monitor)?;
    let (w, h) = cap.size();
    let texture = device.create_texture(
        w,
        h,
        DXGI_FORMAT_B8G8R8A8_UNORM,
        D3D11_USAGE_DEFAULT,
        D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
        D3D11_CPU_ACCESS_FLAG(0),
    )?;
    Ok(Inner::Gdi {
        cap,
        texture,
        interval: Duration::from_secs(1) / fps.max(1),
        next: Instant::now(),
    })
}
