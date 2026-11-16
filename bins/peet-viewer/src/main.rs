//! `peet-viewer`: the PeetConnect desktop viewer.
//!
//! With no arguments it opens the home screen: tailnet devices, favourites and recent
//! connections; click one (or type an address) to connect. `--connect <host>` skips straight
//! to a session. `--loopback` runs the milestone 2 rig (local capture, encode, decode and
//! present), still the quickest way to measure the pipeline on one machine.
//!
//! It is a GUI-subsystem program, so the Start menu opens no console window. Started from a
//! terminal it attaches to that console for `--help` and logs; otherwise it logs to
//! `%LOCALAPPDATA%\PeetConnect\viewer.log`.

#![cfg_attr(windows, windows_subsystem = "windows")]
// On other platforms the binary is a stub, so nothing below main is reachable.
#![cfg_attr(not(windows), allow(dead_code))]

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
    /// Show one host window instead of a monitor: its id (hex or decimal) or part of its title.
    #[arg(long, value_name = "ID|TITLE", conflicts_with = "loopback")]
    window: Option<String>,
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
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    match log_file() {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init(),
    }
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

/// Where logs go when there is no console to write to. `None` means use stderr: either we
/// attached to the terminal that started us, or stderr was redirected by whoever did.
#[cfg(windows)]
fn log_file() -> Option<std::fs::File> {
    use windows::Win32::System::Console::{
        AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
    };

    // SAFETY: plain console calls without pointers.
    let redirected =
        unsafe { GetStdHandle(STD_ERROR_HANDLE) }.is_ok_and(|h| !h.is_invalid() && !h.0.is_null());
    if redirected {
        return None;
    }
    // SAFETY: as above. Fails when the parent has no console (Explorer, the Start menu).
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_ok() {
        return None;
    }
    let dir = dirs::data_local_dir()?.join("PeetConnect");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("viewer.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 10 * 1024 * 1024) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

#[cfg(not(windows))]
fn log_file() -> Option<std::fs::File> {
    None
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
mod thumbs;
#[cfg(windows)]
mod ui;

#[cfg(windows)]
mod app {
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::{channel, sync_channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use anyhow::{Context as _, Result};
    use gpui::{
        canvas, deferred, div, img, prelude::*, px, rgb, rgba, size, App, Application, Bounds,
        ClickEvent, ClipboardItem, Context, CursorStyle, FocusHandle, FontWeight, ImageSource,
        KeyDownEvent, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
        NavigationDirection, ObjectFit, Pixels, Point, RenderImage, ScrollDelta, ScrollWheelEvent,
        SharedString, TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds,
        WindowOptions,
    };
    use peet_capture::windows::Backend;
    use peet_proto::{MonitorId, RejectReason};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    use crate::home::{self, Discovery, ViewerConfig};
    use crate::pipeline::{self, Output, UiFrame};
    use crate::present::{Placement, SharedPlacement};
    use crate::remote::{self, RemoteShared};
    use crate::{thumbs, ui};
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

    /// Which home-screen text box takes typing.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Field {
        Address,
        Pin,
        /// The name in the details panel.
        Name,
    }

    /// The host the details panel shows: its target and discovered (or saved) name.
    #[derive(Debug, Clone)]
    struct Panel {
        target: String,
        base: String,
    }

    /// Bitrate presets offered on the home screen, in kbit/s.
    const BITRATE_PRESETS: [u32; 5] = [4000, 8000, 12_000, 20_000, 30_000];
    /// Frame-rate presets offered on the home screen.
    const FPS_PRESETS: [u32; 2] = [30, 60];
    /// The in-session bar stays visible this close to the top edge while fullscreen.
    const REVEAL_BAR_PX: f32 = 40.0;
    /// Height of the in-session bar, logical pixels.
    const BAR_PX: f32 = 40.0;
    /// Home-screen card width and the gap between cards; four fit the column.
    const CARD_W: f32 = 232.0;
    const CARD_GAP: f32 = 14.0;
    const GRID_W: f32 = CARD_W * 4.0 + CARD_GAP * 3.0;
    /// Width of the preview in the details panel.
    const PANEL_THUMB_W: f32 = 324.0;
    /// A viewer resize by the user is sent to the host once it has held this long.
    const RESIZE_SETTLE: Duration = Duration::from_millis(400);
    /// Video-area changes this soon after the viewer resized itself are its own doing.
    const SELF_RESIZE_GRACE: Duration = Duration::from_millis(700);
    /// How long after the viewer's last pointer move a host cursor move still counts as an
    /// echo of it. A move later than that came from the host side (its own mouse, or an app
    /// warping the pointer), so the cursor is drawn where the host says until the viewer
    /// moves again.
    const CURSOR_ECHO_GRACE: Duration = Duration::from_millis(300);

    /// Settings a session is started with.
    #[derive(Debug, Clone)]
    struct SessionDefaults {
        pin: Option<String>,
        monitor: Option<u32>,
        window: Option<String>,
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
            window: cli.window.clone(),
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
        Application::new()
            .with_assets(ui::Assets)
            .run(move |cx: &mut App| {
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

    #[allow(clippy::struct_excessive_bools)] // independent UI flags, not a state machine
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
        /// PIN typed on the home screen (empty: use the `--pin` flag, if any).
        pin: String,
        field: Field,
        /// The last host asked for a PIN; the home screen points at the PIN box.
        needs_pin: bool,
        /// The user opened the PIN box.
        pin_open: bool,
        /// The stream-quality popover is open.
        quality_open: bool,
        /// The details panel, when open.
        panel: Option<Panel>,
        /// The name being edited in the details panel.
        name_edit: String,
        thumbs: thumbs::Cache,
        /// Current or last session: (target, display name).
        session_target: (String, String),
        /// Window title as last set.
        title: String,
        /// Pointer y in the window, to reveal the bar in fullscreen.
        pointer_y: f32,
        /// When the pointer last moved over the video, so the remote cursor can follow the
        /// local pointer without waiting for the host's echo.
        local_moved_at: Option<Instant>,
        /// The host window list is open under the bar.
        picker_open: bool,
        /// Window-size matching (see [`ViewerConfig::match_window_size`]).
        size_sync: SizeSync,
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
                pin: String::new(),
                field: Field::Address,
                needs_pin: false,
                pin_open: false,
                quality_open: false,
                panel: None,
                name_edit: String::new(),
                thumbs: thumbs::Cache::default(),
                session_target: (String::new(), String::new()),
                title: String::new(),
                pointer_y: 0.0,
                local_moved_at: None,
                picker_open: false,
                size_sync: SizeSync::default(),
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
        /// `name` is the discovered or saved name; the user's own name for the host, if any,
        /// is what the session shows.
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
            self.needs_pin = false;
            self.session_target = (target.clone(), self.config.display_name(&target, &name));
            self.config.record_recent(&target, &name);
            self.quality_open = false;
            self.panel = None;
            if self.field == Field::Name {
                self.field = Field::Address;
            }
            self.screen = Screen::Session;
            self.status = "connecting".into();
            let thumbnail = thumbs::path(&target);
            let config = remote::Config {
                session: remote::SessionConfig {
                    target,
                    pin: if self.pin.is_empty() {
                        self.defaults.pin.clone()
                    } else {
                        Some(self.pin.clone())
                    },
                    monitor: self.defaults.monitor,
                    window: self.defaults.window.clone(),
                    fps: self.defaults.fps,
                    bitrate_kbps: self.defaults.bitrate_kbps,
                    thumbnail,
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
                        let (message, rejected) = remote::describe_error(&err);
                        let mut status = shared.status.lock().expect("status poisoned");
                        status.connected = false;
                        status.message = message;
                        status.rejected = rejected;
                    }
                    shared.finished.store(true, Ordering::Relaxed);
                });
            if let Err(err) = spawned {
                self.last_error = Some(format!("could not start session: {err}"));
                self.screen = Screen::Home;
            }
        }

        /// A session started from a bare address takes the host's own name once connected,
        /// and its recents entry learns the name too.
        fn adopt_host_name(&mut self) {
            let (target, name) = &self.session_target;
            if name != target {
                return;
            }
            let host = self.shared.status().host_name;
            if host.is_empty() {
                return;
            }
            let target = target.clone();
            self.config.record_recent(&target, &host);
            self.session_target.1 = self.config.display_name(&target, &host);
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
            let (target, name) = self.session_target.clone();
            if status.message != "disconnected" && !status.message.is_empty() {
                self.last_error = Some(format!("{name}: {}", status.message));
            }
            if let Some(reason @ (RejectReason::PinRequired | RejectReason::PinIncorrect)) =
                status.rejected
            {
                // Put the host back in the address box and point at the PIN box, so a
                // corrected PIN plus Enter retries.
                if reason == RejectReason::PinIncorrect {
                    self.pin.clear();
                }
                self.address = target;
                self.field = Field::Pin;
                self.needs_pin = true;
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
                        let monitor = if let Some(w) = &s.window {
                            let state = if s.window_closed {
                                " (closed)"
                            } else if w.minimized {
                                " (minimised)"
                            } else {
                                ""
                            };
                            format!("window {}{state}", short_title(&w.title, 40))
                        } else if s.all_monitors {
                            format!("all {} monitors", s.monitors.len())
                        } else {
                            s.current
                                .and_then(|id| s.monitors.iter().position(|m| m.id == id))
                                .map_or(String::new(), |i| {
                                    format!("monitor {} of {}", i + 1, s.monitors.len())
                                })
                        };
                        let rtt = s
                            .rtt_ms
                            .map_or(String::new(), |ms| format!("  |  rtt {ms:.0} ms"));
                        let skipped = if s.skipped > 0 {
                            format!("  |  host skipped {}", s.skipped)
                        } else {
                            String::new()
                        };
                        format!(
                            "{}  |  {monitor}  |  {}x{}  |  {:.0} fps (host {})  |  {} kbit/s  |  decode {:.2} ms{rtt}{skipped}",
                            s.host_name,
                            s.frame_size.0,
                            s.frame_size.1,
                            s.fps,
                            s.host_fps,
                            s.kbps,
                            s.decode_ms,
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
            // Escape while still connecting cancels; once connected every key is the host's.
            if ks.key == "escape" && !self.shared.status().connected {
                self.disconnect();
                return;
            }
            // Ctrl+Alt+<digit> switches the monitor locally instead of reaching the host, so
            // bare digits still type normally on the remote machine. Ctrl+Alt+D disconnects,
            // Ctrl+Alt+F toggles fullscreen.
            if m.control && m.alt {
                if ks.key == "d" {
                    self.disconnect();
                    return;
                }
                if ks.key == "f" {
                    window.toggle_fullscreen();
                    return;
                }
                if ks.key == "0" {
                    self.shared.select_all();
                    return;
                }
                if ks.key == "w" {
                    self.toggle_picker();
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

        /// Typing on the home screen edits the active text box (address, PIN or the panel's
        /// name); Tab switches between address and PIN, Enter connects (or saves the name) and
        /// Escape clears, cancels the rename, or closes what is open.
        fn on_home_key(&mut self, ks: &Keystroke, cx: &mut Context<Self>) {
            let m = ks.modifiers;
            match ks.key.as_str() {
                "enter" if self.field == Field::Name => self.commit_rename(),
                "enter" => self.connect_typed(),
                "tab" => {
                    self.field = match self.field {
                        Field::Address if self.pin_visible() => Field::Pin,
                        Field::Pin => Field::Address,
                        other => other,
                    };
                }
                "escape" if self.field == Field::Name => self.cancel_rename(),
                "escape" if self.quality_open => self.quality_open = false,
                "escape" if self.panel.is_some() => self.close_panel(),
                // GPUI gives the space bar no key_char; names need it.
                "space" if !m.control && !m.alt && !m.platform => self.field_mut().push(' '),
                "backspace" => {
                    let field = self.field_mut();
                    if m.control {
                        field.clear();
                    } else {
                        field.pop();
                    }
                }
                "escape" => self.field_mut().clear(),
                "v" if m.control => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                        self.field_mut().push_str(text.trim());
                    }
                }
                _ => {
                    if !m.control && !m.alt && !m.platform {
                        if let Some(ch) = &ks.key_char {
                            if !ch.chars().any(char::is_control) {
                                self.field_mut().push_str(ch);
                            }
                        }
                    }
                }
            }
            cx.notify();
        }

        fn field_mut(&mut self) -> &mut String {
            match self.field {
                Field::Address => &mut self.address,
                Field::Pin => &mut self.pin,
                Field::Name => &mut self.name_edit,
            }
        }

        /// Connects to what the address box names: the one host a search narrowed to, else
        /// the text itself as an address.
        fn connect_typed(&mut self) {
            let typed = self.address.trim().to_string();
            if typed.is_empty() {
                return;
            }
            let matches = self.search_matches(&typed);
            if let [(target, name)] = matches.as_slice() {
                let (target, name) = (target.clone(), name.clone());
                self.connect(target, name);
            } else {
                self.connect(typed.clone(), typed);
            }
        }

        fn set_fps(&mut self, fps: u32) {
            self.defaults.fps = fps;
            self.config.fps = fps;
            self.config.save();
        }

        fn set_bitrate(&mut self, kbps: u32) {
            self.defaults.bitrate_kbps = kbps;
            self.config.bitrate_kbps = kbps;
            self.config.save();
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
            self.picker_open = false;
            self.shared.select_monitor(monitor);
        }

        /// Shows the primary monitor (or the first), leaving a window stream.
        fn show_desktop(&mut self) {
            let status = self.shared.status();
            let monitor = status
                .current
                .or_else(|| status.monitors.iter().find(|m| m.primary).map(|m| m.id))
                .or_else(|| status.monitors.first().map(|m| m.id));
            if let Some(monitor) = monitor {
                self.select_monitor(monitor);
            }
        }

        /// Keeps the viewer and a shown host window the same size, when enabled. The host's
        /// window resizing (or being picked) resizes the viewer to show it 1:1; the user
        /// resizing the viewer resizes the host window once the drag settles. Changes the
        /// viewer makes itself are never sent back, so the two cannot chase each other.
        fn sync_window_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            let status = self.shared.status();
            let Some(shown) = status
                .window
                .as_ref()
                .filter(|w| status.connected && !status.window_closed && !w.minimized)
            else {
                self.size_sync.fitted = None;
                return;
            };
            let p = *self.placement.lock().expect("placement poisoned");
            if p.width <= 0 || p.height <= 0 {
                return;
            }
            let area = (p.width.cast_unsigned(), p.height.cast_unsigned());
            let sync = &mut self.size_sync;
            let now = Instant::now();
            if !self.config.match_window_size || window.is_fullscreen() || window.is_maximized() {
                sync.last_area = area;
                sync.pending = false;
                sync.fitted = None;
                return;
            }
            if sync.fitted.is_none() {
                // Newly shown window (or matching just switched on): the viewer's current
                // size is where we start, not a resize to pass on.
                sync.last_area = area;
                sync.pending = false;
            }
            if area != sync.last_area {
                sync.last_area = area;
                if sync.self_resize_until.is_none_or(|t| now >= t) {
                    sync.changed_at = Some(now);
                    sync.pending = true;
                }
            }
            let remote = (shown.bounds.width, shown.bounds.height);
            if sync.pending {
                if sync
                    .changed_at
                    .is_some_and(|t| now.duration_since(t) >= RESIZE_SETTLE)
                {
                    sync.pending = false;
                    sync.fitted = Some((shown.id, area));
                    if !near(area, remote) {
                        tracing::debug!(?area, ?remote, "resizing host window to the viewer");
                        self.shared.resize_window(area.0, area.1);
                    }
                }
                return;
            }
            if sync.fitted == Some((shown.id, remote)) {
                return;
            }
            sync.fitted = Some((shown.id, remote));
            if near(area, remote) {
                return;
            }
            // Fit the video area to the window at 1:1, within the local display.
            let scale = window.scale_factor();
            let mut w = remote.0 as f32 / scale;
            let mut h = remote.1 as f32 / scale + BAR_PX;
            if let Some(display) = window.display(cx) {
                let d = display.bounds().size;
                w = w.min(f32::from(d.width) - 16.0);
                h = h.min(f32::from(d.height) - 80.0);
            }
            tracing::debug!(?area, ?remote, w, h, "resizing viewer to the host window");
            sync.self_resize_until = Some(now + SELF_RESIZE_GRACE);
            window.resize(size(px(w.max(200.0)), px(h.max(150.0))));
        }

        fn toggle_match_size(&mut self) {
            self.config.match_window_size = !self.config.match_window_size;
            self.config.save();
            self.size_sync = SizeSync::default();
        }

        fn toggle_picker(&mut self) {
            self.picker_open = !self.picker_open;
            if self.picker_open {
                self.shared.refresh_windows();
            }
        }

        fn select_window(&mut self, window: &peet_proto::WindowInfo) {
            self.picker_open = false;
            self.shared.select_window(window);
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
                self.local_moved_at = Some(Instant::now());
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

        /// Maps a window-space pointer position to host virtual-desktop pixels through the
        /// tiles the presenter published. Outside every tile the nearest one is used, clamped,
        /// so dragging past an edge keeps working. `None` before the first frame.
        fn window_to_host(&self, position: Point<Pixels>, window: &Window) -> Option<(i32, i32)> {
            let device_scale = f64::from(window.scale_factor());
            let dev_x = f64::from(f32::from(position.x)) * device_scale;
            let dev_y = f64::from(f32::from(position.y)) * device_scale;
            let tiles = self.shared.tiles();
            let tile = tiles
                .iter()
                .find(|t| {
                    dev_x >= f64::from(t.rect.left)
                        && dev_x < f64::from(t.rect.right)
                        && dev_y >= f64::from(t.rect.top)
                        && dev_y < f64::from(t.rect.bottom)
                })
                .or_else(|| {
                    tiles.iter().min_by(|a, b| {
                        rect_distance(a.rect, dev_x, dev_y)
                            .total_cmp(&rect_distance(b.rect, dev_x, dev_y))
                    })
                })?;
            let w = f64::from(tile.rect.right - tile.rect.left).max(1.0);
            let h = f64::from(tile.rect.bottom - tile.rect.top).max(1.0);
            let fx = ((dev_x - f64::from(tile.rect.left)) / w).clamp(0.0, 1.0);
            let fy = ((dev_y - f64::from(tile.rect.top)) / h).clamp(0.0, 1.0);
            let b = tile.bounds;
            let vx = f64::from(b.x) + fx * f64::from(b.width.saturating_sub(1));
            let vy = f64::from(b.y) + fy * f64::from(b.height.saturating_sub(1));
            Some((vx.round() as i32, vy.round() as i32))
        }

        /// The remote cursor (its shape, as the host reports it) as an absolutely positioned
        /// image over the video. With `follow_local` it sits at the local pointer while the
        /// viewer is the one moving it, so it moves without the round trip to the host;
        /// otherwise, and whenever the host moved it on its own, it sits where the host says.
        fn cursor_element(
            &self,
            window: &mut Window,
            follow_local: bool,
        ) -> Option<gpui::AnyElement> {
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
            let tiles = self.shared.tiles();
            let device_scale = f64::from(window.scale_factor());
            let tile_scale = |t: &crate::present::Tile| {
                f64::from(t.rect.right - t.rect.left) / f64::from(t.bounds.width.max(1))
            };
            let viewer_drives = match (self.local_moved_at, cursor.moved_at) {
                (Some(local), Some(remote)) => remote <= local + CURSOR_ECHO_GRACE,
                (Some(_), None) => true,
                (None, _) => false,
            };
            // The hotspot in device pixels relative to the window's client area, and the tile
            // it is drawn through.
            let local = if follow_local && viewer_drives {
                let pos = window.mouse_position();
                let x = f64::from(f32::from(pos.x)) * device_scale;
                let y = f64::from(f32::from(pos.y)) * device_scale;
                tiles
                    .iter()
                    .find(|t| {
                        x >= f64::from(t.rect.left)
                            && x < f64::from(t.rect.right)
                            && y >= f64::from(t.rect.top)
                            && y < f64::from(t.rect.bottom)
                    })
                    .map(|t| (t, x, y))
            } else {
                None
            };
            let (tile, hot_x, hot_y) = if let Some(hit) = local {
                hit
            } else {
                let tile = tiles.iter().find(|t| {
                    let b = t.bounds;
                    cursor.x >= b.x
                        && cursor.y >= b.y
                        && i64::from(cursor.x) < i64::from(b.x) + i64::from(b.width)
                        && i64::from(cursor.y) < i64::from(b.y) + i64::from(b.height)
                })?;
                let scale = tile_scale(tile);
                (
                    tile,
                    f64::from(tile.rect.left) + f64::from(cursor.x - tile.bounds.x) * scale,
                    f64::from(tile.rect.top) + f64::from(cursor.y - tile.bounds.y) * scale,
                )
            };
            let scale = tile_scale(tile);
            // Tiles are relative to the client area, but this element is positioned inside
            // the video area, which starts below the session bar.
            let origin = self.placement.lock().map(|p| *p).unwrap_or_default();
            let left =
                (hot_x - f64::from(cursor.hotspot.0) * scale - f64::from(origin.x)) / device_scale;
            let top =
                (hot_y - f64::from(cursor.hotspot.1) * scale - f64::from(origin.y)) / device_scale;
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

        fn pin_visible(&self) -> bool {
            self.pin_open || self.needs_pin || !self.pin.is_empty()
        }

        /// Hosts the search box narrows to: (target, discovered or saved name). Saved hosts
        /// first, then the rest of the tailnet.
        fn search_matches(&self, query: &str) -> Vec<(String, String)> {
            let (peers, this_device) = self
                .discovery
                .lock()
                .map(|d| (d.peers.clone(), d.this_device.clone()))
                .unwrap_or_default();
            let mut out: Vec<(String, String)> = Vec::new();
            for s in self.config.computers() {
                let peer = peers
                    .iter()
                    .chain(this_device.iter())
                    .find(|p| p.matches(&s.target));
                let base = peer.map_or(s.name.as_str(), |p| p.name.as_str());
                let shown = self.config.display_name(&s.target, base);
                let dns = peer.map_or("", |p| p.dns_name.as_str());
                if query_matches(query, &[&shown, base, &s.target, dns]) {
                    out.push((s.target.clone(), base.to_string()));
                }
            }
            for p in &peers {
                if out.iter().any(|(t, _)| p.matches(t)) {
                    continue;
                }
                let shown = self.config.display_name(&p.target, &p.name);
                if query_matches(query, &[&shown, &p.name, &p.target, &p.dns_name]) {
                    out.push((p.target.clone(), p.name.clone()));
                }
            }
            out
        }

        /// Opens the details panel for `target`; `edit` puts the cursor in its name.
        fn open_panel(&mut self, target: String, base: String, edit: bool) {
            self.commit_rename();
            self.name_edit = self.config.display_name(&target, &base);
            self.panel = Some(Panel { target, base });
            self.quality_open = false;
            if edit {
                self.field = Field::Name;
            }
        }

        fn close_panel(&mut self) {
            self.commit_rename();
            self.panel = None;
        }

        /// Saves the name being edited in the panel, if any.
        fn commit_rename(&mut self) {
            if self.field != Field::Name {
                return;
            }
            self.field = Field::Address;
            if let Some(p) = &self.panel {
                let (target, base) = (p.target.clone(), p.base.clone());
                self.config.rename(&target, &self.name_edit, &base);
                self.name_edit = self.config.display_name(&target, &base);
            }
        }

        /// Drops the name being edited and puts back the saved one.
        fn cancel_rename(&mut self) {
            self.field = Field::Address;
            if let Some(p) = &self.panel {
                self.name_edit = self.config.display_name(&p.target, &p.base);
            }
        }

        /// Clicks on the home screen's background close the popover and the panel.
        fn dismiss_overlays(&mut self) {
            self.quality_open = false;
            if self.panel.is_some() {
                self.close_panel();
            }
        }

        /// One saved host as a card: thumbnail, name, last connection, and on hover a way to
        /// connect, rename or see details.
        #[allow(clippy::too_many_arguments)]
        fn computer_card(
            &mut self,
            i: usize,
            saved: &home::Saved,
            peer: Option<&home::Peer>,
            now: u64,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> gpui::AnyElement {
            let target = saved.target.clone();
            let base = peer.map_or_else(|| saved.name.clone(), |p| p.name.clone());
            let name = self.config.display_name(&target, &base);
            let online = peer.map(|p| p.online);
            let os = peer.map_or("", |p| p.os.as_str()).to_string();
            let pinned = self.config.is_favourite(&target);
            let selected = self.panel.as_ref().is_some_and(|p| p.target == target);
            let thumb = self.thumbs.get(&target, window);
            let when = self.config.last_connected(&target);
            let last = ui::ago(when, now);
            let shown_last = if when == 0 {
                "not connected yet".to_string()
            } else {
                last.clone()
            };
            let sub = if name == base || base == target {
                format!("{target}  ·  {shown_last}")
            } else {
                format!("{base}  ·  {shown_last}")
            };
            let group = SharedString::from(format!("card-{i}"));

            let (t_connect, n_connect) = (target.clone(), base.clone());
            let (t_star, n_star) = (target.clone(), base.clone());
            let (t_edit, n_edit) = (target.clone(), base.clone());
            let (t_more, n_more) = (target.clone(), base.clone());

            let mut thumb_area = div()
                .relative()
                .w_full()
                .h(px(CARD_W * 10.0 / 16.0))
                .flex_none()
                .bg(rgb(0x0d0f12))
                .rounded_t_lg()
                .overflow_hidden();
            thumb_area = match thumb {
                Some(image) => thumb_area.child(
                    img(ImageSource::Render(image))
                        .size_full()
                        .object_fit(ObjectFit::Fill)
                        .rounded_t_lg()
                        .when(online == Some(false), |i| i.grayscale(true)),
                ),
                None => thumb_area.child(Self::no_preview(&os)),
            };
            if online == Some(false) {
                thumb_area =
                    thumb_area.child(div().absolute().inset_0().bg(ui::rgba_a(0x000000, 0.45)));
            }
            thumb_area = thumb_area
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(ui::rgba_a(0x080a0e, 0.45))
                        .opacity(0.0)
                        .group_hover(group.clone(), |s| s.opacity(1.0))
                        .child(ui::button(
                            ("card-connect", i),
                            Some("arrow-right"),
                            "Connect",
                            ui::Kind::Primary,
                        )),
                )
                .child(
                    Self::status_badge(online, &last)
                        .absolute()
                        .top(px(8.0))
                        .left(px(8.0)),
                )
                .child(
                    Self::badge()
                        .id(("card-star", i))
                        .absolute()
                        .top(px(8.0))
                        .right(px(8.0))
                        .p(px(4.0))
                        .cursor_pointer()
                        .tooltip(ui::tooltip(if pinned { "Unpin" } else { "Pin" }, None))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.config.toggle_favourite(&t_star, &n_star);
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .child(if pinned {
                            ui::icon("star-filled", 13.0, ui::color::STAR)
                        } else {
                            ui::icon("star", 13.0, ui::color::TEXT2)
                        }),
                );

            let meta = div()
                .relative()
                .px_3()
                .py(px(9.0))
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(if online == Some(false) {
                                    0x9aa0ac
                                } else {
                                    ui::color::TEXT
                                }))
                                .truncate()
                                .child(name),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(ui::color::MUTED))
                                .truncate()
                                .child(sub),
                        ),
                )
                .child(
                    // Over the end of the text, only while hovered (or selected), so the
                    // subtitle has the card's full width the rest of the time.
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right(px(6.0))
                        .pl_2()
                        .flex()
                        .items_center()
                        .gap(px(2.0))
                        .bg(rgb(ui::color::CARD))
                        .when(!selected, gpui::Styled::invisible)
                        .group_hover(group.clone(), gpui::Styled::visible)
                        .child(
                            ui::icon_button(("card-edit", i), "pencil", 13.0, false)
                                .tooltip(ui::tooltip("Rename", None))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_panel(t_edit.clone(), n_edit.clone(), true);
                                    cx.stop_propagation();
                                    cx.notify();
                                })),
                        )
                        .child(
                            ui::icon_button(("card-more", i), "ellipsis", 14.0, selected)
                                .tooltip(ui::tooltip("Details", None))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_panel(t_more.clone(), n_more.clone(), false);
                                    cx.stop_propagation();
                                    cx.notify();
                                })),
                        ),
                );

            div()
                .id(("card", i))
                .group(group)
                .w(px(CARD_W))
                .flex_none()
                .flex()
                .flex_col()
                .rounded_lg()
                .bg(rgb(ui::color::CARD))
                .border_1()
                .border_color(rgb(if selected {
                    ui::color::ACCENT
                } else {
                    ui::color::BORDER
                }))
                .hover(|s| s.border_color(rgb(0x3d4554)).shadow_lg())
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.connect(t_connect.clone(), n_connect.clone());
                    cx.notify();
                }))
                .child(thumb_area)
                .child(meta)
                .into_any_element()
        }

        /// Stand-in for a host that has no thumbnail yet.
        fn no_preview(os: &str) -> gpui::Div {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .bg(rgb(0x15181d))
                .text_xs()
                .text_color(rgb(ui::color::FAINT))
                .child(ui::icon(ui::os_icon(os), 22.0, ui::color::FAINT))
                .child("No preview yet")
        }

        /// A small translucent pill over a thumbnail.
        fn badge() -> gpui::Div {
            div()
                .flex()
                .items_center()
                .gap(px(5.0))
                .px(px(7.0))
                .py(px(2.0))
                .rounded_full()
                .bg(ui::rgba_a(0x0a0c0f, 0.78))
                .border_1()
                .border_color(ui::rgba_a(0xffffff, 0.08))
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(0xffffff))
        }

        fn status_badge(online: Option<bool>, last: &str) -> gpui::Div {
            match online {
                Some(true) => Self::badge()
                    .child(ui::dot(ui::color::GREEN, 7.0))
                    .child("Online"),
                Some(false) => Self::badge()
                    .child(ui::dot(ui::color::OFFLINE, 7.0))
                    .child(format!("Offline  ·  {last}")),
                None => Self::badge()
                    .child(ui::dot(0x5c6b85, 7.0))
                    .child("Not on tailnet"),
            }
        }

        /// One tailnet device that is not among the saved hosts.
        fn tailnet_row(
            &self,
            i: usize,
            p: &home::Peer,
            first: bool,
            cx: &mut Context<Self>,
        ) -> gpui::AnyElement {
            let name = self.config.display_name(&p.target, &p.name);
            let fav = self.config.is_favourite(&p.target);
            let (t1, n1) = (p.target.clone(), p.name.clone());
            let (t2, n2) = (p.target.clone(), p.name.clone());
            let (t3, n3) = (p.target.clone(), p.name.clone());
            let selected = self.panel.as_ref().is_some_and(|x| x.target == p.target);
            div()
                .id(("peer", i))
                .w_full()
                .flex()
                .items_center()
                .gap_3()
                .h(px(46.0))
                .px_3()
                .when(!first, |d| {
                    d.border_t_1().border_color(rgb(ui::color::BORDER))
                })
                .when(selected, |d| d.bg(ui::rgba_a(ui::color::ACCENT, 0.10)))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(0x20242c)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.connect(t1.clone(), n1.clone());
                    cx.notify();
                }))
                .child(
                    div()
                        .size(px(28.0))
                        .flex_none()
                        .rounded_md()
                        .bg(rgb(ui::color::RAISE))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(ui::icon(ui::os_icon(&p.os), 15.0, ui::color::TEXT2)),
                )
                .child(
                    div()
                        .w(px(190.0))
                        .flex_none()
                        .truncate()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(if p.online { ui::color::TEXT } else { 0x777d88 }))
                        .child(name),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(rgb(if p.online {
                            ui::color::MUTED
                        } else {
                            ui::color::FAINT
                        }))
                        .child(p.dns_name.clone()),
                )
                .child(
                    div()
                        .w(px(110.0))
                        .flex_none()
                        .text_xs()
                        .text_color(rgb(ui::color::MUTED))
                        .child(p.target.clone()),
                )
                .child(
                    div()
                        .w(px(70.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_xs()
                        .text_color(rgb(if p.online {
                            ui::color::TEXT2
                        } else {
                            ui::color::FAINT
                        }))
                        .child(ui::dot(
                            if p.online {
                                ui::color::GREEN
                            } else {
                                ui::color::OFFLINE
                            },
                            7.0,
                        ))
                        .child(if p.online { "Online" } else { "Offline" }),
                )
                .child(
                    div()
                        .id(("peer-star", i))
                        .size(px(26.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .hover(|s| s.bg(rgb(ui::color::RAISE)))
                        .tooltip(ui::tooltip(if fav { "Unpin" } else { "Pin" }, None))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.config.toggle_favourite(&t2, &n2);
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .child(if fav {
                            ui::icon("star-filled", 14.0, ui::color::STAR)
                        } else {
                            ui::icon("star", 14.0, ui::color::FAINT)
                        }),
                )
                .child(
                    ui::icon_button(("peer-more", i), "ellipsis", 14.0, selected)
                        .size(px(26.0))
                        .tooltip(ui::tooltip("Details", None))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_panel(t3.clone(), n3.clone(), false);
                            cx.stop_propagation();
                            cx.notify();
                        })),
                )
                .into_any_element()
        }

        /// A home-screen text box. Typing goes to whichever box is active; clicking one
        /// activates it. The PIN box shows dots and turns red when the host asked for a PIN.
        fn text_box(
            &self,
            id: &'static str,
            field: Field,
            placeholder: &'static str,
            leading: Option<&'static str>,
            cx: &mut Context<Self>,
        ) -> gpui::Stateful<gpui::Div> {
            let active = self.field == field;
            let value = match field {
                Field::Address => &self.address,
                Field::Pin => &self.pin,
                Field::Name => &self.name_edit,
            };
            let shown = if field == Field::Pin {
                "•".repeat(value.chars().count())
            } else {
                value.clone()
            };
            let caret = || div().w(px(1.0)).h(px(16.0)).flex_none().bg(rgb(0xffffff));
            let mut content = div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .overflow_hidden();
            content = if value.is_empty() {
                content.when(active, |c| c.child(caret())).child(
                    div()
                        .truncate()
                        .text_color(rgb(0x6a7080))
                        .child(placeholder),
                )
            } else {
                content
                    .child(div().truncate().text_color(rgb(0xf0f0f0)).child(shown))
                    .when(active, |c| c.child(caret()))
            };
            let border = if active {
                ui::color::ACCENT
            } else if field == Field::Pin && self.needs_pin {
                ui::color::ERROR
            } else {
                ui::color::BORDER2
            };
            div()
                .id(id)
                .h(px(36.0))
                .px_3()
                .flex()
                .items_center()
                .gap_2()
                .rounded_lg()
                .bg(rgb(ui::color::FIELD))
                .border_1()
                .border_color(rgb(border))
                .cursor_text()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if field != Field::Name {
                        this.commit_rename();
                    }
                    this.field = field;
                    cx.notify();
                }))
                .children(leading.map(|name| ui::icon(name, 15.0, ui::color::MUTED)))
                .child(content)
        }

        /// The stream-quality button and, when open, its popover of presets.
        fn quality_menu(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
            let label = format!(
                "{} fps  ·  {} Mbit/s",
                self.defaults.fps,
                self.defaults.bitrate_kbps / 1000
            );
            let button = div()
                .id("quality")
                .h(px(36.0))
                .px_3()
                .flex()
                .items_center()
                .gap_2()
                .rounded_lg()
                .border_1()
                .border_color(rgb(if self.quality_open {
                    ui::color::ACCENT
                } else {
                    ui::color::BORDER2
                }))
                .bg(rgb(ui::color::FIELD))
                .hover(|s| s.bg(rgb(0x20242b)))
                .cursor_pointer()
                .whitespace_nowrap()
                .text_sm()
                .text_color(rgb(ui::color::TEXT2))
                .when(!self.quality_open, |b| {
                    b.tooltip(ui::tooltip("Stream quality for new connections", None))
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.quality_open = !this.quality_open;
                    cx.notify();
                }))
                .child(ui::icon("sliders", 15.0, ui::color::TEXT2))
                .child(label)
                .child(ui::icon("chevron-down", 14.0, ui::color::MUTED));
            let mut wrap = div().relative().flex_none().child(button);
            if self.quality_open {
                let mut fps_row = div().flex().gap_1();
                for (i, &fps) in FPS_PRESETS.iter().enumerate() {
                    fps_row = fps_row.child(Self::chip(
                        ("fps", i),
                        format!("{fps} fps"),
                        self.defaults.fps == fps,
                        cx.listener(move |this, _, _, cx| {
                            this.set_fps(fps);
                            cx.notify();
                        }),
                    ));
                }
                let mut kbps_row = div().flex().flex_wrap().gap_1();
                for (i, &kbps) in BITRATE_PRESETS.iter().enumerate() {
                    kbps_row = kbps_row.child(Self::chip(
                        ("kbps", i),
                        format!("{} Mbit/s", kbps / 1000),
                        self.defaults.bitrate_kbps == kbps,
                        cx.listener(move |this, _, _, cx| {
                            this.set_bitrate(kbps);
                            cx.notify();
                        }),
                    ));
                }
                let label = |t: &'static str| {
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(ui::color::MUTED))
                        .child(t)
                };
                wrap = wrap.child(deferred(
                    div()
                        .id("quality-menu")
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .absolute()
                        .top(px(42.0))
                        .right_0()
                        .w(px(300.0))
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .rounded_lg()
                        .bg(rgb(0x23272f))
                        .border_1()
                        .border_color(rgb(0x3a404c))
                        .shadow_lg()
                        .child(label("FRAME RATE"))
                        .child(fps_row)
                        .child(div().h(px(4.0)))
                        .child(label("BITRATE"))
                        .child(kbps_row)
                        .child(
                            div()
                                .pt_1()
                                .text_xs()
                                .text_color(rgb(ui::color::FAINT))
                                .child("Applies to the next connection."),
                        ),
                ));
            }
            wrap.into_any_element()
        }

        fn section(icon: &'static str, title: &'static str, detail: Option<String>) -> gpui::Div {
            div()
                .mt_5()
                .mb(px(10.0))
                .flex()
                .items_center()
                .gap_2()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(ui::color::MUTED))
                .child(ui::icon(icon, 13.0, ui::color::MUTED))
                .child(title)
                .children(detail.map(|d| {
                    div()
                        .font_weight(FontWeight::NORMAL)
                        .text_color(rgb(ui::color::FAINT))
                        .child(d)
                }))
                .child(div().flex_1().h(px(1.0)).bg(rgb(ui::color::BORDER)))
        }

        /// The details panel on the right: preview, editable name, facts, pin, connect, forget.
        fn side_panel(
            &mut self,
            peers: &[home::Peer],
            now: u64,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Option<gpui::AnyElement> {
            let Panel { target, base } = self.panel.clone()?;
            let peer = peers.iter().find(|p| p.matches(&target));
            let thumb = self.thumbs.get(&target, window);
            let renamed = self.config.names.contains_key(&target);
            let pinned = self.config.is_favourite(&target);
            let known = renamed || pinned || self.config.recents.iter().any(|r| r.target == target);
            let editing = self.field == Field::Name;
            let online = peer.map(|p| p.online);
            let last = ui::ago(self.config.last_connected(&target), now);
            let os = peer.map_or("", |p| p.os.as_str()).to_string();

            let mut preview = div()
                .relative()
                .w_full()
                .h(px(PANEL_THUMB_W * 10.0 / 16.0))
                .rounded_lg()
                .overflow_hidden()
                .border_1()
                .border_color(rgb(ui::color::BORDER))
                .bg(rgb(0x0d0f12));
            preview = match thumb {
                Some(image) => preview.child(
                    img(ImageSource::Render(image))
                        .size_full()
                        .object_fit(ObjectFit::Fill)
                        .rounded_lg(),
                ),
                None => preview.child(Self::no_preview(&os)),
            };
            preview = preview.child(
                Self::status_badge(online, &last)
                    .absolute()
                    .top(px(8.0))
                    .left(px(8.0)),
            );

            let name_box = self
                .text_box("panel-name", Field::Name, "Name", None, cx)
                .h(px(40.0))
                .text_lg()
                .font_weight(FontWeight::BOLD)
                .when(!editing, |b| {
                    b.child(ui::icon("pencil", 14.0, ui::color::MUTED))
                });
            let hint = if editing {
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(ui::kbd("Enter"))
                    .child("save")
                    .child(div().w(px(6.0)))
                    .child(ui::kbd("Esc"))
                    .child("cancel")
                    .child(div().w(px(6.0)))
                    .child(format!("·  hostname {base}"))
            } else if renamed {
                let (t, b) = (target.clone(), base.clone());
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(format!("Hostname {base}  ·"))
                    .child(
                        div()
                            .id("panel-reset")
                            .text_color(rgb(ui::color::ACCENT_SOFT))
                            .cursor_pointer()
                            .hover(gpui::Styled::underline)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.config.rename(&t, "", &b);
                                this.name_edit.clone_from(&b);
                                cx.notify();
                            }))
                            .child("use hostname"),
                    )
            } else {
                div().child("Click the name to rename this computer.")
            };

            let kv = |k: &'static str, v: gpui::AnyElement| {
                div()
                    .h(px(36.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .border_t_1()
                    .border_color(rgb(ui::color::BORDER))
                    .text_sm()
                    .child(div().flex_none().text_color(rgb(ui::color::MUTED)).child(k))
                    .child(v)
            };
            let text = |s: String| {
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(rgb(ui::color::TEXT))
                    .child(s)
                    .into_any_element()
            };
            let status = match online {
                Some(true) => "Online",
                Some(false) => "Offline",
                None => "Not seen on the tailnet",
            };
            let (t_pin, n_pin) = (target.clone(), base.clone());
            let toggle = div()
                .id("panel-pin")
                .w(px(34.0))
                .h(px(20.0))
                .rounded_full()
                .relative()
                .cursor_pointer()
                .bg(rgb(if pinned { ui::color::ACCENT } else { 0x3a404c }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.config.toggle_favourite(&t_pin, &n_pin);
                    cx.notify();
                }))
                .child(
                    div()
                        .absolute()
                        .top(px(3.0))
                        .when(pinned, |d| d.right(px(3.0)))
                        .when(!pinned, |d| d.left(px(3.0)))
                        .size(px(14.0))
                        .rounded_full()
                        .bg(rgb(if pinned { 0xffffff } else { 0x9aa0ac })),
                );

            let (t_conn, n_conn) = (target.clone(), base.clone());
            let t_forget = target.clone();
            let footer = div()
                .px(px(18.0))
                .pb(px(18.0))
                .flex()
                .gap_2()
                .child(
                    ui::button(
                        "panel-connect",
                        Some("arrow-right"),
                        "Connect",
                        ui::Kind::Primary,
                    )
                    .h(px(36.0))
                    .flex_1()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_panel();
                        this.connect(t_conn.clone(), n_conn.clone());
                        cx.notify();
                    })),
                )
                .when(known, |f| {
                    f.child(
                        ui::icon_button("panel-forget", "trash", 15.0, false)
                            .size(px(36.0))
                            .border_1()
                            .border_color(rgb(ui::color::BORDER2))
                            .bg(rgb(ui::color::RAISE))
                            .tooltip(ui::tooltip(
                                "Forget: unpin, clear the name and preview",
                                None,
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.field = Field::Address;
                                this.panel = None;
                                this.config.forget(&t_forget);
                                thumbs::remove(&t_forget);
                                this.thumbs.forget(&t_forget, window);
                                cx.notify();
                            })),
                    )
                });

            Some(
                div()
                    .id("panel")
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .w(px(PANEL_THUMB_W + 36.0))
                    .flex()
                    .flex_col()
                    .bg(rgb(0x191c22))
                    .border_l_1()
                    .border_color(rgb(0x333944))
                    .shadow_lg()
                    .child(
                        div()
                            .pl(px(18.0))
                            .pr(px(10.0))
                            .pt(px(10.0))
                            .pb(px(8.0))
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(ui::color::MUTED))
                                    .child("COMPUTER"),
                            )
                            .child(
                                ui::icon_button("panel-close", "x", 16.0, false)
                                    .tooltip(ui::tooltip("Close", Some("Esc".into())))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.close_panel();
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(div().px(px(18.0)).child(preview))
                    .child(
                        div().px(px(18.0)).pt(px(14.0)).child(name_box).child(
                            div()
                                .mt(px(6.0))
                                .mb(px(12.0))
                                .px(px(2.0))
                                .text_xs()
                                .text_color(rgb(ui::color::MUTED))
                                .child(hint),
                        ),
                    )
                    .child(
                        div()
                            .id("panel-facts")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px(px(18.0))
                            .child(kv("Address", text(target.clone())))
                            .child(kv(
                                "MagicDNS",
                                text(peer.map_or_else(|| "—".into(), |p| p.dns_name.clone())),
                            ))
                            .child(kv("System", text(ui::os_name(&os))))
                            .child(kv("Status", text(status.into())))
                            .child(kv("Last connected", text(last.clone())))
                            .child(kv("Pinned", toggle.into_any_element())),
                    )
                    .child(footer)
                    .into_any_element(),
            )
        }

        /// A small clickable pill, highlighted when selected.
        fn chip(
            id: (&'static str, usize),
            label: String,
            selected: bool,
            on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        ) -> gpui::AnyElement {
            ui::button(
                id,
                None,
                label,
                if selected {
                    ui::Kind::Selected
                } else {
                    ui::Kind::Ghost
                },
            )
            .h(px(26.0))
            .px_2()
            .font_weight(FontWeight::MEDIUM)
            .on_click(on_click)
            .into_any_element()
        }

        fn render_home(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
            let (peers, this_device, disc_error, loaded) = self
                .discovery
                .lock()
                .map(|d| {
                    (
                        d.peers.clone(),
                        d.this_device.clone(),
                        d.error.clone(),
                        d.ever_loaded,
                    )
                })
                .unwrap_or_default();
            // Saved hosts can be this machine; the tailnet list never shows it.
            let known: Vec<home::Peer> = peers.iter().cloned().chain(this_device).collect();
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let query = self.address.trim().to_string();

            // Saved hosts as cards, the rest of the tailnet as rows; the search box filters both.
            let computers = self.config.computers();
            let mut cards = Vec::new();
            for (i, s) in computers.iter().enumerate() {
                let peer = known.iter().find(|p| p.matches(&s.target));
                let base = peer.map_or(s.name.as_str(), |p| p.name.as_str());
                let shown = self.config.display_name(&s.target, base);
                let dns = peer.map_or("", |p| p.dns_name.as_str());
                if !query_matches(&query, &[&shown, base, &s.target, dns]) {
                    continue;
                }
                cards.push(self.computer_card(i, s, peer, now, window, cx));
            }
            let others: Vec<&home::Peer> = peers
                .iter()
                .filter(|p| !computers.iter().any(|s| p.matches(&s.target)))
                .collect();
            let rows: Vec<_> = others
                .iter()
                .filter(|p| {
                    let shown = self.config.display_name(&p.target, &p.name);
                    query_matches(&query, &[&shown, &p.name, &p.target, &p.dns_name])
                })
                .enumerate()
                .map(|(i, p)| self.tailnet_row(i, p, i == 0, cx))
                .collect();

            let pin_box = self.pin_visible().then(|| {
                self.text_box("pin", Field::Pin, "PIN", Some("lock"), cx)
                    .w(px(130.0))
                    .flex_none()
            });
            let header = div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .flex_none()
                        .mr_2()
                        .child(img("brand/icon.png").size(px(26.0)))
                        .child(
                            div()
                                .text_lg()
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(0xf0f0f0))
                                .child("PeetConnect"),
                        ),
                )
                .child(
                    self.text_box(
                        "address",
                        Field::Address,
                        "Search, or type a MagicDNS name or Tailscale IP",
                        Some("search"),
                        cx,
                    )
                    .flex_1()
                    .min_w_0()
                    .child(ui::kbd("Enter")),
                )
                .children(pin_box)
                .when(!self.pin_visible(), |h| {
                    h.child(
                        ui::icon_button("pin-toggle", "lock", 15.0, false)
                            .size(px(36.0))
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(ui::color::BORDER2))
                            .bg(rgb(ui::color::FIELD))
                            .tooltip(ui::tooltip("Enter a PIN", None))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.commit_rename();
                                this.pin_open = true;
                                this.field = Field::Pin;
                                cx.notify();
                            })),
                    )
                })
                .child(self.quality_menu(cx));

            let mut column = div()
                .flex()
                .flex_col()
                .w(px(GRID_W))
                .max_w_full()
                .child(header);
            for err in self.last_error.iter().chain(disc_error.iter()) {
                column = column.child(
                    div()
                        .mt_3()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(ui::rgba_a(ui::color::ERROR, 0.08))
                        .border_1()
                        .border_color(ui::rgba_a(ui::color::ERROR, 0.25))
                        .text_sm()
                        .text_color(rgb(ui::color::ERROR))
                        .child(err.clone()),
                );
            }

            column = column.child(Self::section(
                "monitor",
                "COMPUTERS",
                Some("pinned first, then recent".into()),
            ));
            if computers.is_empty() {
                column = column.child(
                    div()
                        .px_4()
                        .py_5()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(ui::color::BORDER))
                        .text_sm()
                        .text_color(rgb(ui::color::MUTED))
                        .child(
                            "Computers you connect to or pin show up here, with a preview of \
                             their screen.",
                        ),
                );
            } else if cards.is_empty() {
                column = column.child(
                    div()
                        .px_1()
                        .text_sm()
                        .text_color(rgb(ui::color::MUTED))
                        .child(format!(
                            "No saved computer matches \u{201c}{query}\u{201d}."
                        )),
                );
            } else {
                column = column.child(div().flex().flex_wrap().gap(px(CARD_GAP)).children(cards));
            }

            let refresh = div()
                .flex()
                .items_center()
                .gap(px(5.0))
                .font_weight(FontWeight::NORMAL)
                .child(ui::icon("refresh", 12.0, ui::color::FAINT))
                .child("live");
            column = column.child(
                Self::section(
                    "network",
                    "TAILNET",
                    Some(format!("{} more devices", others.len())),
                )
                .child(refresh),
            );
            if !loaded {
                column = column.child(
                    div()
                        .px_1()
                        .text_sm()
                        .text_color(rgb(ui::color::MUTED))
                        .child("looking for devices"),
                );
            } else if rows.is_empty() && disc_error.is_none() {
                column = column.child(
                    div()
                        .px_1()
                        .text_sm()
                        .text_color(rgb(ui::color::MUTED))
                        .child(if others.is_empty() {
                            "no other devices on the tailnet".to_string()
                        } else {
                            format!("no tailnet device matches \u{201c}{query}\u{201d}")
                        }),
                );
            } else if !rows.is_empty() {
                column = column.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .rounded_lg()
                        .overflow_hidden()
                        .bg(rgb(ui::color::PANEL))
                        .border_1()
                        .border_color(rgb(ui::color::BORDER))
                        .children(rows),
                );
            }

            let mut keys = div()
                .mt_6()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_x_4()
                .gap_y_2()
                .text_xs()
                .text_color(rgb(ui::color::FAINT))
                .child("In a session:");
            for (key, what) in [
                ("Ctrl Alt 1–9", "one display"),
                ("Ctrl Alt 0", "all displays"),
                ("Ctrl Alt W", "pick a window"),
                ("Ctrl Alt F", "fullscreen"),
                ("Ctrl Alt D", "disconnect"),
            ] {
                keys = keys.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(ui::kbd(key))
                        .child(what),
                );
            }
            column = column.child(keys).child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(rgb(ui::color::FAINT))
                    .child("Clipboard text syncs both ways."),
            );

            let panel = self.side_panel(&known, now, window, cx);
            div()
                .size_full()
                .relative()
                .bg(rgb(ui::color::BG))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.dismiss_overlays();
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .id("home")
                        .size_full()
                        .overflow_y_scroll()
                        .flex()
                        .justify_center()
                        .pt(px(22.0))
                        .pb_8()
                        .px_6()
                        .child(column),
                )
                .children(panel)
                .into_any_element()
        }

        // ---- session overlays ------------------------------------------------------------

        /// A card over the video when the shown window is minimised or closed on the host.
        fn window_card(
            s: &remote::RemoteStatus,
            cx: &mut Context<Self>,
        ) -> Option<gpui::AnyElement> {
            let w = s.window.as_ref()?;
            let (heading, detail, action): (String, &str, gpui::AnyElement) = if s.window_closed {
                (
                    format!("{} was closed", short_title(&w.title, 60)),
                    "The window is gone from the host.",
                    Self::chip(
                        ("card", 0),
                        "Show desktop".into(),
                        false,
                        cx.listener(|this, _, _, _| this.show_desktop()),
                    ),
                )
            } else if w.minimized {
                (
                    format!("{} is minimised", short_title(&w.title, 60)),
                    "Minimised windows send no video.",
                    Self::chip(
                        ("card", 0),
                        "Restore".into(),
                        false,
                        cx.listener(|this, _, _, _| this.shared.focus_window()),
                    ),
                )
            } else {
                return None;
            };
            Some(
                div()
                    .id("window-card")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgba(0x0000_0099))
                    .child(Self::overlay_card(heading, detail.to_string()).child(
                        div().flex().gap_2().child(action).child(Self::chip(
                            ("card", 1),
                            "Pick another window".into(),
                            false,
                            cx.listener(|this, _, _, _| {
                                this.picker_open = true;
                                this.shared.refresh_windows();
                            }),
                        )),
                    ))
                    .into_any_element(),
            )
        }

        /// The box used by the session's centred messages.
        fn overlay_card(heading: String, detail: String) -> gpui::Div {
            div()
                .px_6()
                .py_4()
                .rounded_lg()
                .bg(rgb(ui::color::CARD))
                .border_1()
                .border_color(rgb(ui::color::BORDER2))
                .shadow_lg()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(0xf0f0f0))
                        .child(heading),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(ui::color::MUTED))
                        .child(detail),
                )
        }

        /// The host's windows, front to back, in a panel under the bar's window button.
        fn window_picker(s: &remote::RemoteStatus, cx: &mut Context<Self>) -> gpui::AnyElement {
            let mut list = div()
                .id("window-list")
                .flex()
                .flex_col()
                .p_1()
                .max_h(px(420.0))
                .overflow_y_scroll();
            if s.windows.is_empty() {
                list = list.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(rgb(ui::color::MUTED))
                        .child(if s.windows_loading {
                            "listing windows"
                        } else {
                            "no windows to show"
                        }),
                );
            }
            let shown = s.window.as_ref().map(|w| w.id);
            for (i, w) in s.windows.iter().enumerate() {
                let selected = shown == Some(w.id) && !s.window_closed;
                let window = w.clone();
                let mut detail = w.process.clone();
                if w.minimized {
                    if !detail.is_empty() {
                        detail.push_str("  ·  ");
                    }
                    detail.push_str("minimised");
                }
                list = list.child(
                    div()
                        .id(("window", i))
                        .px_2()
                        .py_1()
                        .flex()
                        .items_center()
                        .gap_3()
                        .rounded_md()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(rgb(ui::color::ACCENT_BG)))
                        .when(!selected, |d| d.hover(|st| st.bg(rgb(ui::color::RAISE))))
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.select_window(&window);
                        }))
                        .child(ui::icon(
                            "app-window",
                            16.0,
                            if selected { 0xffffff } else { ui::color::MUTED },
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_sm()
                                        .truncate()
                                        .text_color(rgb(0xf0f0f0))
                                        .child(short_title(&w.title, 70)),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(if selected {
                                            0xc9d6ff
                                        } else {
                                            ui::color::MUTED
                                        }))
                                        .child(detail),
                                ),
                        ),
                );
            }
            div()
                .id("window-picker")
                .absolute()
                .top(px(6.0))
                .right(px(8.0))
                .occlude()
                .w(px(420.0))
                .rounded_lg()
                .bg(rgb(0x1f232b))
                .border_1()
                .border_color(rgb(0x3a404c))
                .shadow_lg()
                .flex()
                .flex_col()
                .child(
                    div()
                        .pl_3()
                        .pr_1()
                        .py_1()
                        .flex()
                        .items_center()
                        .gap_1()
                        .border_b_1()
                        .border_color(rgb(ui::color::BORDER2))
                        .child(ui::icon("app-window", 14.0, ui::color::MUTED))
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Host windows"),
                        )
                        .child(
                            ui::icon_button(("picker", 0usize), "refresh", 14.0, false)
                                .tooltip(ui::tooltip("Refresh", None))
                                .on_click(
                                    cx.listener(|this, _, _, _| this.shared.refresh_windows()),
                                ),
                        )
                        .child(
                            ui::icon_button(("picker", 1usize), "x", 15.0, false)
                                .tooltip(ui::tooltip("Close", None))
                                .on_click(cx.listener(|this, _, _, _| this.picker_open = false)),
                        ),
                )
                .child(list)
                .into_any_element()
        }

        /// The bar above the video: host and link quality on the left, view and session
        /// controls on the right.
        fn session_bar(
            &self,
            s: Option<&remote::RemoteStatus>,
            fullscreen: bool,
            cx: &mut Context<Self>,
        ) -> gpui::Div {
            let bar = div()
                .h(px(BAR_PX))
                .flex_none()
                .pl_3()
                .pr_2()
                .flex()
                .items_center()
                .gap_1()
                .text_sm()
                .bg(rgb(ui::color::BAR))
                .border_b_1()
                .border_color(rgb(0x262a32));
            let Some(s) = s else {
                // Loopback rigs: just the measurements.
                return bar.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(ui::color::TEXT2))
                        .child(self.status.clone()),
                );
            };

            // The left side gives way first: a viewer fitted to a narrow window must still
            // reach the controls.
            let mut left = div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .overflow_hidden()
                .child(ui::dot(
                    if s.connected {
                        ui::color::GREEN
                    } else {
                        ui::color::AMBER
                    },
                    8.0,
                ))
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(ui::color::TEXT))
                        .child(self.session_target.1.clone()),
                );
            if s.connected {
                let rtt_color = match s.rtt_ms {
                    Some(ms) if ms < 40.0 => ui::color::GREEN,
                    Some(ms) if ms < 100.0 => ui::color::AMBER,
                    Some(_) => ui::color::RED,
                    None => ui::color::MUTED,
                };
                let vsep = || div().w(px(1.0)).h(px(11.0)).bg(rgb(ui::color::BORDER2));
                let mut pill = div()
                    .id("stats")
                    .h(px(24.0))
                    .px(px(9.0))
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .overflow_hidden()
                    .rounded_full()
                    .bg(rgb(ui::color::WELL))
                    .border_1()
                    .border_color(rgb(ui::color::BORDER))
                    .text_xs()
                    .whitespace_nowrap()
                    .text_color(rgb(ui::color::TEXT2))
                    .tooltip(ui::tooltip(self.status.clone(), None))
                    .child(ui::icon("signal", 13.0, rtt_color));
                if let Some(ms) = s.rtt_ms {
                    pill = pill
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(rtt_color))
                                .child(format!("{ms:.0} ms")),
                        )
                        .child(vsep());
                }
                pill = pill
                    .child(format!("{:.0} fps", s.fps))
                    .child(vsep())
                    .child(format!("{:.1} Mbit/s", f64::from(s.kbps) / 1000.0));
                left = left.child(pill);
            } else {
                left = left.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(rgb(ui::color::MUTED))
                        .child(self.status.clone()),
                );
            }

            let mut right = div().flex_none().flex().items_center().gap_1();
            if s.connected {
                let on_monitors = s.window.is_none();
                if s.monitors.len() > 1 {
                    let mut seg = div()
                        .flex()
                        .items_center()
                        .p(px(2.0))
                        .gap(px(2.0))
                        .rounded(px(7.0))
                        .bg(rgb(ui::color::WELL))
                        .border_1()
                        .border_color(rgb(ui::color::BORDER))
                        .child(div().px(px(5.0)).child(ui::icon(
                            "monitor",
                            13.0,
                            ui::color::MUTED,
                        )));
                    for (i, m) in s.monitors.iter().take(9).enumerate() {
                        let id = m.id;
                        let selected = on_monitors && !s.all_monitors && s.current == Some(id);
                        seg = seg.child(
                            Self::seg_item(("mon", i), selected)
                                .tooltip(ui::tooltip(
                                    format!("Display {}", i + 1),
                                    Some(format!("Ctrl Alt {}", i + 1).into()),
                                ))
                                .on_click(cx.listener(move |this, _, _, _| this.select_monitor(id)))
                                .child(format!("{}", i + 1)),
                        );
                    }
                    let all = on_monitors && s.all_monitors;
                    seg = seg.child(
                        Self::seg_item(("mon", 99usize), all)
                            .tooltip(ui::tooltip("All displays", Some("Ctrl Alt 0".into())))
                            .on_click(cx.listener(|this, _, _, _| {
                                this.picker_open = false;
                                this.shared.select_all();
                            }))
                            .child(ui::icon(
                                "layout-grid",
                                13.0,
                                if all { 0xffffff } else { ui::color::TEXT2 },
                            )),
                    );
                    right = right.child(seg);
                } else if !on_monitors {
                    right = right.child(
                        ui::icon_button(("mon", 98usize), "monitor", 16.0, false)
                            .tooltip(ui::tooltip("Show the desktop", None))
                            .on_click(cx.listener(|this, _, _, _| this.show_desktop())),
                    );
                }
                if s.windows_supported {
                    let label = s
                        .window
                        .as_ref()
                        .filter(|_| !s.window_closed)
                        .map_or_else(|| "Window".to_string(), |w| process_label(&w.process));
                    let active = !on_monitors || self.picker_open;
                    right = right.child(div().w(px(4.0))).child(
                        ui::button(
                            ("mon", 97usize),
                            Some("app-window"),
                            label,
                            if active {
                                ui::Kind::Selected
                            } else {
                                ui::Kind::Ghost
                            },
                        )
                        .max_w(px(190.0))
                        .px(px(8.0))
                        .font_weight(FontWeight::MEDIUM)
                        .tooltip(ui::tooltip("Show one window", Some("Ctrl Alt W".into())))
                        .on_click(cx.listener(|this, _, _, _| this.toggle_picker()))
                        .child(ui::icon(
                            "chevron-down",
                            13.0,
                            ui::color::ACCENT_SOFT,
                        )),
                    );
                }
                right = right.child(ui::separator());
                if s.window.is_some() {
                    right = right.child(
                        ui::icon_button(
                            ("bar", 2usize),
                            "scaling",
                            16.0,
                            self.config.match_window_size,
                        )
                        .tooltip(ui::tooltip(
                            if self.config.match_window_size {
                                "Match window size: on"
                            } else {
                                "Match window size: off"
                            },
                            None,
                        ))
                        .on_click(cx.listener(|this, _, _, _| this.toggle_match_size())),
                    );
                }
            }
            right = right
                .child(
                    ui::icon_button(
                        ("bar", 0usize),
                        if fullscreen { "minimize" } else { "maximize" },
                        16.0,
                        false,
                    )
                    .tooltip(ui::tooltip(
                        if fullscreen {
                            "Exit fullscreen"
                        } else {
                            "Fullscreen"
                        },
                        Some("Ctrl Alt F".into()),
                    ))
                    .on_click(|_, window, _| window.toggle_fullscreen()),
                )
                .child(ui::separator())
                .child(
                    ui::button(
                        ("bar", 1usize),
                        Some("power"),
                        "Disconnect",
                        ui::Kind::Danger,
                    )
                    .tooltip(ui::tooltip("End the session", Some("Ctrl Alt D".into())))
                    .on_click(cx.listener(|this, _, _, _| this.disconnect())),
                );
            bar.child(left).child(right)
        }

        /// One button in the bar's display switcher.
        fn seg_item(id: (&'static str, usize), selected: bool) -> gpui::Stateful<gpui::Div> {
            div()
                .id(id)
                .h(px(24.0))
                .min_w(px(28.0))
                .px(px(7.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.0))
                .cursor_pointer()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .when(selected, |d| {
                    d.bg(rgb(ui::color::RAISE)).text_color(rgb(0xffffff))
                })
                .when(!selected, |d| {
                    d.text_color(rgb(ui::color::TEXT2))
                        .hover(|s| s.bg(rgb(0x1d2027)))
                })
        }

        fn track_pointer(&mut self, event: &MouseMoveEvent, _: &mut Window, _: &mut Context<Self>) {
            self.pointer_y = f32::from(event.position.y);
        }
    }

    /// Case-insensitive substring search over a host's names; an empty query matches all.
    fn query_matches(query: &str, fields: &[&str]) -> bool {
        if query.is_empty() {
            return true;
        }
        let q = query.to_lowercase();
        fields.iter().any(|f| f.to_lowercase().contains(&q))
    }

    /// "Notepad" from "Notepad.exe": a short name for the window button.
    fn process_label(process: &str) -> String {
        let stem = process
            .rsplit_once('.')
            .filter(|(_, ext)| ext.eq_ignore_ascii_case("exe"))
            .map_or(process, |(stem, _)| stem);
        if stem.is_empty() {
            "Window".into()
        } else {
            short_title(stem, 18)
        }
    }

    /// Distance from a point to a rectangle (zero inside).
    fn rect_distance(r: windows::Win32::Foundation::RECT, x: f64, y: f64) -> f64 {
        let dx = (f64::from(r.left) - x).max(0.0).max(x - f64::from(r.right));
        let dy = (f64::from(r.top) - y).max(0.0).max(y - f64::from(r.bottom));
        dx.hypot(dy)
    }

    /// Translates a GPUI mouse button to the protocol's.
    /// State for keeping the viewer and a shown host window the same size.
    #[derive(Debug, Default)]
    struct SizeSync {
        /// Video area (physical pixels) as last seen.
        last_area: (u32, u32),
        /// The user changed the video area and it has not been sent yet.
        pending: bool,
        changed_at: Option<Instant>,
        /// Until then, video-area changes come from the viewer's own resize.
        self_resize_until: Option<Instant>,
        /// The window and remote size the two were last matched at.
        fitted: Option<(peet_proto::WindowId, (u32, u32))>,
    }

    /// Sizes within a couple of pixels count as equal (DPI rounding).
    fn near(a: (u32, u32), b: (u32, u32)) -> bool {
        a.0.abs_diff(b.0) <= 2 && a.1.abs_diff(b.1) <= 2
    }

    /// `title` cut to at most `max` characters, with an ellipsis when cut.
    fn short_title(title: &str, max: usize) -> String {
        if title.chars().count() <= max {
            return title.to_string();
        }
        let mut cut: String = title.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }

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
                    self.adopt_host_name();
                }
            }
            self.update_status();
            if self.mode == Mode::Remote && self.screen == Screen::Session {
                self.sync_window_size(window, cx);
            }
            if self.mode == Mode::Remote {
                let wanted = match self.screen {
                    Screen::Home => "PeetConnect".to_string(),
                    Screen::Session => match self.shared.status().window {
                        Some(w) => format!(
                            "{} — {} — PeetConnect",
                            short_title(&w.title, 80),
                            self.session_target.1
                        ),
                        None => format!("{} — PeetConnect", self.session_target.1),
                    },
                };
                if wanted != self.title {
                    window.set_window_title(&wanted);
                    self.title = wanted;
                }
            }

            let root = div()
                .size_full()
                .flex()
                .flex_col()
                .text_color(rgb(0xdddddd))
                .track_focus(&self.focus)
                .on_key_down(cx.listener(Self::on_key))
                .on_mouse_move(cx.listener(Self::track_pointer));

            if self.mode == Mode::Remote && self.screen == Screen::Home {
                return root.child(self.render_home(window, cx));
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

            // In composition modes the video area must stay unpainted so the visual
            // underneath shows through; only the status bar gets a background.
            let root = if self.mode.uses_composition() {
                root
            } else {
                root.bg(rgb(0x101010))
            };
            let session_status = in_session.then(|| self.shared.status());
            let connected = session_status.as_ref().is_some_and(|s| s.connected);
            let fullscreen = window.is_fullscreen();
            // Fullscreen hides the bar once connected; it comes back near the top edge.
            let show_bar =
                !in_session || !fullscreen || !connected || self.pointer_y < REVEAL_BAR_PX;
            let bar = self.session_bar(session_status.as_ref(), fullscreen, cx);
            // While (re)connecting, a card over the (frozen or empty) video says so and offers
            // a way out.
            let overlay = session_status.clone().filter(|s| !s.connected).map(|s| {
                let name = &self.session_target.1;
                let heading = if s.reconnecting {
                    format!("Reconnecting to {name}")
                } else {
                    format!("Connecting to {name}")
                };
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgba(0x0000_0099))
                    .child(Self::overlay_card(heading, s.message).child(Self::chip(
                        ("overlay", 0),
                        "Cancel".into(),
                        false,
                        cx.listener(|this, _, _, _| this.disconnect()),
                    )))
            });
            let window_card = session_status
                .as_ref()
                .filter(|s| s.connected)
                .and_then(|s| Self::window_card(s, cx));
            let picker = session_status
                .as_ref()
                .filter(|s| s.connected && self.picker_open && s.windows_supported)
                .map(|s| Self::window_picker(s, cx));
            // The pointer belongs to the remote desktop unless a card or menu is over the video:
            // then the local arrow stays, for the card's buttons.
            let follow_local =
                connected && overlay.is_none() && window_card.is_none() && picker.is_none();
            let cursor = if in_session {
                self.cursor_element(window, follow_local)
            } else {
                None
            };
            // The remote cursor stands in for the local one over the video, so hide the local
            // one there; two pointers, one a round trip behind the other, look broken.
            let content = if follow_local && cursor.is_some() {
                div()
                    .size_full()
                    .cursor(CursorStyle::None)
                    .child(content)
                    .into_any_element()
            } else {
                content
            };
            let root = if show_bar { root.child(bar) } else { root };
            root.child({
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
                area.child(content)
                    .children(cursor)
                    .children(window_card)
                    .children(overlay)
                    .children(picker)
            })
        }
    }
}
