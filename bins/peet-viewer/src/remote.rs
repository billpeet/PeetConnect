//! Remote session: network thread, decode thread, and the state they share with the UI.
//!
//! The network thread runs a tokio runtime with the `peet-core` session and forwards video
//! frames to the decode thread. The decode thread owns the D3D device, the H.264 decoder and
//! the DirectComposition presenter. The UI thread only reads [`RemoteShared`] and sends
//! control messages through the session's [`Control`] handle.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gpui::RenderImage;
use image::{Frame, ImageBuffer};
use peet_capture::windows::D3dDevice;
use peet_codec::windows::MfH264Decoder;
use peet_core::{ConnectOptions, Control, Event};
use peet_proto::{ControlMessage, CursorShape, MonitorId, MonitorInfo, VideoFrame};
use smallvec::SmallVec;
use tokio::sync::mpsc;
use windows::Win32::Foundation::HWND;

use crate::present::{Presenter, SharedPlacement};

/// Everything the UI shows about the session.
#[derive(Debug, Default, Clone)]
pub struct RemoteStatus {
    pub host_name: String,
    pub monitors: Vec<MonitorInfo>,
    pub current: Option<MonitorId>,
    pub frame_size: (u32, u32),
    pub fps: f64,
    pub decode_ms: f64,
    pub host_fps: u16,
    pub kbps: u32,
    pub skipped: u32,
    pub rtt_ms: Option<f64>,
    pub connected: bool,
    pub message: String,
}

/// Remote cursor, drawn by GPUI over the video.
#[derive(Default)]
pub struct CursorOverlay {
    /// Position in host virtual desktop coordinates.
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub shape: Option<Arc<RenderImage>>,
    pub size: (u32, u32),
    pub hotspot: (u32, u32),
    /// Previous shape, for the UI thread to release.
    pub retired: Option<Arc<RenderImage>>,
}

#[derive(Default)]
pub struct RemoteShared {
    pub status: Mutex<RemoteStatus>,
    pub cursor: Mutex<CursorOverlay>,
    pub control: Mutex<Option<Control>>,
}

impl RemoteShared {
    pub fn status(&self) -> RemoteStatus {
        self.status.lock().expect("status poisoned").clone()
    }

    /// Switches the video stream to another monitor.
    pub fn select_monitor(&self, monitor: MonitorId) {
        let control = self.control.lock().expect("control poisoned").clone();
        let Some(control) = control else { return };
        let previous = {
            let mut status = self.status.lock().expect("status poisoned");
            if status.current == Some(monitor) {
                return;
            }
            status.current.replace(monitor)
        };
        if let Some(previous) = previous {
            control.try_send(ControlMessage::SetStream {
                monitor: previous,
                enabled: false,
                max_fps: None,
                max_bitrate_kbps: None,
            });
        }
        control.try_send(ControlMessage::SetStream {
            monitor,
            enabled: true,
            max_fps: None,
            max_bitrate_kbps: None,
        });
    }
}

pub struct Config {
    pub session: SessionConfig,
    pub window: Receiver<isize>,
    pub placement: SharedPlacement,
    pub vsync: bool,
    pub shared: Arc<RemoteShared>,
}

pub struct SessionConfig {
    pub target: String,
    pub pin: Option<String>,
    pub monitor: Option<u32>,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

/// Connects and runs until the session ends. Blocks the calling thread.
pub fn run(config: Config) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let (frame_tx, frame_rx) = mpsc::channel::<VideoFrame>(32);
    let Config {
        session: session_config,
        window,
        placement,
        vsync,
        shared,
    } = config;
    let shared_for_decode = shared.clone();
    let decode_thread = std::thread::Builder::new()
        .name("decode".into())
        .spawn(move || {
            let shared = shared_for_decode;
            if let Err(err) = decode_loop(frame_rx, &window, placement, vsync, &shared) {
                tracing::error!(%err, "decode thread stopped");
                shared.status.lock().expect("status poisoned").message = err.to_string();
            }
        })?;
    let result = runtime.block_on(session(session_config, shared, frame_tx));
    drop(runtime);
    let _ = decode_thread.join();
    result
}

async fn session(
    config: SessionConfig,
    shared: Arc<RemoteShared>,
    frames: mpsc::Sender<VideoFrame>,
) -> Result<()> {
    set_message(&shared, format!("resolving {}", config.target));
    let addr = peet_core::resolve(&config.target).await?;
    set_message(&shared, format!("connecting to {addr}"));
    let opts = ConnectOptions {
        pin: config.pin,
        ..ConnectOptions::default()
    };
    let session = peet_core::connect(addr, opts)
        .await
        .with_context(|| format!("connecting to {addr}"))?;
    let (info, control, mut events, guard) = session.split();

    let monitor = info
        .monitors
        .iter()
        .find(|m| {
            config.monitor.is_none_or(|id| m.id.0 == id) && (config.monitor.is_some() || m.primary)
        })
        .or(info.monitors.first())
        .map(|m| m.id)
        .context("host reports no monitors")?;
    {
        let mut status = shared.status.lock().expect("status poisoned");
        status.host_name.clone_from(&info.host_name);
        status.monitors.clone_from(&info.monitors);
        status.current = Some(monitor);
        status.connected = true;
        status.message = String::new();
    }
    *shared.control.lock().expect("control poisoned") = Some(control.clone());
    control
        .set_stream(
            monitor,
            true,
            Some(u16::try_from(config.fps).unwrap_or(60)),
            Some(config.bitrate_kbps),
        )
        .await?;

    let mut rtt_tick = tokio::time::interval(Duration::from_secs(1));
    let reason = loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break "session ended".to_string() };
                match event {
                    Event::Video(frame) => {
                        if frames.send(frame).await.is_err() {
                            break "decoder stopped".to_string();
                        }
                    }
                    Event::Cursor(update) => {
                        let mut cursor = shared.cursor.lock().expect("cursor poisoned");
                        cursor.x = update.x;
                        cursor.y = update.y;
                        cursor.visible = update.visible;
                        if let Some(shape) = update.shape {
                            let retired = cursor.shape.take();
                            if retired.is_some() {
                                cursor.retired = retired;
                            }
                            cursor.size = (shape.width, shape.height);
                            cursor.hotspot = (shape.hotspot_x, shape.hotspot_y);
                            cursor.shape = shape_image(shape);
                        }
                    }
                    Event::MonitorsChanged(monitors) => {
                        shared.status.lock().expect("status poisoned").monitors = monitors;
                    }
                    Event::StreamStats { monitor, fps, bitrate_kbps, skipped } => {
                        let mut status = shared.status.lock().expect("status poisoned");
                        if status.current == Some(monitor) {
                            status.host_fps = fps;
                            status.kbps = bitrate_kbps;
                            status.skipped = skipped;
                        }
                    }
                    Event::Closed(reason) => break reason,
                }
            }
            _ = rtt_tick.tick() => {
                let rtt = control.rtt().map(|d| d.as_secs_f64() * 1e3);
                shared.status.lock().expect("status poisoned").rtt_ms = rtt;
            }
        }
    };
    guard.close("viewer closed");
    {
        let mut status = shared.status.lock().expect("status poisoned");
        status.connected = false;
        status.message = format!("disconnected: {reason}");
    }
    *shared.control.lock().expect("control poisoned") = None;
    tracing::info!(%reason, "session over");
    Ok(())
}

fn set_message(shared: &RemoteShared, message: String) {
    shared.status.lock().expect("status poisoned").message = message;
}

fn shape_image(shape: CursorShape) -> Option<Arc<RenderImage>> {
    if shape.width == 0 || shape.height == 0 {
        return None;
    }
    // GPUI's RenderImage takes BGRA bytes in an RGBA-typed buffer.
    let buffer = ImageBuffer::from_raw(shape.width, shape.height, shape.bgra)?;
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(
        Frame::new(buffer),
        1,
    ))))
}

fn decode_loop(
    mut frames: mpsc::Receiver<VideoFrame>,
    window: &Receiver<isize>,
    placement: SharedPlacement,
    vsync: bool,
    shared: &RemoteShared,
) -> Result<()> {
    let hwnd = window
        .recv()
        .context("UI never provided its window handle")?;
    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    let device = D3dDevice::for_adapter(0)?;
    let mut presenter = Presenter::new(device.clone(), hwnd, placement, vsync)?;

    let mut decoder: Option<MfH264Decoder> = None;
    let mut decoder_size = (0u32, 0u32);
    let mut monitor: Option<MonitorId> = None;
    let mut last_seq: u64 = 0;
    let mut waiting_for_keyframe = true;
    let mut last_keyframe_request = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let mut period = Instant::now();
    let mut frames_in_period: u32 = 0;
    let mut decode_time = Duration::ZERO;

    while let Some(frame) = frames.blocking_recv() {
        let current = shared.status.lock().expect("status poisoned").current;
        if current.is_some() && current != Some(frame.monitor) {
            // Tail of a stream we switched away from.
            continue;
        }
        let size = (frame.width, frame.height);
        // Compare against the size we created the decoder with, not the decoder's own
        // report: it switches to the coded size (1088 rows for 1080p) after the first frame.
        let needs_new_decoder =
            monitor != Some(frame.monitor) || decoder.is_none() || decoder_size != size;
        if needs_new_decoder {
            decoder = Some(MfH264Decoder::new(size.0, size.1, &device.device)?);
            decoder_size = size;
            monitor = Some(frame.monitor);
            waiting_for_keyframe = true;
            last_seq = frame.seq.saturating_sub(1);
            shared.status.lock().expect("status poisoned").frame_size = size;
        }
        let lost = frame.seq != last_seq + 1 && frame.seq > last_seq;
        last_seq = frame.seq;
        if lost && !frame.keyframe {
            waiting_for_keyframe = true;
        }
        if waiting_for_keyframe {
            if frame.keyframe {
                waiting_for_keyframe = false;
            } else {
                request_keyframe(shared, frame.monitor, &mut last_keyframe_request);
                continue;
            }
        }
        let dec = decoder.as_mut().expect("decoder ensured");
        let t0 = Instant::now();
        let pictures = match dec.decode(&frame.data, frame.pts_us) {
            Ok(pictures) => pictures,
            Err(err) => {
                tracing::warn!(%err, seq = frame.seq, "decode failed; waiting for a keyframe");
                decoder = None;
                waiting_for_keyframe = true;
                request_keyframe(shared, frame.monitor, &mut last_keyframe_request);
                continue;
            }
        };
        decode_time += t0.elapsed();
        for picture in &pictures {
            presenter.present(picture)?;
            frames_in_period += 1;
        }
        if period.elapsed() >= Duration::from_secs(1) {
            let secs = period.elapsed().as_secs_f64();
            let mut status = shared.status.lock().expect("status poisoned");
            status.fps = f64::from(frames_in_period) / secs;
            status.decode_ms = if frames_in_period > 0 {
                decode_time.as_secs_f64() * 1e3 / f64::from(frames_in_period)
            } else {
                0.0
            };
            frames_in_period = 0;
            decode_time = Duration::ZERO;
            period = Instant::now();
        }
    }
    Ok(())
}

fn request_keyframe(shared: &RemoteShared, monitor: MonitorId, last: &mut Instant) {
    if last.elapsed() < Duration::from_millis(250) {
        return;
    }
    *last = Instant::now();
    if let Some(control) = shared.control.lock().expect("control poisoned").as_ref() {
        control.try_send(ControlMessage::RequestKeyframe { monitor });
    }
}
