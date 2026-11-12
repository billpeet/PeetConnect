//! `peet-viewer`: the PeetConnect desktop viewer.
//!
//! Milestone 2 build: runs a local capture, encode and decode loop and shows the result,
//! through either GPUI's image element or a Direct3D child window, with live timings. The
//! comparison decides how the real viewer presents video.

use clap::{Parser, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "peet-viewer",
    version,
    about = "PeetConnect viewer (loopback test build)"
)]
struct Cli {
    /// Monitor ID to capture (see `peet-bench monitors`); default is the primary.
    #[arg(long)]
    monitor: Option<u32>,
    /// How video reaches the screen.
    #[arg(long, value_enum, default_value_t = PresentMode::Gpui)]
    present: PresentMode,
    #[arg(long, value_enum, default_value_t = CaptureBackend::Auto)]
    capture: CaptureBackend,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    #[arg(long, default_value_t = 12000)]
    bitrate_kbps: u32,
    /// Scale the captured desktop to this size before encoding, e.g. `3840x2160`.
    #[arg(long)]
    upscale: Option<String>,
    /// Present without waiting for vsync (D3D mode only).
    #[arg(long)]
    no_vsync: bool,
    /// Fail instead of falling back to the software encoder.
    #[arg(long)]
    hardware_only: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum PresentMode {
    /// Decode to system memory and draw with GPUI's `img` element.
    Gpui,
    /// Present decoded textures in a D3D11 child window over the GPUI window.
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
mod app {
    use std::sync::mpsc::{channel, sync_channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Context as _, Result};
    use gpui::{
        canvas, div, img, prelude::*, px, rgb, size, App, Application, Bounds, Context,
        ImageSource, ObjectFit, RenderImage, TitlebarOptions, Window, WindowBackgroundAppearance,
        WindowBounds, WindowOptions,
    };
    use peet_capture::windows::Backend;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    use crate::pipeline::{self, Output, UiFrame};
    use crate::present::{Placement, SharedPlacement};
    use crate::{CaptureBackend, Cli, PresentMode};

    pub fn run(cli: &Cli) -> Result<()> {
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
        let (ui_tx, ui_rx) = sync_channel::<UiFrame>(2);
        let (win_tx, win_rx) = channel::<isize>();
        let placement: SharedPlacement = Arc::new(Mutex::new(Placement::default()));
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

        let mode = cli.present;
        Application::new().run(move |cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(1280.0), px(760.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("PeetConnect viewer".into()),
                        ..Default::default()
                    }),
                    // D3D mode draws video underneath GPUI, through a transparent hole.
                    window_background: if mode == PresentMode::D3d {
                        WindowBackgroundAppearance::Transparent
                    } else {
                        WindowBackgroundAppearance::Opaque
                    },
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Viewer::new(mode, ui_rx, win_tx, placement)),
            )
            .expect("open window");
            cx.activate(true);
        });
        Ok(())
    }

    struct Viewer {
        mode: PresentMode,
        rx: Receiver<UiFrame>,
        win_tx: Option<Sender<isize>>,
        placement: SharedPlacement,
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
            mode: PresentMode,
            rx: Receiver<UiFrame>,
            win_tx: Sender<isize>,
            placement: SharedPlacement,
        ) -> Self {
            Self {
                mode,
                rx,
                win_tx: Some(win_tx),
                placement,
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
            let latency_ms = if self.latency_n > 0 {
                self.latency.as_secs_f64() * 1e3 / f64::from(self.latency_n)
            } else {
                0.0
            };
            self.status = match self.mode {
                PresentMode::Gpui => format!(
                    "gpui image  |  {}x{}  |  ui {:.0} fps  |  new frames {:.0} fps  |  capture to render {:.1} ms",
                    self.image_size.0,
                    self.image_size.1,
                    f64::from(self.renders) / secs,
                    f64::from(self.new_frames) / secs,
                    latency_ms
                ),
                PresentMode::D3d => format!(
                    "d3d via DirectComposition  |  ui {:.0} fps  |  video timings on stderr",
                    f64::from(self.renders) / secs
                ),
            };
            self.renders = 0;
            self.new_frames = 0;
            self.latency = Duration::ZERO;
            self.latency_n = 0;
            self.since = Instant::now();
        }

        /// Hands the native window handle to the pipeline once, so it can attach its
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
    }

    impl Render for Viewer {
        fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            window.request_animation_frame();
            self.renders += 1;
            if self.mode == PresentMode::Gpui {
                self.take_latest_frame(window);
            } else {
                self.publish_window_handle(window);
            }
            self.update_status();

            let content = match self.mode {
                PresentMode::Gpui => match &self.image {
                    Some(image) => img(ImageSource::Render(image.clone()))
                        .size_full()
                        .object_fit(ObjectFit::Contain)
                        .into_any_element(),
                    None => div().size_full().into_any_element(),
                },
                PresentMode::D3d => {
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

            let root = div()
                .size_full()
                .flex()
                .flex_col()
                .text_color(rgb(0xdddddd));
            // In D3D mode the video area must stay unpainted so the composition visual
            // underneath shows through; only the status bar gets a background.
            let root = if self.mode == PresentMode::Gpui {
                root.bg(rgb(0x101010))
            } else {
                root
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
            .child(div().flex_1().min_h_0().child(content))
        }
    }
}
