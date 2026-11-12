//! One video stream: a capture-and-encode thread per monitor, feeding a QUIC stream.
//!
//! The thread captures at the configured rate, encodes on the GPU and hands finished frames
//! to a small queue. A tokio task drains the queue into a unidirectional QUIC stream. If the
//! queue is full the network is behind, so the thread *skips the capture* (never drops an
//! encoded frame, which would break the reference chain) and the rate controller lowers the
//! bitrate. When the queue has been empty for a while the bitrate creeps back up.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use peet_capture::windows::{Backend, D3dDevice, FrameSource};
use peet_capture::{CaptureError, CursorState, MonitorDesc};
use peet_codec::windows::MfH264Encoder;
use peet_codec::{EncoderConfig, VideoEncoder};
use peet_proto::{frame, Codec, ControlMessage, CursorUpdate, StreamKind, VideoFrame};
use peet_transport::quinn;
use tokio::sync::mpsc;

/// Frames waiting for the network before the encoder starts skipping captures.
const QUEUE_DEPTH: usize = 3;

#[derive(Debug, Clone)]
pub struct StreamParams {
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub backend: Backend,
    /// Follow the active input desktop (service worker mode) so capture survives a desktop
    /// switch to the secure desktop.
    pub follow_desktop: bool,
}

/// Handle to a running stream. Dropping it stops the thread and finishes the QUIC stream.
pub struct MonitorStream {
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub params: StreamParams,
}

impl std::fmt::Debug for MonitorStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitorStream")
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

impl MonitorStream {
    pub fn start(
        connection: quinn::Connection,
        monitor: MonitorDesc,
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

        let worker = Worker {
            monitor: monitor.clone(),
            params: params.clone(),
            control,
            frame_tx,
            stop: stop.clone(),
            keyframe: keyframe.clone(),
            session_start,
        };
        let thread = std::thread::Builder::new()
            .name(format!("stream-{}", monitor.id.0))
            .spawn(move || {
                if let Err(err) = worker.run() {
                    tracing::error!(monitor = %monitor.name, %err, "stream thread failed");
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
}

impl Drop for MonitorStream {
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

struct Worker {
    monitor: MonitorDesc,
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
        let device = D3dDevice::for_adapter(self.monitor.adapter_index)?;
        let mut source = FrameSource::new(
            device.clone(),
            &self.monitor,
            self.params.backend,
            self.params.fps,
        )?;
        let (width, height) = source.size();
        let enc_config = EncoderConfig {
            width,
            height,
            fps: self.params.fps,
            bitrate_kbps: self.params.bitrate_kbps,
            keyframe_interval: self.params.fps * 4,
        };
        let d3d = Some((&device.device, &device.context));
        let mut encoder = match MfH264Encoder::new(&enc_config, true, d3d) {
            Ok(enc) if enc.accepts_textures() => enc,
            Ok(_) | Err(_) => {
                tracing::warn!(monitor = %self.monitor.name, "no hardware encoder; using software MFT");
                MfH264Encoder::new(&enc_config, false, d3d).context("creating software encoder")?
            }
        };
        tracing::info!(
            monitor = %self.monitor.name, capture = source.name(), encoder = encoder.name(),
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

        while !self.stop.load(Ordering::Relaxed) {
            source.wait_for_slot();
            let meta = match source.acquire(Duration::from_millis(100)) {
                Ok(meta) => meta,
                Err(CaptureError::Timeout | CaptureError::AccessLost) => continue,
                Err(err) => return Err(err.into()),
            };
            if let Some(update) = cursor.update(meta.cursor.as_ref(), &self.monitor) {
                let _ = self.control.try_send(ControlMessage::Cursor(update));
            }
            if !meta.desktop_updated {
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
            let packets = encoder.encode_texture(source.texture(), pts_us)?;
            for packet in packets {
                seq += 1;
                stats.frames += 1;
                stats.bytes += packet.data.len() as u64;
                let frame = VideoFrame {
                    monitor: self.monitor.id,
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
                tracing::debug!(monitor = %self.monitor.name, kbps, "bitrate adjusted");
                encoder.set_bitrate(kbps);
            }
            if last_stats.elapsed() >= Duration::from_secs(1) {
                let secs = last_stats.elapsed().as_secs_f64();
                let _ = self.control.try_send(ControlMessage::StreamStats {
                    monitor: self.monitor.id,
                    fps: (f64::from(stats.frames) / secs).round() as u16,
                    bitrate_kbps: (stats.bytes as f64 * 8.0 / 1000.0 / secs) as u32,
                    skipped: stats.skipped,
                });
                stats = PeriodStats::default();
                last_stats = Instant::now();
            }
        }
        tracing::info!(monitor = %self.monitor.name, "stream stopped");
        Ok(())
    }
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
    fn update(
        &mut self,
        state: Option<&CursorState>,
        monitor: &MonitorDesc,
    ) -> Option<CursorUpdate> {
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
            x: monitor.bounds.x + state.x,
            y: monitor.bounds.y + state.y,
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
