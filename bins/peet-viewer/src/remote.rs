//! Remote session: network thread, decode thread, and the state they share with the UI.
//!
//! The network thread runs a tokio runtime with the `peet-core` session and forwards video
//! frames to the decode thread. The decode thread owns the D3D device, the H.264 decoder and
//! the DirectComposition presenter. The UI thread only reads [`RemoteShared`] and sends
//! control messages through the session's [`Control`] handle.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gpui::RenderImage;
use image::{Frame, ImageBuffer};
use peet_capture::windows::D3dDevice;
use peet_codec::windows::MfH264Decoder;
use peet_core::{ConnectOptions, Control, Event};
use peet_proto::{
    ControlMessage, CursorShape, Feature, InputEvent, MonitorId, MonitorInfo, RejectReason,
    SourceId, VideoFrame, WindowChange, WindowId, WindowInfo,
};
use smallvec::SmallVec;
use tokio::sync::mpsc;
use windows::Win32::Foundation::HWND;

use crate::present::{Presenter, SharedPlacement, SharedTiles, Tile, View};

/// Everything the UI shows about the session.
#[derive(Debug, Default, Clone)]
#[allow(clippy::struct_excessive_bools)] // independent UI flags, not a state machine
pub struct RemoteStatus {
    pub host_name: String,
    pub monitors: Vec<MonitorInfo>,
    pub current: Option<MonitorId>,
    /// Show every monitor at once, laid out as on the host.
    pub all_monitors: bool,
    /// The host window being shown instead of monitors, kept current from the host's
    /// `WindowChanged` reports. `current` remembers the monitor to go back to.
    pub window: Option<WindowInfo>,
    /// The shown window was closed on the host; its stream has ended.
    pub window_closed: bool,
    /// The host can stream single windows.
    pub windows_supported: bool,
    /// The host's windows as last listed, front to back.
    pub windows: Vec<WindowInfo>,
    /// A window list request is in flight.
    pub windows_loading: bool,
    pub frame_size: (u32, u32),
    pub fps: f64,
    pub decode_ms: f64,
    pub host_fps: u16,
    pub kbps: u32,
    pub skipped: u32,
    pub rtt_ms: Option<f64>,
    pub connected: bool,
    pub message: String,
    /// Set when the host turned us away, so the UI can react (ask for a PIN, say).
    pub rejected: Option<RejectReason>,
    /// A session that once worked was lost and is being re-established.
    pub reconnecting: bool,
}

impl RemoteStatus {
    /// What is on screen: the chosen window, every monitor, or the current monitor.
    pub fn views(&self) -> Vec<View> {
        if let Some(window) = &self.window {
            return vec![window.into()];
        }
        self.monitors
            .iter()
            .filter(|m| self.all_monitors || Some(m.id) == self.current)
            .map(View::from)
            .collect()
    }

    /// The stream whose statistics the status bar shows.
    pub fn main_source(&self) -> Option<SourceId> {
        match &self.window {
            Some(window) => Some(SourceId::Window(window.id)),
            None => self.current.map(SourceId::Monitor),
        }
    }
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
    /// Where each visible monitor is drawn, published by the presenter.
    pub tiles: SharedTiles,
    /// Set by the UI to end the session for good (no reconnect).
    pub quit: AtomicBool,
    /// Bumped per connection so the decoder starts clean after a reconnect.
    pub generation: AtomicU64,
    /// Clipboard text the host pushed, waiting for the UI thread to write locally.
    pub clipboard_in: Mutex<Option<String>>,
    /// Set once the network thread has fully stopped (decoder released), so the UI may start
    /// another session against the same window.
    pub finished: AtomicBool,
    /// Set by the UI to ask the network thread for a fresh window list.
    pub refresh_windows: AtomicBool,
    /// When the shown window was last asked to come to the front.
    last_focus: Mutex<Option<Instant>>,
}

/// A window stream asks the host to bring its window forward before input, at most this often.
/// The host does nothing when the window's app is already in front, so open menus survive.
const FOCUS_EVERY: Duration = Duration::from_secs(2);

impl RemoteShared {
    pub fn status(&self) -> RemoteStatus {
        self.status.lock().expect("status poisoned").clone()
    }

    /// Ends the session and stops reconnecting.
    pub fn quit(&self) {
        self.quit.store(true, Ordering::Relaxed);
    }

    /// Sends local clipboard text to the host.
    pub fn push_clipboard(&self, text: String) {
        let control = self.control.lock().expect("control poisoned").clone();
        if let Some(control) = control {
            control.try_send(ControlMessage::SetClipboard { text });
        }
    }

    /// Takes clipboard text the host pushed, if any.
    pub fn take_clipboard(&self) -> Option<String> {
        self.clipboard_in.lock().expect("clipboard poisoned").take()
    }

    /// Forwards an input event to the host, if connected. Returns false if dropped. While a
    /// window is shown, a click or keystroke first brings that window to the front, since
    /// injected input goes to whatever is on top.
    pub fn send_input(&self, event: InputEvent) -> bool {
        let control = self.control.lock().expect("control poisoned").clone();
        let Some(control) = control else { return false };
        if !matches!(event, InputEvent::MouseMove { .. }) {
            let window = self.status.lock().expect("status poisoned").window.clone();
            if let Some(window) = window {
                let mut last = self.last_focus.lock().expect("focus poisoned");
                if last.is_none_or(|t| t.elapsed() >= FOCUS_EVERY) {
                    *last = Some(Instant::now());
                    control.try_send(ControlMessage::FocusWindow { window: window.id });
                }
            }
        }
        control.send_input(event)
    }

    /// Restores the shown window if minimised and brings it to the front.
    pub fn focus_window(&self) {
        let control = self.control.lock().expect("control poisoned").clone();
        let window = self.status.lock().expect("status poisoned").window.clone();
        if let (Some(control), Some(window)) = (control, window) {
            *self.last_focus.lock().expect("focus poisoned") = Some(Instant::now());
            control.try_send(ControlMessage::FocusWindow { window: window.id });
        }
    }

    /// Asks the host to resize the shown window's visible area to `width` x `height`.
    pub fn resize_window(&self, width: u32, height: u32) {
        let control = self.control.lock().expect("control poisoned").clone();
        let window = self.status.lock().expect("status poisoned").window.clone();
        if let (Some(control), Some(window)) = (control, window) {
            control.try_send(ControlMessage::ResizeWindow {
                window: window.id,
                width,
                height,
            });
        }
    }

    /// Asks the network thread for a fresh window list.
    pub fn refresh_windows(&self) {
        let mut status = self.status.lock().expect("status poisoned");
        if status.windows_supported && !status.windows_loading {
            status.windows_loading = true;
            self.refresh_windows.store(true, Ordering::Relaxed);
        }
    }

    /// The presenter's tiles, with a shown window's tile moved to where the window is now: a
    /// window dragged on the host may send no new frame, so the presenter's copy goes stale.
    pub fn tiles(&self) -> Vec<Tile> {
        let mut tiles = self.tiles.lock().expect("tiles poisoned").clone();
        let window = self.status.lock().expect("status poisoned").window.clone();
        if let Some(window) = window {
            for tile in &mut tiles {
                if tile.source == SourceId::Window(window.id) {
                    tile.bounds = window.bounds;
                }
            }
        }
        tiles
    }

    /// Shows one host window instead of monitors.
    pub fn select_window(&self, window: &WindowInfo) {
        let control = self.control.lock().expect("control poisoned").clone();
        let Some(control) = control else { return };
        let disable: Vec<SourceId> = {
            let mut status = self.status.lock().expect("status poisoned");
            if status.window.as_ref().is_some_and(|w| w.id == window.id) && !status.window_closed {
                return;
            }
            let disable = status
                .views()
                .into_iter()
                .map(|v| v.source)
                .filter(|&s| s != SourceId::Window(window.id))
                .collect();
            status.window = Some(window.clone());
            status.window_closed = false;
            disable
        };
        for previous in disable {
            stream(&control, previous, false);
        }
        *self.last_focus.lock().expect("focus poisoned") = None;
        stream(&control, SourceId::Window(window.id), true);
    }

    /// Stops a shown window's stream, if any. Returns whether one was shown.
    fn leave_window(control: &Control, status: &mut RemoteStatus) -> bool {
        let Some(window) = status.window.take() else {
            return false;
        };
        if !status.window_closed {
            stream(control, SourceId::Window(window.id), false);
        }
        status.window_closed = false;
        true
    }

    /// Shows one monitor, stopping the streams of any others.
    pub fn select_monitor(&self, monitor: MonitorId) {
        let control = self.control.lock().expect("control poisoned").clone();
        let Some(control) = control else { return };
        let disable: Vec<MonitorId> = {
            let mut status = self.status.lock().expect("status poisoned");
            let left_window = Self::leave_window(&control, &mut status);
            if !left_window && !status.all_monitors && status.current == Some(monitor) {
                return;
            }
            if left_window {
                // None of the monitors is streaming any more.
                status.all_monitors = false;
                status.current = None;
            }
            let disable = if status.all_monitors {
                status
                    .monitors
                    .iter()
                    .map(|m| m.id)
                    .filter(|&m| m != monitor)
                    .collect()
            } else {
                status
                    .current
                    .into_iter()
                    .filter(|&m| m != monitor)
                    .collect()
            };
            status.all_monitors = false;
            status.current = Some(monitor);
            disable
        };
        for previous in disable {
            stream(&control, SourceId::Monitor(previous), false);
        }
        stream(&control, SourceId::Monitor(monitor), true);
    }

    /// Shows every monitor at once.
    pub fn select_all(&self) {
        let control = self.control.lock().expect("control poisoned").clone();
        let Some(control) = control else { return };
        let monitors: Vec<MonitorId> = {
            let mut status = self.status.lock().expect("status poisoned");
            let left_window = Self::leave_window(&control, &mut status);
            if status.all_monitors && !left_window {
                return;
            }
            status.all_monitors = true;
            status.monitors.iter().map(|m| m.id).collect()
        };
        for monitor in monitors {
            stream(&control, SourceId::Monitor(monitor), true);
        }
    }
}

fn stream(control: &Control, source: SourceId, enabled: bool) {
    control.try_send(ControlMessage::SetStream {
        source,
        enabled,
        max_fps: None,
        max_bitrate_kbps: None,
    });
}

/// Finds a window by hex or decimal id, or by case-insensitive title substring (front-most
/// match first).
fn find_window(windows: &[WindowInfo], spec: &str) -> Option<WindowInfo> {
    let id = spec.strip_prefix("0x").map_or_else(
        || spec.parse().ok(),
        |hex| u64::from_str_radix(hex, 16).ok(),
    );
    if let Some(id) = id {
        if let Some(w) = windows.iter().find(|w| w.id == WindowId(id)) {
            return Some(w.clone());
        }
    }
    let needle = spec.to_lowercase();
    windows
        .iter()
        .find(|w| w.title.to_lowercase().contains(&needle))
        .cloned()
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
    /// Window to show on the first connection: id or title substring.
    pub window: Option<String>,
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
    let result = runtime.block_on(session_loop(session_config, &shared, frame_tx));
    drop(runtime);
    let _ = decode_thread.join();
    result
}

/// Why a session ended.
enum SessionEnd {
    /// The UI asked to stop; do not reconnect.
    Quit,
    /// The connection was lost; reconnect.
    Lost(String),
    /// Someone at the host ended the session; do not reconnect.
    EndedByHost(String),
}

/// Runs sessions back to back, reconnecting with backoff until the UI quits or the host
/// rejects us outright (a wrong PIN will not fix itself).
async fn session_loop(
    config: SessionConfig,
    shared: &Arc<RemoteShared>,
    frames: mpsc::Sender<VideoFrame>,
) -> Result<()> {
    let mut attempt: u32 = 0;
    let mut ever_connected = false;
    loop {
        if shared.quit.load(Ordering::Relaxed) {
            return Ok(());
        }
        shared.generation.fetch_add(1, Ordering::Relaxed);
        match session(&config, shared, frames.clone()).await {
            Ok(SessionEnd::Quit | SessionEnd::EndedByHost(_)) => return Ok(()),
            Ok(SessionEnd::Lost(reason)) => {
                ever_connected = true;
                attempt = 0;
                shared.status.lock().expect("status poisoned").reconnecting = true;
                tracing::info!(%reason, "connection lost; reconnecting");
            }
            Err(err) => {
                // Only a session that once worked is worth retrying: a host that never
                // answered, or one that rejected us, is reported straight back to the user.
                if !ever_connected
                    || matches!(
                        err.downcast_ref::<peet_core::ConnectError>(),
                        Some(peet_core::ConnectError::Rejected(_))
                    )
                {
                    return Err(err);
                }
                attempt += 1;
                tracing::warn!(%err, attempt, "connect failed; retrying");
            }
        }
        // 1 s, 2 s, 4 s, 8 s, then 10 s between attempts.
        let delay =
            Duration::from_secs(u64::from(1u32 << attempt.min(3))).min(Duration::from_secs(10));
        set_message(
            shared,
            format!("reconnecting to {} in {}s", config.target, delay.as_secs()),
        );
        let deadline = Instant::now() + delay;
        while Instant::now() < deadline {
            if shared.quit.load(Ordering::Relaxed) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn session(
    config: &SessionConfig,
    shared: &Arc<RemoteShared>,
    frames: mpsc::Sender<VideoFrame>,
) -> Result<SessionEnd> {
    set_message(shared, format!("resolving {}", config.target));
    let addr = peet_core::resolve(&config.target).await?;
    set_message(shared, format!("connecting to {addr}"));
    let opts = ConnectOptions {
        pin: config.pin.clone(),
        ..ConnectOptions::default()
    };
    let session = peet_core::connect(addr, opts)
        .await
        .with_context(|| format!("connecting to {addr}"))?;
    let (info, control, mut events, guard) = session.split();

    // After a reconnect, go back to what the user was watching.
    let (wanted, all_monitors, mut window, first) = {
        let status = shared.status();
        (
            status.current.map(|m| m.0).or(config.monitor),
            status.all_monitors,
            status.window.clone().filter(|_| !status.window_closed),
            !status.connected && !status.reconnecting,
        )
    };
    let windows_supported = info.capabilities.features.contains(&Feature::WindowCapture);
    if !windows_supported {
        window = None;
    }
    if let (true, true, Some(spec)) = (first, windows_supported, &config.window) {
        let windows = control.list_windows(Duration::from_secs(10)).await?;
        window = Some(
            find_window(&windows, spec)
                .with_context(|| format!("no window on the host matches {spec:?}"))?,
        );
        shared.status.lock().expect("status poisoned").windows = windows;
    }
    let monitor = info
        .monitors
        .iter()
        .find(|m| wanted.is_none_or(|id| m.id.0 == id) && (wanted.is_some() || m.primary))
        .or(info.monitors.first())
        .map(|m| m.id)
        .context("host reports no monitors")?;
    {
        let mut status = shared.status.lock().expect("status poisoned");
        status.host_name.clone_from(&info.host_name);
        status.monitors.clone_from(&info.monitors);
        status.current = Some(monitor);
        status.window.clone_from(&window);
        status.window_closed = false;
        status.windows_supported = windows_supported;
        status.windows_loading = false;
        status.connected = true;
        status.reconnecting = false;
        status.message = String::new();
    }
    *shared.control.lock().expect("control poisoned") = Some(control.clone());
    let wanted_streams: Vec<SourceId> = if let Some(window) = &window {
        vec![SourceId::Window(window.id)]
    } else if all_monitors {
        info.monitors.iter().map(|m| m.id.into()).collect()
    } else {
        vec![monitor.into()]
    };
    for id in wanted_streams {
        control
            .set_stream(
                id,
                true,
                Some(u16::try_from(config.fps).unwrap_or(60)),
                Some(config.bitrate_kbps),
            )
            .await?;
    }

    let mut rtt_tick = tokio::time::interval(Duration::from_secs(1));
    let mut quit_tick = tokio::time::interval(Duration::from_millis(100));
    let end = loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break SessionEnd::Lost("session ended".to_string()) };
                match event {
                    Event::Video(frame) => {
                        if frames.send(frame).await.is_err() {
                            break SessionEnd::Quit; // the decoder died; reconnecting cannot help
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
                    Event::StreamStats { source, fps, bitrate_kbps, skipped } => {
                        let mut status = shared.status.lock().expect("status poisoned");
                        if status.main_source() == Some(source) {
                            status.host_fps = fps;
                            status.kbps = bitrate_kbps;
                            status.skipped = skipped;
                        }
                    }
                    Event::WindowChanged { window, change } => {
                        let mut status = shared.status.lock().expect("status poisoned");
                        let closed = matches!(change, WindowChange::Closed);
                        if let Some(shown) = status.window.as_mut().filter(|w| w.id == window) {
                            match change {
                                WindowChange::Bounds(bounds) => shown.bounds = bounds,
                                WindowChange::Minimized => shown.minimized = true,
                                WindowChange::Restored => shown.minimized = false,
                                WindowChange::Closed => {}
                            }
                            if closed {
                                status.window_closed = true;
                            }
                        }
                        if closed {
                            status.windows.retain(|w| w.id != window);
                        }
                    }
                    Event::Clipboard(text) => {
                        tracing::info!(chars = text.chars().count(), "clipboard received from host");
                        *shared.clipboard_in.lock().expect("clipboard poisoned") = Some(text);
                    }
                    Event::Closed(reason) => break SessionEnd::Lost(reason),
                    Event::EndedByHost(reason) => break SessionEnd::EndedByHost(reason),
                }
            }
            _ = rtt_tick.tick() => {
                let rtt = control.rtt().map(|d| d.as_secs_f64() * 1e3);
                shared.status.lock().expect("status poisoned").rtt_ms = rtt;
            }
            _ = quit_tick.tick() => {
                if shared.quit.load(Ordering::Relaxed) {
                    break SessionEnd::Quit;
                }
                if shared.refresh_windows.swap(false, Ordering::Relaxed) {
                    let (control, shared) = (control.clone(), shared.clone());
                    tokio::spawn(async move {
                        let result = control.list_windows(Duration::from_secs(5)).await;
                        let mut status = shared.status.lock().expect("status poisoned");
                        status.windows_loading = false;
                        match result {
                            Ok(windows) => status.windows = windows,
                            Err(err) => tracing::warn!(%err, "could not list host windows"),
                        }
                    });
                }
            }
        }
    };
    guard.close("viewer closed");
    let reason = match &end {
        SessionEnd::Quit => "disconnected".to_string(),
        SessionEnd::Lost(reason) => format!("disconnected: {reason}"),
        SessionEnd::EndedByHost(reason) => format!("the host ended the session ({reason})"),
    };
    {
        let mut status = shared.status.lock().expect("status poisoned");
        status.connected = false;
        status.message.clone_from(&reason);
    }
    *shared.control.lock().expect("control poisoned") = None;
    tracing::info!(%reason, "session over");
    Ok(end)
}

/// Turns a session error into a sentence for the user, plus the reject reason when the host
/// turned us away.
pub fn describe_error(err: &anyhow::Error) -> (String, Option<RejectReason>) {
    if let Some(peet_core::ConnectError::Rejected(reason)) =
        err.downcast_ref::<peet_core::ConnectError>()
    {
        let text = match reason {
            RejectReason::PinRequired => "this host needs a PIN",
            RejectReason::PinIncorrect => "wrong PIN",
            RejectReason::NotAuthorised => "the host does not allow your Tailscale identity",
            RejectReason::Busy => "the host is busy with another viewer",
            RejectReason::Paused => "the host is not accepting new connections right now",
            RejectReason::NoCommonCodec => "no video codec in common with the host",
            RejectReason::UnsupportedProtocolVersion => {
                "the host runs an incompatible PeetConnect version"
            }
        };
        return (text.to_string(), Some(*reason));
    }
    (format!("{err:#}"), None)
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

/// Decoder state for one stream.
struct Dec {
    decoder: MfH264Decoder,
    size: (u32, u32),
    last_seq: u64,
    waiting_for_keyframe: bool,
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
    let mut presenter =
        Presenter::new(device.clone(), hwnd, placement, shared.tiles.clone(), vsync)?;

    let long_ago = || {
        Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now)
    };
    let mut decoders: HashMap<SourceId, Dec> = HashMap::new();
    let mut last_keyframe_request: HashMap<SourceId, Instant> = HashMap::new();
    let mut period = Instant::now();
    let mut frames_in_period: u32 = 0;
    let mut decode_time = Duration::ZERO;
    let mut generation = shared.generation.load(Ordering::Relaxed);

    while let Some(frame) = frames.blocking_recv() {
        let frame_source = frame.source;
        let (visible, main) = {
            let status = shared.status.lock().expect("status poisoned");
            (status.views(), status.main_source())
        };
        if !visible.iter().any(|v| v.source == frame_source) {
            // Tail of a stream we switched away from.
            decoders.remove(&frame_source);
            continue;
        }
        // A reconnect restarts sequence numbers and may change the streams, so start clean.
        let now_generation = shared.generation.load(Ordering::Relaxed);
        if now_generation != generation {
            generation = now_generation;
            decoders.clear();
        }
        let size = (frame.width, frame.height);
        // Compare against the size we created the decoder with, not the decoder's own
        // report: it switches to the coded size (1088 rows for 1080p) after the first frame.
        if decoders.get(&frame_source).is_some_and(|d| d.size != size) {
            decoders.remove(&frame_source);
        }
        if let std::collections::hash_map::Entry::Vacant(slot) = decoders.entry(frame_source) {
            slot.insert(Dec {
                decoder: MfH264Decoder::new(size.0, size.1, &device.device)?,
                size,
                last_seq: frame.seq.saturating_sub(1),
                waiting_for_keyframe: true,
            });
            if main == Some(frame_source) || visible.len() == 1 {
                shared.status.lock().expect("status poisoned").frame_size = size;
            }
        }
        let dec = decoders.get_mut(&frame_source).expect("decoder ensured");
        let lost = frame.seq != dec.last_seq + 1 && frame.seq > dec.last_seq;
        dec.last_seq = frame.seq;
        if lost && !frame.keyframe {
            dec.waiting_for_keyframe = true;
        }
        if dec.waiting_for_keyframe {
            if frame.keyframe {
                dec.waiting_for_keyframe = false;
            } else {
                let since = last_keyframe_request
                    .entry(frame_source)
                    .or_insert_with(long_ago);
                request_keyframe(shared, frame_source, since);
                continue;
            }
        }
        let t0 = Instant::now();
        let pictures = match dec.decoder.decode(&frame.data, frame.pts_us) {
            Ok(pictures) => pictures,
            Err(err) => {
                tracing::warn!(%err, seq = frame.seq, "decode failed; waiting for a keyframe");
                decoders.remove(&frame_source);
                let since = last_keyframe_request
                    .entry(frame_source)
                    .or_insert_with(long_ago);
                request_keyframe(shared, frame_source, since);
                continue;
            }
        };
        decode_time += t0.elapsed();
        for picture in &pictures {
            presenter.present(frame_source, picture, &visible)?;
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

fn request_keyframe(shared: &RemoteShared, source: SourceId, last: &mut Instant) {
    if last.elapsed() < Duration::from_millis(250) {
        return;
    }
    *last = Instant::now();
    if let Some(control) = shared.control.lock().expect("control poisoned").as_ref() {
        control.try_send(ControlMessage::RequestKeyframe { source });
    }
}
