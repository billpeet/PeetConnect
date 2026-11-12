//! `peet-viewer`: the PeetConnect desktop viewer.
//!
//! `--connect <host>` views a remote PeetConnect host over Tailscale. Without it the viewer
//! runs the milestone 2 loopback rig (local capture, encode, decode and present) which is
//! still the quickest way to measure the pipeline on one machine.

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
    /// Loopback only: how video reaches the screen.
    #[arg(long, value_enum, default_value_t = PresentMode::D3d)]
    present: PresentMode,
    /// Loopback only: capture backend.
    #[arg(long, value_enum, default_value_t = CaptureBackend::Auto)]
    capture: CaptureBackend,
    /// Frame rate to request.
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Bitrate to request, kbit/s.
    #[arg(long, default_value_t = 12000)]
    bitrate_kbps: u32,
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
mod pipeline;
#[cfg(windows)]
mod present;
#[cfg(windows)]
mod remote;

#[cfg(windows)]
mod app {
    use std::sync::mpsc::{channel, sync_channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Context as _, Result};
    use gpui::{
        canvas, div, img, prelude::*, px, rgb, size, App, Application, Bounds, Context,
        FocusHandle, ImageSource, KeyDownEvent, ObjectFit, RenderImage, TitlebarOptions, Window,
        WindowBackgroundAppearance, WindowBounds, WindowOptions,
    };
    use peet_capture::windows::Backend;
    use peet_proto::MonitorId;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

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
        /// Remote session drawing through DirectComposition.
        Remote,
    }

    impl Mode {
        fn uses_composition(self) -> bool {
            self != Mode::LoopbackImage
        }
    }

    pub fn run(cli: &Cli) -> Result<()> {
        let (ui_tx, ui_rx) = sync_channel::<UiFrame>(2);
        let (win_tx, win_rx) = channel::<isize>();
        let placement: SharedPlacement = Arc::new(Mutex::new(Placement::default()));
        let shared = Arc::new(RemoteShared::default());

        let mode = match (&cli.connect, cli.present) {
            (Some(_), _) => Mode::Remote,
            (None, PresentMode::Gpui) => Mode::LoopbackImage,
            (None, PresentMode::D3d) => Mode::LoopbackD3d,
        };

        if let Some(target) = &cli.connect {
            let config = remote::Config {
                session: remote::SessionConfig {
                    target: target.clone(),
                    pin: cli.pin.clone(),
                    monitor: cli.monitor,
                    fps: cli.fps,
                    bitrate_kbps: cli.bitrate_kbps,
                },
                window: win_rx,
                placement: placement.clone(),
                vsync: !cli.no_vsync,
                shared: shared.clone(),
            };
            let shared_for_thread = shared.clone();
            std::thread::Builder::new()
                .name("network".into())
                .spawn(move || {
                    if let Err(err) = remote::run(config) {
                        tracing::error!(%err, "session failed");
                        let mut status = shared_for_thread.status.lock().expect("status poisoned");
                        status.connected = false;
                        status.message = format!("{err:#}");
                    }
                })?;
        } else {
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
                fps: cli.fps,
                bitrate_kbps: cli.bitrate_kbps,
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

        let title = match &cli.connect {
            Some(target) => format!("PeetConnect - {target}"),
            None => "PeetConnect viewer (loopback)".to_string(),
        };
        Application::new().run(move |cx: &mut App| {
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
                |_, cx| cx.new(|cx| Viewer::new(mode, ui_rx, win_tx, placement, shared, cx)),
            )
            .expect("open window");
            cx.activate(true);
        });
        Ok(())
    }

    struct Viewer {
        mode: Mode,
        rx: Receiver<UiFrame>,
        win_tx: Option<Sender<isize>>,
        placement: SharedPlacement,
        shared: Arc<RemoteShared>,
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
    }

    impl Viewer {
        fn new(
            mode: Mode,
            rx: Receiver<UiFrame>,
            win_tx: Sender<isize>,
            placement: SharedPlacement,
            shared: Arc<RemoteShared>,
            cx: &mut Context<Self>,
        ) -> Self {
            Self {
                mode,
                rx,
                win_tx: Some(win_tx),
                placement,
                shared,
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
                            "{}  |  {monitor}  |  {}x{}  |  {:.0} fps (host {})  |  {} kbit/s  |  decode {:.2} ms{rtt}{skipped}  |  keys 1-{} switch monitor",
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

        /// Hands the native window handle to the video thread once, so it can attach its
        /// composition target.
        fn publish_window_handle(&mut self, window: &Window) {
            let Some(tx) = self.win_tx.take() else {
                return;
            };
            let Ok(handle) = HasWindowHandle::window_handle(window) else {
                self.win_tx = Some(tx);
                return;
            };
            if let RawWindowHandle::Win32(win32) = handle.as_raw() {
                let _ = tx.send(win32.hwnd.get());
            }
        }

        fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, _cx: &mut Context<Self>) {
            if self.mode != Mode::Remote {
                return;
            }
            let key = event.keystroke.key.as_str();
            if let Some(index) = key.parse::<usize>().ok().filter(|&n| (1..=9).contains(&n)) {
                let target = self.shared.status().monitors.get(index - 1).map(|m| m.id);
                if let Some(target) = target {
                    self.select_monitor(target);
                }
            }
        }

        fn select_monitor(&mut self, monitor: MonitorId) {
            self.shared.select_monitor(monitor);
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
            if !self.focused_once {
                self.focused_once = true;
                window.focus(&self.focus);
            }
            self.update_status();

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
            let cursor = if self.mode == Mode::Remote {
                self.cursor_element(window)
            } else {
                None
            };

            let root = div()
                .size_full()
                .flex()
                .flex_col()
                .text_color(rgb(0xdddddd))
                .track_focus(&self.focus)
                .on_key_down(cx.listener(Self::on_key));
            // In composition modes the video area must stay unpainted so the visual
            // underneath shows through; only the status bar gets a background.
            let root = if self.mode.uses_composition() {
                root
            } else {
                root.bg(rgb(0x101010))
            };
            root.child(
                div()
                    .h(px(26.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .text_sm()
                    .bg(rgb(0x101010))
                    .child(self.status.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(content)
                    .children(cursor),
            )
        }
    }
}
