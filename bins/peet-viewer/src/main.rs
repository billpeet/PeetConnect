//! `peet-viewer`: the PeetConnect desktop viewer.
//!
//! With no arguments it opens the home screen: tailnet devices, favourites and recent
//! connections; click one (or type an address) to connect. `--connect <host>` skips straight
//! to a session. `--loopback` runs the milestone 2 rig (local capture, encode, decode and
//! present), still the quickest way to measure the pipeline on one machine.

use clap::{Parser, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "peet-viewer", version, about = "PeetConnect viewer")]
struct Cli {
    /// Host to connect to: MagicDNS name or Tailscale IP, with optional :port.
    #[arg(long, value_name = "HOST")]
    connect: Option<String>,
    /// PIN, if the host requires one.
    #[arg(long)]
    pin: Option<String>,
    /// Monitor ID to show or capture; default is the primary.
    #[arg(long)]
    monitor: Option<u32>,
    /// Run the local loopback rig instead of a remote session.
    #[arg(long)]
    loopback: bool,
    /// Loopback only: how video reaches the screen.
    #[arg(long, value_enum, default_value_t = PresentMode::D3d)]
    present: PresentMode,
    /// Loopback only: capture backend.
    #[arg(long, value_enum, default_value_t = CaptureBackend::Auto)]
    capture: CaptureBackend,
    /// Frame rate to request. Default: the saved setting (60).
    #[arg(long)]
    fps: Option<u32>,
    /// Bitrate to request, kbit/s. Default: the saved setting (12000).
    #[arg(long)]
    bitrate_kbps: Option<u32>,
    /// Loopback only: scale the captured desktop to this size before encoding, e.g. `3840x2160`.
    #[arg(long)]
    upscale: Option<String>,
    /// Present without waiting for vsync.
    #[arg(long)]
    no_vsync: bool,
    /// Loopback only: fail instead of falling back to the software encoder.
    #[arg(long)]
    hardware_only: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum PresentMode {
    /// Decode to system memory and draw with GPUI's `img` element (slow; for comparison).
    Gpui,
    /// Present decoded textures through DirectComposition beneath the GPUI window.
    D3d,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum CaptureBackend {
    Auto,
    Dxgi,
    Gdi,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    #[cfg(windows)]
    {
        app::run(&cli)
    }
    #[cfg(not(windows))]
    {
        let _ = cli;
        anyhow::bail!("peet-viewer currently supports Windows only")
    }
}

#[cfg(windows)]
mod home;
#[cfg(windows)]
mod pipeline;
#[cfg(windows)]
mod present;
#[cfg(windows)]
mod remote;

#[cfg(windows)]
mod app {
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::{channel, sync_channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Context as _, Result};
    use gpui::{
        canvas, div, img, prelude::*, px, rgb, size, App, Application, Bounds, ClipboardItem,
        Context, FocusHandle, FontWeight, ImageSource, KeyDownEvent, Keystroke, MouseButton,
        MouseDownEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection, ObjectFit, Pixels,
        Point, RenderImage, ScrollDelta, ScrollWheelEvent, TitlebarOptions, Window,
        WindowBackgroundAppearance, WindowBounds, WindowOptions,
    };
    use peet_capture::windows::Backend;
    use peet_proto::MonitorId;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    use crate::home::{self, Discovery, ViewerConfig};
    use crate::pipeline::{self, Output, UiFrame};
    use crate::present::{Placement, SharedPlacement};
    use crate::remote::{self, RemoteShared};
    use crate::{CaptureBackend, Cli, PresentMode};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Mode {
        /// Loopback rig drawing through GPUI's image element.
        LoopbackImage,
        /// Loopback rig drawing through DirectComposition.
        LoopbackD3d,
        /// Remote sessions drawing through DirectComposition, with the home screen between.
        Remote,
    }

    impl Mode {
        fn uses_composition(self) -> bool {
            self != Mode::LoopbackImage
        }
    }

    /// What the window shows in `Mode::Remote`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Screen {
        Home,
        Session,
    }

    /// Settings a session is started with.
    #[derive(Debug, Clone)]
    struct SessionDefaults {
        pin: Option<String>,
        monitor: Option<u32>,
        fps: u32,
        bitrate_kbps: u32,
        vsync: bool,
    }

    pub fn run(cli: &Cli) -> Result<()> {
        let (ui_tx, ui_rx) = sync_channel::<UiFrame>(2);
        let (win_tx, win_rx) = channel::<isize>();
        let placement: SharedPlacement = Arc::new(Mutex::new(Placement::default()));
        let config = ViewerConfig::load();

        let mode = match (cli.loopback, cli.present) {
            (false, _) => Mode::Remote,
            (true, PresentMode::Gpui) => Mode::LoopbackImage,
            (true, PresentMode::D3d) => Mode::LoopbackD3d,
        };
        let defaults = SessionDefaults {
            pin: cli.pin.clone(),
            monitor: cli.monitor,
            fps: cli.fps.unwrap_or(config.fps),
            bitrate_kbps: cli.bitrate_kbps.unwrap_or(config.bitrate_kbps),
            vsync: !cli.no_vsync,
        };

        if mode != Mode::Remote {
            let upscale = cli
                .upscale
                .as_deref()
                .map(|s| -> Result<(u32, u32)> {
                    let (w, h) = s
                        .split_once('x')
                        .context("upscale must look like 3840x2160")?;
                    Ok((w.parse()?, h.parse()?))
                })
                .transpose()?;
            let output = match cli.present {
                PresentMode::Gpui => Output::Gpui(ui_tx),
                PresentMode::D3d => Output::D3d {
                    window: win_rx,
                    placement: placement.clone(),
                    vsync: !cli.no_vsync,
                },
            };
            let config = pipeline::Config {
                monitor: cli.monitor,
                backend: match cli.capture {
                    CaptureBackend::Auto => Backend::Auto,
                    CaptureBackend::Dxgi => Backend::Dxgi,
                    CaptureBackend::Gdi => Backend::Gdi,
                },
                fps: defaults.fps,
                bitrate_kbps: defaults.bitrate_kbps,
                upscale,
                hardware_only: cli.hardware_only,
                output,
            };
            std::thread::Builder::new()
                .name("pipeline".into())
                .spawn(move || {
                    if let Err(err) = pipeline::run(config) {
                        tracing::error!(%err, "pipeline stopped");
                        std::process::exit(1);
                    }
                })?;
        }

        let title = if mode == Mode::Remote {
            "PeetConnect".to_string()
        } else {
            "PeetConnect viewer (loopback)".to_string()
        };
        let initial_target = cli.connect.clone();
        Application::new().run(move |cx: &mut App| {
            // Closing the last window exits; the viewer's drop ends any session.
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let bounds = Bounds::centered(None, size(px(1280.0), px(760.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some(title.into()),
                        ..Default::default()
                    }),
                    // Composition modes draw video underneath GPUI, through a transparent hole.
                    window_background: if mode.uses_composition() {
                        WindowBackgroundAppearance::Transparent
                    } else {
                        WindowBackgroundAppearance::Opaque
                    },
                    ..Default::default()
                },
                |_, cx| {
                    cx.new(|cx| {
                        Viewer::new(
                            mode,
                            ui_rx,
                            win_tx,
                            placement,
                            config,
                            defaults,
                            initial_target,
                            cx,
                        )
                    })
                },
            )
            .expect("open window");
            cx.activate(true);
        });
        Ok(())
    }

    struct Viewer {
        mode: Mode,
        screen: Screen,
        rx: Receiver<UiFrame>,
        /// Loopback only: hands the window handle to the pipeline thread once.
        win_tx: Option<Sender<isize>>,
        hwnd: Option<isize>,
        placement: SharedPlacement,
        shared: Arc<RemoteShared>,
        config: ViewerConfig,
        defaults: SessionDefaults,
        discovery: Arc<Mutex<Discovery>>,
        /// Address typed on the home screen.
        address: String,
        /// Last session's failure, shown on the home screen.
        last_error: Option<String>,
        /// A connect requested before the window handle was known: (target, display name).
        pending: Option<(String, String)>,
        focus: FocusHandle,
        focused_once: bool,
        image: Option<Arc<RenderImage>>,
        image_size: (u32, u32),
        renders: u32,
        new_frames: u32,
        latency: Duration,
        latency_n: u32,
        since: Instant,
        status: String,
        /// Last clipboard text seen locally, so we only push changes and never echo.
        last_clipboard: Option<String>,
        clipboard_checked: Instant,
    }

    impl Viewer {
        #[allow(clippy::too_many_arguments)]
        fn new(
            mode: Mode,
            rx: Receiver<UiFrame>,
            win_tx: Sender<isize>,
            placement: SharedPlacement,
            config: ViewerConfig,
            defaults: SessionDefaults,
            initial_target: Option<String>,
            cx: &mut Context<Self>,
        ) -> Self {
            let pending = initial_target.map(|t| (t.clone(), t));
            Self {
                mode,
                screen: Screen::Home,
                rx,
                win_tx: Some(win_tx),
                hwnd: None,
                placement,
                shared: Arc::new(RemoteShared::default()),
                config,
                defaults,
                discovery: home::start_discovery(),
                address: String::new(),
                last_error: None,
                pending,
                focus: cx.focus_handle(),
                focused_once: false,
                image: None,
                image_size: (0, 0),
                renders: 0,
                new_frames: 0,
                latency: Duration::ZERO,
                latency_n: 0,
                since: Instant::now(),
                status: "starting".into(),
                last_clipboard: None,
                clipboard_checked: Instant::now(),
            }
        }

        /// Starts a session against `target`, or queues it until the window handle is known.
        fn connect(&mut self, target: String, name: String) {
            let Some(hwnd) = self.hwnd else {
                self.pending = Some((target, name));
                return;
            };
            let target = target.trim().to_string();
            if target.is_empty() {
                return;
            }
            let (win_tx, win_rx) = channel::<isize>();
            let _ = win_tx.send(hwnd);
            let shared = Arc::new(RemoteShared::default());
            self.shared = shared.clone();
            self.last_error = None;
            self.last_clipboard = None;
            self.config.record_recent(&target, &name);
            self.screen = Screen::Session;
            self.status = "connecting".into();
            let config = remote::Config {
                session: remote::SessionConfig {
                    target,
                    pin: self.defaults.pin.clone(),
                    monitor: self.defaults.monitor,
                    fps: self.defaults.fps,
                    bitrate_kbps: self.defaults.bitrate_kbps,
                },
                window: win_rx,
                placement: self.placement.clone(),
                vsync: self.defaults.vsync,
                shared: shared.clone(),
            };
            let spawned = std::thread::Builder::new()
                .name("network".into())
                .spawn(move || {
                    if let Err(err) = remote::run(config) {
                        tracing::error!(%err, "session failed");
                        let mut status = shared.status.lock().expect("status poisoned");
                        status.connected = false;
                        status.message = format!("{err:#}");
                    }
                    shared.finished.store(true, Ordering::Relaxed);
                });
            if let Err(err) = spawned {
                self.last_error = Some(format!("could not start session: {err}"));
                self.screen = Screen::Home;
            }
        }

        /// Asks the session to end; the screen flips to Home once the thread has stopped.
        fn disconnect(&mut self) {
            self.shared.quit();
        }

        /// Flips back to the home screen once a session thread has fully stopped (and with it
        /// the presenter, so a new session can attach to the window).
        fn poll_session_end(&mut self) {
            if self.screen != Screen::Session || !self.shared.finished.load(Ordering::Relaxed) {
                return;
            }
            let status = self.shared.status();
            if status.message != "disconnected" && !status.message.is_empty() {
                self.last_error = Some(status.message);
            }
            self.screen = Screen::Home;
            self.status = String::new();
        }

        /// Two-way clipboard sync. Text the host pushed is written locally; local changes
        /// (checked a few times a second while the window is active) are pushed to the host.
        /// `last_clipboard` tracks both directions so nothing bounces back and forth.
        fn sync_clipboard(&mut self, window: &Window, cx: &mut Context<Self>) {
            if let Some(text) = self.shared.take_clipboard() {
                if self.last_clipboard.as_deref() != Some(text.as_str()) {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                    self.last_clipboard = Some(text);
                }
            }
            if self.clipboard_checked.elapsed() < Duration::from_millis(400) {
                return;
            }
            self.clipboard_checked = Instant::now();
            if !window.is_window_active() || !self.shared.status().connected {
                return;
            }
            let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
                return;
            };
            if self.last_clipboard.as_deref() == Some(text.as_str()) {
                return;
            }
            // The first read after connecting only primes the baseline; pushing whatever
            // happened to be on the clipboard would surprise the user.
            let prime = self.last_clipboard.is_none();
            self.last_clipboard = Some(text.clone());
            if !prime {
                self.shared.push_clipboard(text);
            }
        }

        fn take_latest_frame(&mut self, window: &mut Window) {
            let mut newest = None;
            while let Ok(frame) = self.rx.try_recv() {
                newest = Some(frame);
            }
            if let Some(frame) = newest {
                if let Some(old) = self.image.replace(frame.image) {
                    let _ = window.drop_image(old);
                }
                self.image_size = (frame.width, frame.height);
                self.new_frames += 1;
                self.latency += frame.captured_at.elapsed();
                self.latency_n += 1;
            }
        }

        fn update_status(&mut self) {
            let elapsed = self.since.elapsed();
            if elapsed < Duration::from_secs(1) {
                return;
            }
            let secs = elapsed.as_secs_f64();
            let ui_fps = f64::from(self.renders) / secs;
            self.status = match self.mode {
                Mode::LoopbackImage => {
                    let latency_ms = if self.latency_n > 0 {
                        self.latency.as_secs_f64() * 1e3 / f64::from(self.latency_n)
                    } else {
                        0.0
                    };
                    format!(
                        "gpui image  |  {}x{}  |  ui {ui_fps:.0} fps  |  new frames {:.0} fps  |  capture to render {latency_ms:.1} ms",
                        self.image_size.0,
                        self.image_size.1,
                        f64::from(self.new_frames) / secs,
                    )
                }
                Mode::LoopbackD3d => {
                    format!("d3d via DirectComposition  |  ui {ui_fps:.0} fps  |  video timings on stderr")
                }
                Mode::Remote => {
                    let s = self.shared.status();
                    if s.connected {
                        let monitor = s
                            .current
                            .and_then(|id| s.monitors.iter().position(|m| m.id == id))
                            .map_or(String::new(), |i| {
                                format!("monitor {} of {}", i + 1, s.monitors.len())
                            });
                        let rtt = s
                            .rtt_ms
                            .map_or(String::new(), |ms| format!("  |  rtt {ms:.0} ms"));
                        let skipped = if s.skipped > 0 {
                            format!("  |  host skipped {}", s.skipped)
                        } else {
                            String::new()
                        };
                        format!(
                            "{}  |  {monitor}  |  {}x{}  |  {:.0} fps (host {})  |  {} kbit/s  |  decode {:.2} ms{rtt}{skipped}  |  ctrl+alt+1-{} monitor, ctrl+alt+d disconnect",
                            s.host_name,
                            s.frame_size.0,
                            s.frame_size.1,
                            s.fps,
                            s.host_fps,
                            s.kbps,
                            s.decode_ms,
                            s.monitors.len().min(9),
                        )
                    } else if s.message.is_empty() {
                        "connecting".to_string()
                    } else {
                        s.message
                    }
                }
            };
            self.renders = 0;
            self.new_frames = 0;
            self.latency = Duration::ZERO;
            self.latency_n = 0;
            self.since = Instant::now();
        }

        /// Learns the native window handle once. Loopback hands it straight to the pipeline
        /// thread; remote sessions get it when they start.
        fn publish_window_handle(&mut self, window: &Window) {
            if self.hwnd.is_some() {
                return;
            }
            let Ok(handle) = HasWindowHandle::window_handle(window) else {
                return;
            };
            if let RawWindowHandle::Win32(win32) = handle.as_raw() {
                let hwnd = win32.hwnd.get();
                self.hwnd = Some(hwnd);
                if self.mode != Mode::Remote {
                    if let Some(tx) = self.win_tx.take() {
                        let _ = tx.send(hwnd);
                    }
                }
            }
        }

        fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
            if self.mode != Mode::Remote {
                return;
            }
            window.focus(&self.focus);
            let ks = &event.keystroke;
            let m = ks.modifiers;
            tracing::trace!(key = %ks.key, ?m, screen = ?self.screen, "key down");
            if self.screen == Screen::Home {
                self.on_home_key(ks, cx);
                return;
            }
            // Ctrl+Alt+<digit> switches the monitor locally instead of reaching the host, so
            // bare digits still type normally on the remote machine. Ctrl+Alt+D disconnects.
            if m.control && m.alt {
                if ks.key == "d" {
                    self.disconnect();
                    return;
                }
                if let Some(index) = ks
                    .key
                    .parse::<usize>()
                    .ok()
                    .filter(|&n| (1..=9).contains(&n))
                {
                    if let Some(target) =
                        self.shared.status().monitors.get(index - 1).map(|mm| mm.id)
                    {
                        self.select_monitor(target);
                    }
                    return;
                }
            }
            self.forward_key(ks);
        }

        /// The home screen's only text field: the address box.
        fn on_home_key(&mut self, ks: &Keystroke, cx: &mut Context<Self>) {
            let m = ks.modifiers;
            match ks.key.as_str() {
                "enter" => {
                    let target = self.address.trim().to_string();
                    if !target.is_empty() {
                        self.connect(target.clone(), target);
                    }
                }
                "backspace" => {
                    if m.control {
                        self.address.clear();
                    } else {
                        self.address.pop();
                    }
                }
                "escape" => self.address.clear(),
                "v" if m.control => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                        self.address.push_str(text.trim());
                    }
                }
                _ => {
                    if !m.control && !m.alt && !m.platform {
                        if let Some(ch) = &ks.key_char {
                            if !ch.chars().any(char::is_control) {
                                self.address.push_str(ch);
                            }
                        }
                    }
                }
            }
            cx.notify();
        }

        /// Sends a keystroke to the host as scancodes, wrapping the key in whatever modifiers
        /// were held. Characters with no scancode fall back to Unicode text.
        fn forward_key(&self, ks: &Keystroke) {
            let m = ks.modifiers;
            let mut mods = Vec::new();
            if m.control {
                mods.push(peet_input::keys::lookup("ctrl").unwrap());
            }
            if m.alt {
                mods.push(peet_input::keys::lookup("alt").unwrap());
            }
            if m.shift {
                mods.push(peet_input::keys::lookup("shift").unwrap());
            }
            if m.platform {
                mods.push(peet_input::keys::lookup("win").unwrap());
            }
            if let Some(key) = peet_input::keys::lookup(&ks.key) {
                for k in &mods {
                    self.send_key(*k, true);
                }
                self.send_key(key, true);
                self.send_key(key, false);
                for k in mods.iter().rev() {
                    self.send_key(*k, false);
                }
            } else if !m.control && !m.alt && !m.platform {
                if let Some(ch) = &ks.key_char {
                    self.shared
                        .send_input(peet_proto::InputEvent::Text { text: ch.clone() });
                }
            }
        }

        fn send_key(&self, key: peet_input::keys::Key, pressed: bool) {
            self.shared.send_input(peet_proto::InputEvent::Key {
                scancode: key.scancode,
                extended: key.extended,
                pressed,
            });
        }

        fn select_monitor(&mut self, monitor: MonitorId) {
            self.shared.select_monitor(monitor);
        }

        fn on_mouse_down(
            &mut self,
            event: &MouseDownEvent,
            window: &mut Window,
            _: &mut Context<Self>,
        ) {
            window.focus(&self.focus);
            if let Some((x, y)) = self.window_to_host(event.position, window) {
                self.shared
                    .send_input(peet_proto::InputEvent::MouseMove { x, y });
            }
            let button = map_button(event.button);
            self.shared.send_input(peet_proto::InputEvent::MouseButton {
                button,
                pressed: true,
            });
        }

        fn on_mouse_up(
            &mut self,
            event: &MouseUpEvent,
            window: &mut Window,
            _: &mut Context<Self>,
        ) {
            if let Some((x, y)) = self.window_to_host(event.position, window) {
                self.shared
                    .send_input(peet_proto::InputEvent::MouseMove { x, y });
            }
            let button = map_button(event.button);
            self.shared.send_input(peet_proto::InputEvent::MouseButton {
                button,
                pressed: false,
            });
        }

        fn on_mouse_move(
            &mut self,
            event: &MouseMoveEvent,
            window: &mut Window,
            _: &mut Context<Self>,
        ) {
            if let Some((x, y)) = self.window_to_host(event.position, window) {
                self.shared
                    .send_input(peet_proto::InputEvent::MouseMove { x, y });
            }
        }

        fn on_scroll(
            &mut self,
            event: &ScrollWheelEvent,
            _window: &mut Window,
            _: &mut Context<Self>,
        ) {
            // Protocol wheel deltas are in 1/120 units (one notch = 120), matching Windows.
            let (dx, dy) = match event.delta {
                ScrollDelta::Lines(p) => {
                    ((p.x * 120.0).round() as i32, (p.y * 120.0).round() as i32)
                }
                ScrollDelta::Pixels(p) => {
                    (f32::from(p.x).round() as i32, f32::from(p.y).round() as i32)
                }
            };
            if dx != 0 || dy != 0 {
                self.shared.send_input(peet_proto::InputEvent::MouseWheel {
                    delta_x: dx,
                    delta_y: dy,
                });
            }
        }

        /// Maps a window-space pointer position to host virtual-desktop pixels, inverting the
        /// letterbox the presenter uses. Returns `None` when there is no active monitor or the
        /// video area has no size yet.
        fn window_to_host(&self, position: Point<Pixels>, window: &Window) -> Option<(i32, i32)> {
            let status = self.shared.status();
            let current = status.current?;
            let monitor = status.monitors.iter().find(|m| m.id == current)?;
            let (fw, fh) = status.frame_size;
            if fw == 0 || fh == 0 {
                return None;
            }
            let placement = *self.placement.lock().ok()?;
            if placement.width <= 0 || placement.height <= 0 {
                return None;
            }
            let device_scale = f64::from(window.scale_factor());
            let scale = (f64::from(placement.width) / f64::from(fw))
                .min(f64::from(placement.height) / f64::from(fh));
            if scale <= 0.0 {
                return None;
            }
            let shown_w = f64::from(fw) * scale;
            let shown_h = f64::from(fh) * scale;
            let off_x = f64::from(placement.x) + (f64::from(placement.width) - shown_w) / 2.0;
            let off_y = f64::from(placement.y) + (f64::from(placement.height) - shown_h) / 2.0;
            let dev_x = f64::from(f32::from(position.x)) * device_scale;
            let dev_y = f64::from(f32::from(position.y)) * device_scale;
            let frame_x = ((dev_x - off_x) / scale).clamp(0.0, f64::from(fw - 1));
            let frame_y = ((dev_y - off_y) / scale).clamp(0.0, f64::from(fh - 1));
            let vx = f64::from(monitor.bounds.x)
                + frame_x * f64::from(monitor.bounds.width) / f64::from(fw);
            let vy = f64::from(monitor.bounds.y)
                + frame_y * f64::from(monitor.bounds.height) / f64::from(fh);
            Some((vx.round() as i32, vy.round() as i32))
        }

        /// The remote cursor as an absolutely positioned image over the video, mapped through
        /// the same letterbox the presenter uses.
        fn cursor_element(&self, window: &mut Window) -> Option<gpui::AnyElement> {
            let status = self.shared.status();
            let current = status.current?;
            let monitor = status.monitors.iter().find(|m| m.id == current)?;
            let (fw, fh) = status.frame_size;
            if fw == 0 || fh == 0 {
                return None;
            }
            let placement = *self.placement.lock().ok()?;
            if placement.width <= 0 || placement.height <= 0 {
                return None;
            }
            let mut cursor = self.shared.cursor.lock().ok()?;
            if let Some(retired) = cursor.retired.take() {
                drop(cursor);
                // Dropping a RenderImage requires the window; do it here on the UI thread.
                let _ = window.drop_image(retired);
                cursor = self.shared.cursor.lock().ok()?;
            }
            if !cursor.visible {
                return None;
            }
            let shape = cursor.shape.clone()?;
            let scale = (f64::from(placement.width) / f64::from(fw))
                .min(f64::from(placement.height) / f64::from(fh));
            let shown_w = f64::from(fw) * scale;
            let shown_h = f64::from(fh) * scale;
            let off_x = f64::from(placement.x) + (f64::from(placement.width) - shown_w) / 2.0;
            let off_y = f64::from(placement.y) + (f64::from(placement.height) - shown_h) / 2.0;
            let local_x = f64::from(cursor.x - monitor.bounds.x) - f64::from(cursor.hotspot.0);
            let local_y = f64::from(cursor.y - monitor.bounds.y) - f64::from(cursor.hotspot.1);
            let device_scale = f64::from(window.scale_factor());
            let left = (off_x + local_x * scale) / device_scale;
            let top = (off_y + local_y * scale) / device_scale;
            let width = f64::from(cursor.size.0) * scale / device_scale;
            let height = f64::from(cursor.size.1) * scale / device_scale;
            Some(
                div()
                    .absolute()
                    .left(px(left as f32))
                    .top(px(top as f32))
                    .w(px(width as f32))
                    .h(px(height as f32))
                    .child(img(ImageSource::Render(shape)).size_full())
                    .into_any_element(),
            )
        }

        // ---- home screen -----------------------------------------------------------------

        /// One clickable host row with a favourite toggle.
        fn host_row(
            &self,
            id: (&'static str, usize),
            target: &str,
            name: String,
            detail: String,
            online: Option<bool>,
            cx: &mut Context<Self>,
        ) -> gpui::AnyElement {
            let fav = self.config.is_favourite(target);
            let (t1, n1) = (target.to_string(), name.clone());
            let (t2, n2) = (target.to_string(), name.clone());
            let text = if online == Some(false) {
                0x777777
            } else {
                0xe6e6e6
            };
            let dot = match online {
                Some(true) => 0x3dd68c,
                Some(false) => 0x444a55,
                None => 0x5c6b85,
            };
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .rounded_md()
                .cursor_pointer()
                .hover(|s| s.bg(rgb(0x2a2f3a)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.connect(t1.clone(), n1.clone());
                    cx.notify();
                }))
                .child(
                    div()
                        .id((id.0, id.1 + 10_000))
                        .w(px(22.0))
                        .text_center()
                        .cursor_pointer()
                        .text_color(rgb(if fav { 0xffc53d } else { 0x555b66 }))
                        .hover(|s| s.text_color(rgb(0xffc53d)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.config.toggle_favourite(&t2, &n2);
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .child(if fav { "★" } else { "☆" }),
                )
                .child(div().w(px(10.0)).h(px(10.0)).rounded_full().bg(rgb(dot)))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(div().text_color(rgb(text)).child(name))
                        .child(div().text_xs().text_color(rgb(0x8a909c)).child(detail)),
                )
                .into_any_element()
        }

        fn section(title: &'static str) -> gpui::Div {
            div()
                .mt_4()
                .mb_1()
                .px_3()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(0x8a909c))
                .child(title)
        }

        fn render_home(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
            let (peers, disc_error, loaded) = self
                .discovery
                .lock()
                .map(|d| (d.peers.clone(), d.error.clone(), d.ever_loaded))
                .unwrap_or_default();
            let fav_rows: Vec<_> = self
                .config
                .favourites
                .clone()
                .into_iter()
                .enumerate()
                .map(|(i, s)| {
                    let online = peers
                        .iter()
                        .find(|p| p.target == s.target)
                        .map(|p| p.online);
                    self.host_row(("fav", i), &s.target, s.name, s.target.clone(), online, cx)
                })
                .collect();
            let recent_rows: Vec<_> = self
                .config
                .recents
                .clone()
                .into_iter()
                .enumerate()
                .map(|(i, s)| {
                    let online = peers
                        .iter()
                        .find(|p| p.target == s.target)
                        .map(|p| p.online);
                    self.host_row(
                        ("recent", i),
                        &s.target,
                        s.name,
                        s.target.clone(),
                        online,
                        cx,
                    )
                })
                .collect();
            let peer_rows: Vec<_> = peers
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let detail = format!(
                        "{}  ·  {}  ·  {}",
                        p.dns_name,
                        p.target,
                        if p.online { &p.os } else { "offline" }
                    );
                    self.host_row(
                        ("peer", i),
                        &p.target,
                        p.name.clone(),
                        detail,
                        Some(p.online),
                        cx,
                    )
                })
                .collect();

            let address = if self.address.is_empty() {
                div()
                    .text_color(rgb(0x6a7080))
                    .child("type a MagicDNS name or Tailscale IP, then Enter")
            } else {
                div()
                    .text_color(rgb(0xf0f0f0))
                    .child(format!("{}▏", self.address))
            };
            let mut column = div()
                .flex()
                .flex_col()
                .w(px(640.0))
                .max_w_full()
                .child(
                    div()
                        .mb_3()
                        .text_xl()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(0xf0f0f0))
                        .child("PeetConnect"),
                )
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(0x1c2028))
                        .border_1()
                        .border_color(rgb(0x343a46))
                        .child(address),
                );
            if let Some(err) = &self.last_error {
                column = column.child(
                    div()
                        .mt_2()
                        .px_3()
                        .text_sm()
                        .text_color(rgb(0xff7b72))
                        .child(err.clone()),
                );
            }
            if !fav_rows.is_empty() {
                column = column.child(Self::section("FAVOURITES")).children(fav_rows);
            }
            if !recent_rows.is_empty() {
                column = column.child(Self::section("RECENT")).children(recent_rows);
            }
            column = column.child(Self::section("TAILNET"));
            if let Some(err) = disc_error {
                column = column.child(div().px_3().text_sm().text_color(rgb(0xff7b72)).child(err));
            } else if !loaded {
                column = column.child(
                    div()
                        .px_3()
                        .text_sm()
                        .text_color(rgb(0x8a909c))
                        .child("looking for devices"),
                );
            } else if peer_rows.is_empty() {
                column = column.child(
                    div()
                        .px_3()
                        .text_sm()
                        .text_color(rgb(0x8a909c))
                        .child("no other devices on the tailnet"),
                );
            } else {
                column = column.children(peer_rows);
            }

            div()
                .id("home")
                .size_full()
                .overflow_y_scroll()
                .bg(rgb(0x14161a))
                .flex()
                .justify_center()
                .py_8()
                .px_4()
                .child(column)
                .into_any_element()
        }
    }

    /// Translates a GPUI mouse button to the protocol's.
    fn map_button(button: MouseButton) -> peet_proto::MouseButton {
        match button {
            MouseButton::Left => peet_proto::MouseButton::Left,
            MouseButton::Right => peet_proto::MouseButton::Right,
            MouseButton::Middle => peet_proto::MouseButton::Middle,
            MouseButton::Navigate(NavigationDirection::Back) => peet_proto::MouseButton::Back,
            MouseButton::Navigate(NavigationDirection::Forward) => peet_proto::MouseButton::Forward,
        }
    }

    impl Drop for Viewer {
        fn drop(&mut self) {
            self.shared.quit();
        }
    }

    impl Render for Viewer {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            window.request_animation_frame();
            self.renders += 1;
            if self.mode == Mode::LoopbackImage {
                self.take_latest_frame(window);
            } else {
                self.publish_window_handle(window);
            }
            // Keep keyboard focus on the root: GPUI drops it when the window deactivates, and
            // nothing else in this window ever takes it.
            if !self.focused_once || (window.is_window_active() && !self.focus.is_focused(window)) {
                self.focused_once = true;
                window.focus(&self.focus);
            }
            if self.mode == Mode::Remote {
                if let Some((target, name)) = self.pending.take() {
                    if self.hwnd.is_some() {
                        self.connect(target, name);
                    } else {
                        self.pending = Some((target, name));
                    }
                }
                self.poll_session_end();
                if self.screen == Screen::Session {
                    self.sync_clipboard(window, cx);
                }
            }
            self.update_status();

            let root = div()
                .size_full()
                .flex()
                .flex_col()
                .text_color(rgb(0xdddddd))
                .track_focus(&self.focus)
                .on_key_down(cx.listener(Self::on_key));

            if self.mode == Mode::Remote && self.screen == Screen::Home {
                return root.child(self.render_home(cx));
            }

            let in_session = self.mode == Mode::Remote;
            let content = match self.mode {
                Mode::LoopbackImage => match &self.image {
                    Some(image) => img(ImageSource::Render(image.clone()))
                        .size_full()
                        .object_fit(ObjectFit::Contain)
                        .into_any_element(),
                    None => div().size_full().into_any_element(),
                },
                Mode::LoopbackD3d | Mode::Remote => {
                    let placement = self.placement.clone();
                    let scale = window.scale_factor();
                    canvas(
                        |_, _, _| (),
                        move |bounds, (), _, _| {
                            let to_px = |v: gpui::Pixels| (f32::from(v) * scale).round() as i32;
                            let next = Placement {
                                x: to_px(bounds.origin.x),
                                y: to_px(bounds.origin.y),
                                width: to_px(bounds.size.width),
                                height: to_px(bounds.size.height),
                            };
                            if let Ok(mut p) = placement.lock() {
                                *p = next;
                            }
                        },
                    )
                    .size_full()
                    .into_any_element()
                }
            };
            let cursor = if in_session {
                self.cursor_element(window)
            } else {
                None
            };

            // In composition modes the video area must stay unpainted so the visual
            // underneath shows through; only the status bar gets a background.
            let root = if self.mode.uses_composition() {
                root
            } else {
                root.bg(rgb(0x101010))
            };
            let mut bar = div()
                .h(px(26.0))
                .px_2()
                .flex()
                .items_center()
                .gap_3()
                .text_sm()
                .bg(rgb(0x101010))
                .child(div().flex_1().child(self.status.clone()));
            if in_session {
                bar = bar.child(
                    div()
                        .id("disconnect")
                        .px_2()
                        .rounded_md()
                        .cursor_pointer()
                        .text_color(rgb(0xaaaaaa))
                        .hover(|s| s.bg(rgb(0x333333)).text_color(rgb(0xffffff)))
                        .on_click(cx.listener(|this, _, _, _| this.disconnect()))
                        .child("disconnect"),
                );
            }
            root.child(bar).child({
                let mut area = div().flex_1().min_h_0().relative();
                if in_session {
                    area = area
                        .on_mouse_move(cx.listener(Self::on_mouse_move))
                        .on_scroll_wheel(cx.listener(Self::on_scroll));
                    for button in [
                        MouseButton::Left,
                        MouseButton::Right,
                        MouseButton::Middle,
                        MouseButton::Navigate(NavigationDirection::Back),
                        MouseButton::Navigate(NavigationDirection::Forward),
                    ] {
                        area = area
                            .on_mouse_down(button, cx.listener(Self::on_mouse_down))
                            .on_mouse_up(button, cx.listener(Self::on_mouse_up));
                    }
                }
                area.child(content).children(cursor)
            })
        }
    }
}
