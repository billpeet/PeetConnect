//! One video stream: a capture-and-encode thread per source (monitor or window), feeding a
//! QUIC stream.
//!
//! The thread captures at the configured rate, encodes on the GPU and hands finished frames
//! to a small queue. A tokio task drains the queue into a unidirectional QUIC stream. If the
//! queue is full the network is behind, so the thread *skips the capture* (never drops an
//! encoded frame, which would break the reference chain) and the rate controller lowers the
//! bitrate. When the queue has been empty for a while the bitrate creeps back up.
//!
//! Window streams use Windows.Graphics.Capture. The thread also watches the window: it reports
//! moves, minimise and restore to the viewer, rebuilds capture and encoder at the new size once
//! a resize settles, and ends the stream when the window closes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use peet_capture::windows::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use peet_capture::windows::{
    describe_window, enumerate_monitors, Backend, D3dDevice, FrameSource, WgcCapturer, WgcOptions,
    WgcTarget,
};
use peet_capture::{CaptureError, CursorState, FrameMeta, MonitorDesc};
use peet_codec::windows::MfH264Encoder;
use peet_codec::{EncoderConfig, VideoEncoder};
use peet_proto::{
    frame, Codec, ControlMessage, CursorUpdate, Rect, SourceId, StreamKind, VideoFrame,
    WindowChange, WindowId,
};
use peet_transport::quinn;
use tokio::sync::mpsc;

/// Frames waiting for the network before the encoder starts skipping captures.
const QUEUE_DEPTH: usize = 3;

/// How long a window's new size must hold before capture and encoder are rebuilt for it.
const RESIZE_SETTLE: Duration = Duration::from_millis(300);

/// How often a window stream checks the window's position and state.
const WINDOW_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct StreamParams {
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub backend: Backend,
    /// Follow the active input desktop (service worker mode) so capture survives a desktop
    /// switch to the secure desktop.
    pub follow_desktop: bool,
}

/// What a stream captures.
#[derive(Debug, Clone)]
pub enum Source {
    Monitor(MonitorDesc),
    Window(WindowId),
}

impl Source {
    fn id(&self) -> SourceId {
        match self {
            Source::Monitor(m) => SourceId::Monitor(m.id),
            Source::Window(w) => SourceId::Window(*w),
        }
    }
}

/// Handle to a running stream. Dropping it stops the thread and finishes the QUIC stream.
pub struct VideoStream {
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub params: StreamParams,
}

impl std::fmt::Debug for VideoStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoStream")
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

impl VideoStream {
    pub fn start(
        connection: quinn::Connection,
        source: Source,
        params: StreamParams,
        control: mpsc::Sender<ControlMessage>,
        session_start: Instant,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let keyframe = Arc::new(AtomicBool::new(false));
        let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>(QUEUE_DEPTH);
        let runtime = tokio::runtime::Handle::current();
        let stop_net = stop.clone();
        runtime.spawn(sender(connection, frame_rx, stop_net));

        let id = source.id();
        let worker = Worker {
            source,
            params: params.clone(),
            control,
            frame_tx,
            stop: stop.clone(),
            keyframe: keyframe.clone(),
            session_start,
        };
        let thread = std::thread::Builder::new()
            .name(format!("stream-{id}"))
            .spawn(move || {
                if let Err(err) = worker.run() {
                    tracing::error!(source = %id, "stream thread failed: {err:#}");
                }
            })
            .expect("spawn stream thread");
        Self {
            stop,
            keyframe,
            thread: Some(thread),
            params,
        }
    }

    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    /// True once the capture thread has exited (window closed, capture failed, network gone).
    pub fn finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Drains encoded frames into one unidirectional QUIC stream.
async fn sender(
    connection: quinn::Connection,
    mut frames: mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
) {
    let mut stream = match peet_transport::open_uni(&connection, StreamKind::Video).await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(%err, "could not open video stream");
            stop.store(true, Ordering::Relaxed);
            return;
        }
    };
    while let Some(bytes) = frames.recv().await {
        if let Err(err) = stream.send_encoded(&bytes).await {
            tracing::debug!(%err, "video stream write failed");
            stop.store(true, Ordering::Relaxed);
            break;
        }
    }
    let _ = stream.finish();
}

/// The capture side of a stream.
enum Capture {
    Monitor(FrameSource),
    Window(WgcCapturer),
}

impl Capture {
    fn name(&self) -> &'static str {
        match self {
            Capture::Monitor(s) => s.name(),
            Capture::Window(_) => "wgc-window",
        }
    }

    fn size(&self) -> (u32, u32) {
        match self {
            Capture::Monitor(s) => s.size(),
            Capture::Window(c) => c.size(),
        }
    }

    fn wait_for_slot(&mut self) {
        if let Capture::Monitor(s) = self {
            s.wait_for_slot();
        }
    }

    fn acquire(&mut self, timeout: Duration) -> Result<FrameMeta, CaptureError> {
        match self {
            Capture::Monitor(s) => s.acquire(timeout),
            Capture::Window(c) => c.acquire(timeout),
        }
    }

    fn texture(&self) -> &ID3D11Texture2D {
        match self {
            Capture::Monitor(s) => s.texture(),
            Capture::Window(c) => c.texture(),
        }
    }
}

/// Window position and state as last reported to the viewer.
struct WindowWatch {
    id: WindowId,
    bounds: Rect,
    minimized: bool,
    last_poll: Instant,
    /// When the capture content first differed from the encoded size, for resize debounce.
    resize_seen: Option<Instant>,
}

struct Worker {
    source: Source,
    params: StreamParams,
    control: mpsc::Sender<ControlMessage>,
    frame_tx: mpsc::Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    session_start: Instant,
}

impl Worker {
    fn run(self) -> Result<()> {
        // In service worker mode, sit on the active input desktop so capture keeps producing
        // frames across a switch to the secure desktop. Held for the thread's lifetime.
        let _desktop = self
            .params
            .follow_desktop
            .then(crate::desktop::attach_input_desktop)
            .flatten();
        let id = self.source.id();
        let (device, mut capture, mut watch) = match &self.source {
            Source::Monitor(monitor) => {
                let device = D3dDevice::for_adapter(monitor.adapter_index)?;
                let backend = match self.params.backend {
                    Backend::Auto if self.params.follow_desktop => Backend::AutoNoWgc,
                    other => other,
                };
                let source = FrameSource::new(device.clone(), monitor, backend, self.params.fps)?;
                (device, Capture::Monitor(source), None)
            }
            Source::Window(window) => {
                let Some(desc) = describe_window(window.0) else {
                    self.window_changed(*window, WindowChange::Closed);
                    return Ok(());
                };
                let device = D3dDevice::for_adapter(adapter_for(desc.bounds))?;
                let capture = match self.window_capture(&device, *window) {
                    Ok(c) => c,
                    Err(CaptureError::Closed) => {
                        self.window_changed(*window, WindowChange::Closed);
                        return Ok(());
                    }
                    Err(err) => return Err(err).context("starting window capture"),
                };
                let watch = WindowWatch {
                    id: *window,
                    bounds: desc.bounds,
                    minimized: desc.minimized,
                    last_poll: Instant::now(),
                    resize_seen: None,
                };
                (device, capture, Some(watch))
            }
        };
        let (mut width, mut height) = capture.size();
        let mut encoder = self.encoder(&device, width, height)?;
        tracing::info!(
            source = %id, capture = capture.name(), encoder = encoder.name(),
            width, height, fps = self.params.fps, kbps = self.params.bitrate_kbps, "stream started"
        );

        let mut rate = RateController::new(self.params.bitrate_kbps);
        let mut cursor = CursorTracker::default();
        let mut seq: u64 = 0;
        let mut stats = PeriodStats::default();
        let mut last_stats = Instant::now();
        // First frame must be an IDR frame; the encoder does that by itself, but a viewer
        // that joins an existing stream later asks explicitly.
        encoder.request_keyframe();

        // True once the capture texture holds a real frame. DXGI and WGC only deliver frames
        // when something changes, so on a still screen a keyframe request re-encodes this
        // texture instead of waiting for the next change.
        let mut have_frame = false;
        while !self.stop.load(Ordering::Relaxed) {
            if let (Some(w), Capture::Window(cap)) = (&mut watch, &capture) {
                if self.poll_window(w, cap.content_size(), (width, height)) {
                    // The resize settled: rebuild at the new size. The new encoder starts
                    // with an IDR frame at the new dimensions.
                    capture = match self.window_capture(&device, w.id) {
                        Ok(c) => c,
                        Err(CaptureError::Closed) => {
                            self.window_changed(w.id, WindowChange::Closed);
                            break;
                        }
                        Err(err) => return Err(err).context("restarting window capture"),
                    };
                    (width, height) = capture.size();
                    encoder = self.encoder(&device, width, height)?;
                    encoder.request_keyframe();
                    rate = RateController::new(self.params.bitrate_kbps);
                    have_frame = false;
                    tracing::info!(source = %id, width, height, "window resized; stream rebuilt");
                }
            }
            capture.wait_for_slot();
            let fresh = match capture.acquire(Duration::from_millis(100)) {
                Ok(meta) => {
                    let origin = match (&self.source, &watch) {
                        (Source::Monitor(m), _) => (m.bounds.x, m.bounds.y),
                        (Source::Window(_), Some(w)) => (w.bounds.x, w.bounds.y),
                        (Source::Window(_), None) => (0, 0),
                    };
                    if let Some(update) = cursor.update(meta.cursor.as_ref(), origin) {
                        let _ = self.control.try_send(ControlMessage::Cursor(update));
                    }
                    meta.desktop_updated
                }
                Err(CaptureError::Timeout) => false,
                Err(CaptureError::AccessLost) => {
                    // The source was recreated with a new, empty texture.
                    have_frame = false;
                    continue;
                }
                Err(CaptureError::Closed) => {
                    if let Some(w) = &watch {
                        self.window_changed(w.id, WindowChange::Closed);
                    }
                    break;
                }
                Err(err) => return Err(err.into()),
            };
            have_frame |= fresh;
            let repeat = !fresh && have_frame && self.keyframe.load(Ordering::Relaxed);
            if !fresh && !repeat {
                continue;
            }
            if self.frame_tx.capacity() == 0 {
                // Network is behind: skip this capture rather than queue more.
                stats.skipped += 1;
                rate.on_skip();
                if let Some(kbps) = rate.update() {
                    encoder.set_bitrate(kbps);
                }
                continue;
            }
            if self.keyframe.swap(false, Ordering::Relaxed) {
                encoder.request_keyframe();
            }
            let pts_us = u64::try_from(self.session_start.elapsed().as_micros()).unwrap_or(0);
            let packets = encoder.encode_texture(capture.texture(), pts_us)?;
            for packet in packets {
                seq += 1;
                stats.frames += 1;
                stats.bytes += packet.data.len() as u64;
                let frame = VideoFrame {
                    source: id,
                    seq,
                    pts_us: packet.pts_us,
                    keyframe: packet.keyframe,
                    codec: Codec::H264,
                    width,
                    height,
                    dirty: Vec::new(),
                    data: packet.data,
                };
                let bytes = frame::encode(&frame)?;
                if self.frame_tx.blocking_send(bytes).is_err() {
                    return Ok(());
                }
            }
            if let Some(kbps) = rate.update() {
                tracing::debug!(source = %id, kbps, "bitrate adjusted");
                encoder.set_bitrate(kbps);
            }
            if last_stats.elapsed() >= Duration::from_secs(1) {
                let secs = last_stats.elapsed().as_secs_f64();
                let _ = self.control.try_send(ControlMessage::StreamStats {
                    source: id,
                    fps: (f64::from(stats.frames) / secs).round() as u16,
                    bitrate_kbps: (stats.bytes as f64 * 8.0 / 1000.0 / secs) as u32,
                    skipped: stats.skipped,
                });
                stats = PeriodStats::default();
                last_stats = Instant::now();
            }
        }
        tracing::info!(source = %id, "stream stopped");
        Ok(())
    }

    fn encoder(&self, device: &D3dDevice, width: u32, height: u32) -> Result<MfH264Encoder> {
        let enc_config = EncoderConfig {
            width,
            height,
            fps: self.params.fps,
            bitrate_kbps: self.params.bitrate_kbps,
            keyframe_interval: self.params.fps * 4,
        };
        let d3d = Some((&device.device, &device.context));
        match MfH264Encoder::new(&enc_config, true, d3d) {
            Ok(enc) if enc.accepts_textures() => Ok(enc),
            Ok(_) | Err(_) => {
                tracing::warn!(source = %self.source.id(), "no hardware encoder; using software MFT");
                // Microsoft's software MFT ships with every Windows edition except N, where
                // Media Foundation itself is an optional feature.
                MfH264Encoder::new(&enc_config, false, d3d).context(
                    "no H.264 encoder available (on Windows N editions, install the Media \
                     Feature Pack: Settings > Apps > Optional features)",
                )
            }
        }
    }

    fn window_capture(
        &self,
        device: &D3dDevice,
        window: WindowId,
    ) -> Result<Capture, CaptureError> {
        let options = WgcOptions {
            min_interval: Some(Duration::from_secs(1) / self.params.fps.max(1)),
            ..WgcOptions::default()
        };
        Ok(Capture::Window(WgcCapturer::new(
            device.clone(),
            WgcTarget::Window(window.0),
            options,
        )?))
    }

    fn window_changed(&self, window: WindowId, change: WindowChange) {
        tracing::debug!(window = window.0, ?change, "window changed");
        let _ = self
            .control
            .try_send(ControlMessage::WindowChanged { window, change });
    }

    /// Reports moves and minimise/restore to the viewer, and returns true when the capture
    /// content has had a different size from the encoded one for [`RESIZE_SETTLE`].
    fn poll_window(&self, w: &mut WindowWatch, content: (u32, u32), encoded: (u32, u32)) -> bool {
        if w.last_poll.elapsed() < WINDOW_POLL {
            return false;
        }
        w.last_poll = Instant::now();
        let Some(desc) = describe_window(w.id.0) else {
            // Gone; the capturer reports Closed on the next acquire.
            return false;
        };
        if desc.minimized != w.minimized {
            w.minimized = desc.minimized;
            self.window_changed(
                w.id,
                if desc.minimized {
                    WindowChange::Minimized
                } else {
                    WindowChange::Restored
                },
            );
        }
        // A minimised window is parked off-screen at a token size; keep the last real bounds.
        if !desc.minimized && desc.bounds != w.bounds {
            w.bounds = desc.bounds;
            self.window_changed(w.id, WindowChange::Bounds(desc.bounds));
        }
        // The texture is the content size rounded down to even numbers.
        let target = (content.0 & !1, content.1 & !1);
        if desc.minimized || target == encoded || target.0 == 0 || target.1 == 0 {
            w.resize_seen = None;
            return false;
        }
        let seen = *w.resize_seen.get_or_insert_with(Instant::now);
        if seen.elapsed() >= RESIZE_SETTLE {
            w.resize_seen = None;
            return true;
        }
        false
    }
}

/// The graphics adapter driving the monitor under the centre of `bounds`; adapter 0 if none.
fn adapter_for(bounds: Rect) -> u32 {
    let cx = bounds.x + i32::try_from(bounds.width / 2).unwrap_or(0);
    let cy = bounds.y + i32::try_from(bounds.height / 2).unwrap_or(0);
    enumerate_monitors()
        .ok()
        .and_then(|monitors| {
            monitors.into_iter().find(|m| {
                let b = m.bounds;
                cx >= b.x
                    && cy >= b.y
                    && i64::from(cx) < i64::from(b.x) + i64::from(b.width)
                    && i64::from(cy) < i64::from(b.y) + i64::from(b.height)
            })
        })
        .map_or(0, |m| m.adapter_index)
}

#[derive(Default)]
struct PeriodStats {
    frames: u32,
    bytes: u64,
    skipped: u32,
}

/// Tracks cursor position and shape so only changes are sent.
#[derive(Default)]
struct CursorTracker {
    last: Option<(i32, i32, bool)>,
}

impl CursorTracker {
    /// `origin` is the desktop position of the capture's top-left pixel, which the backend's
    /// cursor coordinates are relative to.
    fn update(&mut self, state: Option<&CursorState>, origin: (i32, i32)) -> Option<CursorUpdate> {
        let state = state?;
        let key = (state.x, state.y, state.visible);
        let shape = state
            .shape
            .as_ref()
            .map(peet_capture::CursorShapeData::to_proto);
        if shape.is_none() && self.last == Some(key) {
            return None;
        }
        self.last = Some(key);
        Some(CursorUpdate {
            x: origin.0 + state.x,
            y: origin.1 + state.y,
            visible: state.visible,
            shape,
        })
    }
}

/// Additive-increase, multiplicative-decrease bitrate control driven by queue pressure.
struct RateController {
    target: u32,
    floor: u32,
    current: u32,
    window_start: Instant,
    skips: u32,
    quiet_windows: u32,
    last_change: Instant,
}

impl RateController {
    const WINDOW: Duration = Duration::from_millis(500);

    fn new(target_kbps: u32) -> Self {
        Self {
            target: target_kbps,
            floor: (target_kbps / 16).max(300),
            current: target_kbps,
            window_start: Instant::now(),
            skips: 0,
            quiet_windows: 0,
            last_change: Instant::now(),
        }
    }

    fn on_skip(&mut self) {
        self.skips += 1;
    }

    /// Call once per loop iteration; returns a new bitrate when it should change.
    fn update(&mut self) -> Option<u32> {
        if self.window_start.elapsed() < Self::WINDOW {
            return None;
        }
        self.window_start = Instant::now();
        let skips = std::mem::take(&mut self.skips);
        if skips >= 2 {
            self.quiet_windows = 0;
            if self.last_change.elapsed() >= Duration::from_secs(1) && self.current > self.floor {
                self.current = ((f64::from(self.current) * 0.7) as u32).max(self.floor);
                self.last_change = Instant::now();
                return Some(self.current);
            }
        } else if skips == 0 {
            self.quiet_windows += 1;
            if self.quiet_windows >= 4 && self.current < self.target {
                self.quiet_windows = 0;
                self.current = ((f64::from(self.current) * 1.2) as u32).min(self.target);
                self.last_change = Instant::now();
                return Some(self.current);
            }
        }
        None
    }
}
