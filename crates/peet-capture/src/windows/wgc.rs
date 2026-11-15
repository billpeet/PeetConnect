//! Windows.Graphics.Capture: monitor or single-window capture through DWM.
//!
//! WGC reads DWM's surfaces directly, so a window keeps producing frames while other windows
//! cover it, and frames arrive as D3D11 textures on our device. It is also an alternative to
//! Desktop Duplication for whole monitors, which on some machines delivers nothing.
//!
//! The frame pool is free-threaded: DWM signals `FrameArrived` on a worker thread, which only
//! wakes [`WgcCapturer::acquire`]; frames are pulled and copied on the caller's thread.
//!
//! The output texture keeps the size the capture started with, rounded down to even numbers
//! for the encoder. When a window is resized the content is copied into its top-left corner,
//! clipped or padded with black, and [`WgcCapturer::content_size`] reports the real size so a
//! caller can restart its encoder.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Interface, HRESULT};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind,
    GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11RenderTargetView, ID3D11Texture2D, D3D11_BIND_FLAG, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_FLAG, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use super::cursor::CursorTracker;
use super::device::D3dDevice;
use super::gdi::cursor_state;
use super::toplevel;
use crate::{CaptureError, CpuFrame, FrameMeta, MonitorDesc};

/// What a [`WgcCapturer`] captures.
#[derive(Debug, Clone)]
pub enum WgcTarget {
    Monitor(MonitorDesc),
    /// A top-level window by [`crate::WindowDesc::id`].
    Window(u64),
}

/// Session options. Each is best effort: older Windows builds lack some of them, and a
/// failure to apply one is logged, not fatal.
#[derive(Debug, Clone, Copy)]
pub struct WgcOptions {
    /// Ask for no yellow capture border (Windows 11; needs a borderless access grant, which
    /// unpackaged desktop apps normally get without a prompt).
    pub borderless: bool,
    /// Include the window's popups, menus and dialogs (Windows 11 24H2+). Windows only.
    pub include_secondary_windows: bool,
    /// Lower bound on the time between frames, so DWM does not wake us faster than the
    /// stream rate (Windows 11 24H2+).
    pub min_interval: Option<Duration>,
}

impl Default for WgcOptions {
    fn default() -> Self {
        Self {
            borderless: true,
            include_secondary_windows: true,
            min_interval: None,
        }
    }
}

/// Which session options took effect, for diagnostics.
#[derive(Debug, Clone, Copy, Default)]
pub struct WgcApplied {
    pub borderless: bool,
    pub secondary_windows: bool,
    pub min_interval: bool,
}

#[derive(Default)]
struct Signal {
    ready: Mutex<bool>,
    cond: Condvar,
}

impl Signal {
    fn notify(&self) {
        if let Ok(mut ready) = self.ready.lock() {
            *ready = true;
            self.cond.notify_all();
        }
    }

    /// Waits until notified or `timeout` passes; clears the flag either way.
    fn wait(&self, timeout: Duration) {
        let Ok(guard) = self.ready.lock() else {
            return;
        };
        if let Ok((mut ready, _)) = self.cond.wait_timeout_while(guard, timeout, |r| !*r) {
            *ready = false;
        }
    }
}

pub struct WgcCapturer {
    device: D3dDevice,
    target: WgcTarget,
    winrt_device: IDirect3DDevice,
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    item: GraphicsCaptureItem,
    frame_token: i64,
    closed_token: i64,
    signal: Arc<Signal>,
    closed: Arc<AtomicBool>,
    pool_size: SizeInt32,
    width: u32,
    height: u32,
    content: (u32, u32),
    frame: ID3D11Texture2D,
    rtv: Option<ID3D11RenderTargetView>,
    staging: Option<ID3D11Texture2D>,
    cursor: CursorTracker,
    applied: WgcApplied,
}

impl std::fmt::Debug for WgcCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgcCapturer")
            .field("target", &self.target)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// True when this Windows build has Windows.Graphics.Capture (1903 or later).
pub fn wgc_supported() -> bool {
    init_winrt();
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

fn init_winrt() {
    // SAFETY: per-thread apartment initialisation. RPC_E_CHANGED_MODE (an STA thread) is
    // fine: the frame pool is free-threaded and works from either apartment.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
}

impl WgcCapturer {
    pub fn new(
        device: D3dDevice,
        target: WgcTarget,
        options: WgcOptions,
    ) -> Result<Self, CaptureError> {
        init_winrt();
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err(CaptureError::Unsupported(
                "Windows.Graphics.Capture needs Windows 10 1903 or later".into(),
            ));
        }
        let item = create_item(&target)?;
        let winrt_device = winrt_device(&device)?;
        let size = item.Size()?;
        let width = even(size.Width);
        let height = even(size.Height);
        if width == 0 || height == 0 {
            return Err(CaptureError::Unsupported(
                "capture target has zero size (minimised?)".into(),
            ));
        }
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        let applied = configure(&session, &target, options);

        let signal = Arc::new(Signal::default());
        let closed = Arc::new(AtomicBool::new(false));
        let frame_token = {
            let signal = signal.clone();
            pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
                signal.notify();
                Ok(())
            }))?
        };
        let closed_token = {
            let signal = signal.clone();
            let closed = closed.clone();
            item.Closed(&TypedEventHandler::new(move |_, _| {
                closed.store(true, Ordering::SeqCst);
                signal.notify();
                Ok(())
            }))?
        };

        let frame = device.create_texture(
            width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
            D3D11_CPU_ACCESS_FLAG(0),
        )?;
        session.StartCapture()?;
        tracing::debug!(?target, width, height, ?applied, "WGC capture started");
        Ok(Self {
            device,
            target,
            winrt_device,
            session,
            pool,
            item,
            frame_token,
            closed_token,
            signal,
            closed,
            pool_size: size,
            width,
            height,
            content: (
                size.Width.max(0).cast_unsigned(),
                size.Height.max(0).cast_unsigned(),
            ),
            frame,
            rtv: None,
            staging: None,
            cursor: CursorTracker::default(),
            applied,
        })
    }

    pub fn target(&self) -> &WgcTarget {
        &self.target
    }

    /// Size of [`texture`](Self::texture); fixed for the capturer's lifetime.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Size of the most recent frame's content, which differs from [`size`](Self::size) after
    /// the window was resized.
    pub fn content_size(&self) -> (u32, u32) {
        self.content
    }

    pub fn applied(&self) -> WgcApplied {
        self.applied
    }

    /// True when the captured window is minimised, which is why no frames arrive.
    pub fn minimized(&self) -> bool {
        matches!(self.target, WgcTarget::Window(id) if toplevel::is_minimized(id))
    }

    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.frame
    }

    /// Waits up to `timeout` for DWM to deliver a frame, then copies the newest one into
    /// [`texture`](Self::texture). WGC only produces frames when the content changes, so a
    /// still window times out.
    pub fn acquire(&mut self, timeout: Duration) -> Result<FrameMeta, CaptureError> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return Err(CaptureError::Closed);
            }
            let mut latest = None;
            let mut accumulated = 0u32;
            while let Some(frame) = next_frame(&self.pool)? {
                accumulated += 1;
                if let Some(old) = latest.replace(frame) {
                    let _ = old.Close();
                }
            }
            if let Some(frame) = latest {
                let result = self.take(&frame, accumulated);
                let _ = frame.Close();
                return result;
            }
            let now = Instant::now();
            if now >= deadline {
                // `GraphicsCaptureItem.Closed` does not reliably fire for a closed window (seen
                // on Windows 11 26H2 with a free-threaded pool), so check the handle as well.
                if matches!(self.target, WgcTarget::Window(id) if !toplevel::exists(id)) {
                    return Err(CaptureError::Closed);
                }
                return Err(CaptureError::Timeout);
            }
            self.signal.wait(deadline - now);
        }
    }

    fn take(
        &mut self,
        frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
        accumulated: u32,
    ) -> Result<FrameMeta, CaptureError> {
        let captured_at = Instant::now();
        let content = frame.ContentSize()?;
        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        // SAFETY: the frame's surface is a D3D11 texture on our device.
        let texture: ID3D11Texture2D = unsafe { access.GetInterface() }?;

        let content_wh = (
            content.Width.max(0).cast_unsigned(),
            content.Height.max(0).cast_unsigned(),
        );
        if content_wh != self.content {
            tracing::debug!(from = ?self.content, to = ?content_wh, "WGC content size changed");
            if content_wh.0 < self.width || content_wh.1 < self.height {
                self.clear()?;
            }
            self.content = content_wh;
        }
        let copy_w = content_wh
            .0
            .min(self.width)
            .min(self.pool_size.Width.max(0).cast_unsigned());
        let copy_h = content_wh
            .1
            .min(self.height)
            .min(self.pool_size.Height.max(0).cast_unsigned());
        if copy_w > 0 && copy_h > 0 {
            let region = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: copy_w,
                bottom: copy_h,
                back: 1,
            };
            // SAFETY: both textures are BGRA8 on the same device and the box lies inside both.
            unsafe {
                self.device.context.CopySubresourceRegion(
                    &self.frame,
                    0,
                    0,
                    0,
                    0,
                    &texture,
                    0,
                    Some(&region),
                );
            }
        }
        if content.Width != self.pool_size.Width || content.Height != self.pool_size.Height {
            // Frames are clipped to the pool size until it is recreated at the new size.
            self.pool.Recreate(
                &self.winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                content,
            )?;
            self.pool_size = content;
        }

        let (origin_x, origin_y) = self.origin();
        Ok(FrameMeta {
            captured_at: Some(captured_at),
            desktop_updated: true,
            accumulated_frames: accumulated,
            dirty: Vec::new(),
            moved: Vec::new(),
            cursor: cursor_state(origin_x, origin_y, &mut self.cursor),
        })
    }

    /// Desktop position of the texture's top-left pixel, for cursor coordinates.
    fn origin(&self) -> (i32, i32) {
        match &self.target {
            WgcTarget::Monitor(m) => (m.bounds.x, m.bounds.y),
            WgcTarget::Window(id) => {
                toplevel::window_bounds(toplevel::hwnd_from_id(*id)).map_or((0, 0), |b| (b.x, b.y))
            }
        }
    }

    fn clear(&mut self) -> Result<(), CaptureError> {
        if self.rtv.is_none() {
            let mut rtv = None;
            // SAFETY: the frame texture was created with D3D11_BIND_RENDER_TARGET.
            unsafe {
                self.device
                    .device
                    .CreateRenderTargetView(&self.frame, None, Some(&mut rtv))?;
            }
            self.rtv = rtv;
        }
        if let Some(rtv) = &self.rtv {
            // SAFETY: valid view of our own texture.
            unsafe {
                self.device
                    .context
                    .ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 1.0]);
            }
        }
        Ok(())
    }

    /// Copies the current texture into `out`, reallocating as needed.
    pub fn read_cpu(&mut self, out: &mut CpuFrame) -> Result<(), CaptureError> {
        if self.staging.is_none() {
            self.staging = Some(self.device.create_texture(
                self.width,
                self.height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_USAGE_STAGING,
                D3D11_BIND_FLAG(0),
                D3D11_CPU_ACCESS_READ,
            )?);
        }
        let staging = self.staging.as_ref().expect("staging texture just created");
        let ctx = &self.device.context;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging matches frame in size and format; Map/Unmap are paired.
        unsafe {
            ctx.CopyResource(staging, &self.frame);
            ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let row_bytes = self.width as usize * 4;
        let src_pitch = mapped.RowPitch as usize;
        out.width = self.width;
        out.height = self.height;
        out.stride = row_bytes;
        out.bgra.resize(row_bytes * self.height as usize, 0);
        // SAFETY: the mapped region is RowPitch * height bytes and stays valid until Unmap.
        unsafe {
            let src = std::slice::from_raw_parts(
                mapped.pData.cast::<u8>(),
                src_pitch * self.height as usize,
            );
            for (y, dst_row) in out.bgra.chunks_exact_mut(row_bytes).enumerate() {
                let start = y * src_pitch;
                dst_row.copy_from_slice(&src[start..start + row_bytes]);
            }
            ctx.Unmap(staging, 0);
        }
        Ok(())
    }
}

impl Drop for WgcCapturer {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.frame_token);
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn create_item(target: &WgcTarget) -> Result<GraphicsCaptureItem, CaptureError> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    match target {
        WgcTarget::Monitor(m) => {
            let centre = POINT {
                x: m.bounds.x + i32::try_from(m.bounds.width / 2).unwrap_or(0),
                y: m.bounds.y + i32::try_from(m.bounds.height / 2).unwrap_or(0),
            };
            // SAFETY: plain lookup; MONITOR_DEFAULTTONULL returns null for a stale layout.
            let hmonitor = unsafe { MonitorFromPoint(centre, MONITOR_DEFAULTTONULL) };
            if hmonitor.is_invalid() {
                return Err(CaptureError::MonitorNotFound(m.id));
            }
            // SAFETY: valid HMONITOR.
            Ok(unsafe { interop.CreateForMonitor(hmonitor) }?)
        }
        WgcTarget::Window(id) => {
            let hwnd = toplevel::hwnd_from_id(*id);
            // SAFETY: CreateForWindow validates the handle and fails for a stale one.
            unsafe { interop.CreateForWindow(hwnd) }.map_err(|err| {
                if toplevel::describe_window(*id).is_none() {
                    CaptureError::Closed
                } else {
                    err.into()
                }
            })
        }
    }
}

fn winrt_device(device: &D3dDevice) -> Result<IDirect3DDevice, CaptureError> {
    let dxgi: IDXGIDevice = device.device.cast()?;
    // SAFETY: wraps our live DXGI device in a WinRT object that holds its own reference.
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?;
    Ok(inspectable.cast()?)
}

fn configure(
    session: &GraphicsCaptureSession,
    target: &WgcTarget,
    options: WgcOptions,
) -> WgcApplied {
    let mut applied = WgcApplied::default();
    // The viewer draws the cursor itself from out-of-band updates.
    if let Err(err) = session.SetIsCursorCaptureEnabled(false) {
        tracing::debug!(%err, "could not disable WGC cursor capture");
    }
    if options.borderless {
        let granted =
            GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
                .and_then(|op| op.join());
        if let Err(err) = &granted {
            tracing::debug!(%err, "borderless capture access request failed");
        }
        match session.SetIsBorderRequired(false) {
            Ok(()) => applied.borderless = session.IsBorderRequired().is_ok_and(|b| !b),
            Err(err) => tracing::debug!(%err, "could not disable the WGC border"),
        }
    }
    if options.include_secondary_windows && matches!(target, WgcTarget::Window(_)) {
        match session.SetIncludeSecondaryWindows(true) {
            Ok(()) => applied.secondary_windows = true,
            Err(err) => tracing::debug!(%err, "IncludeSecondaryWindows unavailable"),
        }
    }
    if let Some(interval) = options.min_interval {
        let ticks = i64::try_from(interval.as_nanos() / 100).unwrap_or(i64::MAX);
        match session.SetMinUpdateInterval(TimeSpan { Duration: ticks }) {
            Ok(()) => applied.min_interval = true,
            Err(err) => tracing::debug!(%err, "MinUpdateInterval unavailable"),
        }
    }
    applied
}

/// The next queued frame, or `None` when the pool is empty (WinRT returns null, which
/// windows-rs surfaces as an error carrying `S_OK`).
fn next_frame(
    pool: &Direct3D11CaptureFramePool,
) -> Result<Option<windows::Graphics::Capture::Direct3D11CaptureFrame>, CaptureError> {
    match pool.TryGetNextFrame() {
        Ok(frame) => Ok(Some(frame)),
        Err(err) if err.code() == HRESULT(0) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Rounds down to an even, non-negative size; H.264 needs even dimensions.
fn even(value: i32) -> u32 {
    value.max(0).cast_unsigned() & !1
}
