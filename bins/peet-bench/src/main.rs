//! `peet-bench`: exercises capture and encode without any networking.
//!
//! The `capture` command grabs every monitor (or one), encodes what it sees for a few
//! seconds, writes raw Annex B H.264 per monitor, and prints timing statistics. Animated
//! windows are opened on each monitor so there is always something changing.
//!
//! The `window` command captures one window with Windows.Graphics.Capture and runs it through
//! phases (clear, covered, minimised) to show what single-window streaming can and cannot see.

// On other platforms the binary is a stub, so nothing below main is reachable.
#![cfg_attr(not(windows), allow(dead_code))]

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "peet-bench",
    version,
    about = "PeetConnect capture/encode benchmarks"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List monitors the capture backend can see.
    Monitors,
    /// List available H.264 encoders.
    Encoders,
    /// Low-level duplication probe: acquire a few frames per monitor and print raw results.
    Probe,
    /// List top-level windows that single-window capture can target.
    Windows,
    /// Capture one window with Windows.Graphics.Capture, through clear / covered / minimised
    /// phases, writing window-<id>.h264 and per-phase stats.
    Window {
        /// Window to capture: an HWND (decimal or 0x hex) or a case-insensitive title
        /// substring (see `windows`). Default: the bench's own animated window.
        #[arg(long)]
        target: Option<String>,
        /// Total run time, split evenly across the phases that are enabled.
        #[arg(long, default_value_t = 9)]
        seconds: u64,
        /// Cover the target with a grey topmost window for one phase. The cover is owned by a
        /// child process, like another app would be.
        #[arg(long)]
        occlude: bool,
        /// Open the cover from this process instead, to see `IncludeSecondaryWindows` pull
        /// same-app windows into the capture.
        #[arg(long)]
        occlude_same_process: bool,
        /// Minimise the target for one phase, then restore it.
        #[arg(long)]
        minimize: bool,
        /// Keep the yellow capture border.
        #[arg(long)]
        border: bool,
        /// Leave out the window's popups and menus.
        #[arg(long)]
        no_secondary: bool,
        #[arg(long, default_value = "bench-out")]
        out: std::path::PathBuf,
        #[arg(long, value_enum, default_value_t = Encoder::Auto)]
        encoder: Encoder,
        #[arg(long, value_enum, default_value_t = Path::Gpu)]
        path: Path,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        #[arg(long, default_value_t = 8000)]
        bitrate_kbps: u32,
        #[arg(long)]
        no_write: bool,
    },
    /// Internal: open an ordinary animated window titled "PeetConnect test window" on the
    /// primary monitor that moves (2 s), resizes (4 s), minimises (6 s), restores (8 s) and
    /// closes itself (10 s), for testing window streams end to end.
    #[command(hide = true)]
    Testwindow,
    /// Internal: show a grey topmost window over a rectangle until killed (used by `window
    /// --occlude`).
    #[command(hide = true)]
    Occluder { x: i32, y: i32, w: i32, h: i32 },
    /// Capture and encode for a while, writing .h264 files and timing stats.
    Capture {
        /// How long to run.
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Output directory for monitor-<id>.h264 files.
        #[arg(long, default_value = "bench-out")]
        out: std::path::PathBuf,
        /// Capture backend. `auto` starts with DXGI and falls back to GDI if no frames arrive;
        /// `wgc` is Windows.Graphics.Capture.
        #[arg(long, value_enum, default_value_t = CaptureBackend::Auto)]
        capture: CaptureBackend,
        /// Which encoder to use.
        #[arg(long, value_enum, default_value_t = Encoder::Auto)]
        encoder: Encoder,
        /// Feed the encoder from the GPU texture (zero copy) or via system memory.
        #[arg(long, value_enum, default_value_t = Path::Gpu)]
        path: Path,
        /// Only capture the monitor with this ID (see `monitors`).
        #[arg(long)]
        monitor: Option<u32>,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        #[arg(long, default_value_t = 8000)]
        bitrate_kbps: u32,
        /// Do not write output files.
        #[arg(long)]
        no_write: bool,
        /// Do not open the animated test windows that give the capturer something to see.
        #[arg(long)]
        no_stimulate: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum CaptureBackend {
    Auto,
    Dxgi,
    Wgc,
    Gdi,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum Encoder {
    Auto,
    MfHw,
    MfSw,
    Openh264,
    /// Capture only; measures the capture path alone.
    None,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum Path {
    Gpu,
    Cpu,
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
        win::run(cli.command)
    }
    #[cfg(not(windows))]
    {
        let _ = cli;
        anyhow::bail!("peet-bench currently supports Windows only")
    }
}

#[cfg(windows)]
mod stimulate;

#[cfg(windows)]
mod win {
    use std::fs::File;
    use std::io::{BufWriter, Write};
    use std::time::{Duration, Instant};

    use anyhow::Result;
    use peet_capture::windows::{
        describe_window, enumerate_monitors, enumerate_windows, wgc_supported, window_rect_raw,
        Backend, D3dDevice, DxgiCapturer, FrameSource, WgcCapturer, WgcOptions, WgcTarget,
    };
    use peet_capture::{CaptureError, CpuFrame, MonitorDesc, WindowDesc};
    use peet_codec::windows::{list_encoders, MfH264Encoder};
    use peet_codec::{EncoderConfig, EncoderKind, VideoEncoder};
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_MINIMIZE, SW_RESTORE};

    use super::{CaptureBackend, Command, Encoder, Path};

    pub fn run(command: Command) -> Result<()> {
        // SAFETY: process-wide setting; must happen before any window or DXGI call.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        match command {
            Command::Monitors => {
                for m in enumerate_monitors()? {
                    println!(
                        "{:>4}  {:<14} {:>5}x{:<5} at ({},{})  {}%{}{}  [{}]",
                        m.id.0,
                        m.name,
                        m.bounds.width,
                        m.bounds.height,
                        m.bounds.x,
                        m.bounds.y,
                        m.scale_percent,
                        if m.primary { "  primary" } else { "" },
                        if m.rotation == peet_capture::Rotation::Identity {
                            ""
                        } else {
                            "  rotated"
                        },
                        m.adapter_name
                    );
                }
                Ok(())
            }
            Command::Encoders => {
                for e in list_encoders()? {
                    println!("{}  {}", if e.hardware { "hw" } else { "sw" }, e.name);
                }
                println!("sw  openh264 (software)");
                Ok(())
            }
            Command::Probe => {
                let _stim = super::stimulate::start(&enumerate_monitors()?);
                for m in enumerate_monitors()? {
                    let device = D3dDevice::for_adapter(m.adapter_index)?;
                    println!("{} on {} ({})", m.name, m.adapter_name, m.adapter_index);
                    let mut cap = match DxgiCapturer::new(device, &m) {
                        Ok(c) => c,
                        Err(err) => {
                            println!("   duplicate failed: {err}");
                            continue;
                        }
                    };
                    println!("   duplication format {:?}", cap.format());
                    for i in 0..6 {
                        let t = Instant::now();
                        match cap.acquire(Duration::from_millis(500)) {
                            Ok(meta) => println!(
                                "   #{i} ok after {:?}: updated={} dirty={} moved={} cursor={} acc={}",
                                t.elapsed(),
                                meta.desktop_updated,
                                meta.dirty.len(),
                                meta.moved.len(),
                                meta.cursor.is_some(),
                                meta.accumulated_frames
                            ),
                            Err(err) => println!("   #{i} {err} after {:?}", t.elapsed()),
                        }
                    }
                }
                Ok(())
            }
            Command::Windows => {
                println!("WGC supported: {}", wgc_supported());
                for w in enumerate_windows()? {
                    println!(
                        "{:#010x}  {:>5}x{:<5} at ({},{}){}  {:<24} {}",
                        w.id,
                        w.bounds.width,
                        w.bounds.height,
                        w.bounds.x,
                        w.bounds.y,
                        if w.minimized { "  minimised" } else { "" },
                        w.process,
                        w.title
                    );
                }
                Ok(())
            }
            Command::Testwindow => run_test_window(),
            Command::Occluder { x, y, w, h } => {
                let _cover = super::stimulate::occluder(x, y, w, h);
                // The parent kills us when the phase ends.
                loop {
                    std::thread::park();
                }
            }
            Command::Window {
                target,
                seconds,
                occlude,
                occlude_same_process,
                minimize,
                border,
                no_secondary,
                out,
                encoder,
                path,
                fps,
                bitrate_kbps,
                no_write,
            } => {
                if !no_write {
                    std::fs::create_dir_all(&out)?;
                }
                let opts = Opts {
                    duration: Duration::from_secs(seconds),
                    out: (!no_write).then_some(out),
                    capture: CaptureBackend::Wgc,
                    encoder,
                    path,
                    fps,
                    bitrate_kbps,
                };
                let scenario = Scenario {
                    occlude: occlude || occlude_same_process,
                    occlude_same_process,
                    minimize,
                    options: WgcOptions {
                        borderless: !border,
                        include_secondary_windows: !no_secondary,
                        min_interval: Some(Duration::from_secs(1) / fps.max(1)),
                    },
                };
                run_window(target.as_deref(), &opts, &scenario)
            }
            Command::Capture {
                seconds,
                out,
                capture,
                encoder,
                path,
                monitor,
                fps,
                bitrate_kbps,
                no_write,
                no_stimulate,
            } => {
                let monitors: Vec<MonitorDesc> = enumerate_monitors()?
                    .into_iter()
                    .filter(|m| monitor.is_none_or(|id| m.id.0 == id))
                    .collect();
                anyhow::ensure!(!monitors.is_empty(), "no matching monitors");
                if !no_write {
                    std::fs::create_dir_all(&out)?;
                }
                let opts = Opts {
                    duration: Duration::from_secs(seconds),
                    out: (!no_write).then_some(out),
                    capture,
                    encoder,
                    path,
                    fps,
                    bitrate_kbps,
                };
                let _stimulator = (!no_stimulate).then(|| super::stimulate::start(&monitors));
                if !no_stimulate {
                    std::thread::sleep(Duration::from_millis(300));
                }
                let cpu = CpuClock::start();
                let handles: Vec<_> = monitors
                    .into_iter()
                    .map(|m| {
                        let opts = opts.clone();
                        std::thread::spawn(move || run_monitor(&m, &opts))
                    })
                    .collect();
                let mut reports = Vec::new();
                for h in handles {
                    reports.push(h.join().expect("monitor thread panicked")?);
                }
                let cpu_percent = cpu.percent();
                for r in &reports {
                    print!("{r}");
                }
                println!(
                    "process CPU: {cpu_percent:.1}% of one core ({:.1}% of {} cores)",
                    cpu_percent / cpu.cores as f64,
                    cpu.cores
                );
                Ok(())
            }
        }
    }

    #[derive(Clone)]
    struct Opts {
        duration: Duration,
        out: Option<std::path::PathBuf>,
        capture: CaptureBackend,
        encoder: Encoder,
        path: Path,
        fps: u32,
        bitrate_kbps: u32,
    }

    enum Enc {
        None,
        Texture(MfH264Encoder),
        Cpu(Box<dyn VideoEncoder>),
    }

    impl Enc {
        fn name(&self) -> String {
            match self {
                Enc::None => "none".into(),
                Enc::Texture(e) => format!("{} [gpu]", e.name()),
                Enc::Cpu(e) => format!("{} [cpu]", e.name()),
            }
        }
    }

    fn make_encoder(opts: &Opts, device: &D3dDevice, width: u32, height: u32) -> Result<Enc> {
        let config = EncoderConfig {
            width,
            height,
            fps: opts.fps,
            bitrate_kbps: opts.bitrate_kbps,
            keyframe_interval: opts.fps * 2,
        };
        let kind = match opts.encoder {
            Encoder::None => return Ok(Enc::None),
            Encoder::Auto => EncoderKind::Auto,
            Encoder::MfHw => EncoderKind::MediaFoundationHardware,
            Encoder::MfSw => EncoderKind::MediaFoundationSoftware,
            Encoder::Openh264 => EncoderKind::OpenH264,
        };
        if opts.path == Path::Gpu {
            let attempts: &[bool] = match kind {
                EncoderKind::MediaFoundationHardware => &[true],
                EncoderKind::MediaFoundationSoftware => &[false],
                EncoderKind::Auto => &[true, false],
                EncoderKind::OpenH264 => &[],
            };
            for &hw in attempts {
                match MfH264Encoder::new(&config, hw, Some((&device.device, &device.context))) {
                    Ok(enc) if enc.accepts_textures() => return Ok(Enc::Texture(enc)),
                    Ok(enc) => {
                        tracing::warn!("{} cannot take textures; using CPU path", enc.name());
                        return Ok(Enc::Cpu(Box::new(enc)));
                    }
                    Err(err) => tracing::debug!(hw, %err, "GPU encoder unavailable"),
                }
            }
            if kind != EncoderKind::OpenH264 {
                tracing::warn!("no Media Foundation encoder took the GPU path; using CPU path");
            }
        }
        Ok(Enc::Cpu(peet_codec::create_encoder(kind, &config)?))
    }

    fn run_monitor(monitor: &MonitorDesc, opts: &Opts) -> Result<Report> {
        let device = D3dDevice::for_adapter(monitor.adapter_index)?;
        let backend = match opts.capture {
            CaptureBackend::Auto => Backend::Auto,
            CaptureBackend::Dxgi => Backend::Dxgi,
            CaptureBackend::Wgc => Backend::Wgc,
            CaptureBackend::Gdi => Backend::Gdi,
        };
        let mut source = FrameSource::new(device.clone(), monitor, backend, opts.fps)?;
        let (width, height) = source.size();
        let mut encoder = make_encoder(opts, &device, width, height)?;
        let mut writer = match &opts.out {
            Some(dir) => {
                let file = File::create(dir.join(format!("monitor-{}.h264", monitor.id.0)))?;
                Some(BufWriter::new(file))
            }
            None => None,
        };

        let mut cpu_frame = CpuFrame::default();
        let mut stats = Stats::default();
        let start = Instant::now();
        let mut first_frame_at: Option<Instant> = None;
        while start.elapsed() < opts.duration {
            source.wait_for_slot();
            let t0 = Instant::now();
            let meta = match source.acquire(Duration::from_millis(100)) {
                Ok(meta) => meta,
                Err(CaptureError::Timeout) => {
                    stats.timeouts += 1;
                    continue;
                }
                Err(CaptureError::AccessLost) => {
                    stats.access_lost += 1;
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            if !meta.desktop_updated {
                stats.cursor_only += 1;
                continue;
            }
            first_frame_at.get_or_insert(t0);
            stats.frames += 1;
            stats.dirty_rects += meta.dirty.len() as u64;
            if meta.accumulated_frames > 1 {
                stats.dropped += u64::from(meta.accumulated_frames - 1);
            }
            let pts_us = u64::try_from(start.elapsed().as_micros()).unwrap_or(0);

            let t1 = Instant::now();
            let packets = match &mut encoder {
                Enc::None => {
                    if opts.path == Path::Cpu {
                        source.read_cpu(&mut cpu_frame)?;
                    }
                    stats.capture.push(t1 - t0);
                    continue;
                }
                Enc::Texture(enc) => {
                    stats.capture.push(t1 - t0);
                    enc.encode_texture(source.texture(), pts_us)?
                }
                Enc::Cpu(enc) => {
                    source.read_cpu(&mut cpu_frame)?;
                    let t_read = Instant::now();
                    stats.capture.push(t_read - t0);
                    enc.encode_bgra(&cpu_frame.bgra, cpu_frame.stride, pts_us)?
                }
            };
            stats.encode.push(t1.elapsed());
            for p in packets {
                stats.packets += 1;
                stats.bytes += p.data.len() as u64;
                if p.keyframe {
                    stats.keyframes += 1;
                }
                if let Some(w) = &mut writer {
                    w.write_all(&p.data)?;
                }
            }
        }
        if let Some(mut w) = writer {
            w.flush()?;
        }
        let active = first_frame_at.map_or(Duration::ZERO, |t| t.elapsed());
        stats.fell_back = source.fell_back();
        Ok(Report {
            monitor: format!("{} ({}x{})", monitor.name, width, height),
            source: source.name(),
            encoder: encoder.name(),
            active,
            stats,
        })
    }

    fn run_test_window() -> Result<()> {
        use windows::Win32::UI::WindowsAndMessaging::{
            PostMessageW, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER, WM_CLOSE,
        };
        let primary = enumerate_monitors()?
            .into_iter()
            .find(|m| m.primary)
            .ok_or_else(|| anyhow::anyhow!("no primary monitor"))?;
        let (x0, y0) = (primary.bounds.x + 100, primary.bounds.y + 100);
        let (thread, id) = super::stimulate::test_window(x0, y0, 800, 600);
        anyhow::ensure!(id != 0, "could not create the test window");
        let hwnd = HWND(usize::try_from(id).unwrap_or(0) as *mut core::ffi::c_void);
        println!("{id:#x}");
        let step = |at: u64, what: &str| {
            std::thread::sleep(Duration::from_secs(at));
            eprintln!("test window: {what}");
        };
        let place = |x, y, w, h| {
            // SAFETY: plain window placement on our own window.
            let _ = unsafe { SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE) };
        };
        step(2, "move");
        place(x0 + 200, y0 + 50, 800, 600);
        step(2, "resize");
        place(x0 + 200, y0 + 50, 1001, 701);
        step(2, "minimise");
        // SAFETY: as above.
        let _ = unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
        step(2, "restore");
        // SAFETY: as above.
        let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
        step(2, "close");
        // SAFETY: as above.
        let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        let _ = thread.join();
        Ok(())
    }

    struct Scenario {
        occlude: bool,
        occlude_same_process: bool,
        minimize: bool,
        options: WgcOptions,
    }

    /// A grey cover over a rectangle, owned by this process or by a child process.
    enum Cover {
        Local(#[allow(dead_code)] super::stimulate::Occluder),
        Child(std::process::Child),
    }

    impl Drop for Cover {
        fn drop(&mut self) {
            if let Cover::Child(child) = self {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn cover(window: &WindowDesc, same_process: bool) -> Result<Cover> {
        let rect = window.bounds;
        let w = i32::try_from(rect.width).unwrap_or(0);
        let h = i32::try_from(rect.height).unwrap_or(0);
        if same_process {
            return Ok(Cover::Local(super::stimulate::occluder(
                rect.x, rect.y, w, h,
            )));
        }
        let child = std::process::Command::new(std::env::current_exe()?)
            .args(["occluder", "--"])
            .args([rect.x, rect.y, w, h].map(|v| v.to_string()))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(Cover::Child(child))
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Phase {
        Clear,
        Covered,
        Minimised,
    }

    #[derive(Default)]
    struct PhaseStats {
        frames: u64,
        timeouts: u64,
        /// Frames whose centre pixel was the occluder's grey: the cover leaked into capture.
        grey_centre: u64,
        sampled: u64,
        /// The window reported itself minimised during the phase.
        minimized: bool,
        elapsed: Duration,
    }

    fn find_window(target: &str) -> Result<WindowDesc> {
        let parsed = target
            .strip_prefix("0x")
            .map_or_else(|| target.parse::<u64>(), |hex| u64::from_str_radix(hex, 16));
        if let Ok(id) = parsed {
            return describe_window(id).ok_or_else(|| anyhow::anyhow!("no window {target}"));
        }
        let needle = target.to_lowercase();
        let matches: Vec<WindowDesc> = enumerate_windows()?
            .into_iter()
            .filter(|w| w.title.to_lowercase().contains(&needle))
            .collect();
        match matches.as_slice() {
            [one] => Ok(one.clone()),
            [] => anyhow::bail!("no window title contains {target:?} (see `windows`)"),
            many => anyhow::bail!(
                "{} windows match {target:?}; pass an HWND instead:\n{}",
                many.len(),
                many.iter()
                    .map(|w| format!("  {:#010x}  {}", w.id, w.title))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        }
    }

    fn run_window(target: Option<&str>, opts: &Opts, scenario: &Scenario) -> Result<()> {
        let monitors = enumerate_monitors()?;
        let primary = monitors
            .iter()
            .find(|m| m.primary)
            .or(monitors.first())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no monitors"))?;
        // Our own animated window unless the user named one. It repaints every 16 ms, so a
        // missing frame is a capture fact rather than an idle window.
        let stimulator = match target {
            Some(_) => None,
            None => Some(super::stimulate::start(std::slice::from_ref(&primary))),
        };
        let window = match (target, &stimulator) {
            (Some(t), _) => find_window(t)?,
            (None, stim) => {
                std::thread::sleep(Duration::from_millis(300));
                let id = stim
                    .as_ref()
                    .and_then(|s| s.hwnds.first().copied())
                    .unwrap_or(0);
                describe_window(id).ok_or_else(|| anyhow::anyhow!("stimulator window missing"))?
            }
        };
        let own = stimulator.is_some();

        let device = D3dDevice::for_adapter(primary.adapter_index)?;
        let mut cap = WgcCapturer::new(
            device.clone(),
            WgcTarget::Window(window.id),
            scenario.options,
        )?;
        let (width, height) = cap.size();
        let raw = window_rect_raw(window.id);
        println!(
            "== window {:#010x} {:?} ({})",
            window.id, window.title, window.process
        );
        println!(
            "   bounds (DWM) {}x{} at ({},{})   GetWindowRect {}   texture {}x{}   content {:?}",
            window.bounds.width,
            window.bounds.height,
            window.bounds.x,
            window.bounds.y,
            raw.map_or_else(
                || "?".into(),
                |r| format!("{}x{} at ({},{})", r.width, r.height, r.x, r.y)
            ),
            width,
            height,
            cap.content_size()
        );
        println!("   session options applied: {:?}", cap.applied());

        let mut encoder = make_encoder(opts, &device, width, height)?;
        let mut writer = match &opts.out {
            Some(dir) => Some(BufWriter::new(File::create(
                dir.join(format!("window-{:x}.h264", window.id)),
            )?)),
            None => None,
        };

        let mut phases = vec![Phase::Clear];
        if scenario.occlude {
            phases.push(Phase::Covered);
        }
        if scenario.minimize {
            phases.push(Phase::Minimised);
        }
        let slice = opts.duration / u32::try_from(phases.len()).unwrap_or(1);
        let hwnd = HWND(usize::try_from(window.id).unwrap_or(0) as *mut core::ffi::c_void);
        let mut results: Vec<(Phase, PhaseStats)> = Vec::new();
        let mut stats = Stats::default();
        let mut cpu_frame = CpuFrame::default();
        let start = Instant::now();

        'phases: for &phase in &phases {
            let occluder = if phase == Phase::Covered {
                Some(cover(&window, scenario.occlude_same_process)?)
            } else {
                None
            };
            if phase == Phase::Minimised {
                // SAFETY: plain window state change; a stale handle just fails.
                let _ = unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
            }
            // Let DWM settle so a phase change measures the steady state, not the transition.
            // The first phase keeps its queued frame: WGC sends one frame at start and then
            // only on change, so an idle window would otherwise show nothing at all.
            if phase != Phase::Clear {
                std::thread::sleep(Duration::from_millis(250));
                while cap.acquire(Duration::ZERO).is_ok() {}
            }

            let mut ps = PhaseStats::default();
            let phase_start = Instant::now();
            while phase_start.elapsed() < slice {
                ps.minimized |= cap.minimized();
                let t0 = Instant::now();
                match cap.acquire(Duration::from_millis(100)) {
                    Ok(_) => {}
                    Err(CaptureError::Timeout) => {
                        ps.timeouts += 1;
                        stats.timeouts += 1;
                        continue;
                    }
                    Err(CaptureError::Closed) => {
                        println!("   window closed during {phase:?}");
                        ps.elapsed = phase_start.elapsed();
                        results.push((phase, ps));
                        break 'phases;
                    }
                    Err(err) => return Err(err.into()),
                }
                ps.frames += 1;
                stats.frames += 1;
                if own && phase == Phase::Covered {
                    cap.read_cpu(&mut cpu_frame)?;
                    let (cx, cy) = (cpu_frame.width as usize / 2, cpu_frame.height as usize / 2);
                    let i = cy * cpu_frame.stride + cx * 4;
                    ps.sampled += 1;
                    if cpu_frame.bgra[i..i + 3]
                        .iter()
                        .all(|&c| c.abs_diff(0x80) <= 2)
                    {
                        ps.grey_centre += 1;
                    }
                }
                let pts_us = u64::try_from(start.elapsed().as_micros()).unwrap_or(0);
                let t1 = Instant::now();
                stats.capture.push(t1 - t0);
                let packets = match &mut encoder {
                    Enc::None => continue,
                    Enc::Texture(enc) => enc.encode_texture(cap.texture(), pts_us)?,
                    Enc::Cpu(enc) => {
                        cap.read_cpu(&mut cpu_frame)?;
                        enc.encode_bgra(&cpu_frame.bgra, cpu_frame.stride, pts_us)?
                    }
                };
                stats.encode.push(t1.elapsed());
                for p in packets {
                    stats.packets += 1;
                    stats.bytes += p.data.len() as u64;
                    if p.keyframe {
                        stats.keyframes += 1;
                    }
                    if let Some(w) = &mut writer {
                        w.write_all(&p.data)?;
                    }
                }
            }
            ps.elapsed = phase_start.elapsed();
            if phase == Phase::Minimised {
                // SAFETY: as above.
                let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
            }
            drop(occluder);
            results.push((phase, ps));
        }
        if let Some(mut w) = writer {
            w.flush()?;
        }
        drop(stimulator);

        for (phase, ps) in &results {
            let secs = ps.elapsed.as_secs_f64().max(1e-6);
            print!(
                "   {:<10} frames {:>5}  {:>6.1} fps  timeouts {:>3}{}",
                format!("{phase:?}"),
                ps.frames,
                ps.frames as f64 / secs,
                ps.timeouts,
                if ps.minimized { "  (minimised)" } else { "" }
            );
            if ps.sampled > 0 {
                print!(
                    "  occluder visible in {}/{} sampled frames",
                    ps.grey_centre, ps.sampled
                );
            }
            println!();
        }
        print!(
            "{}",
            Report {
                monitor: format!("window ({width}x{height})"),
                source: "wgc",
                encoder: encoder.name(),
                active: start.elapsed(),
                stats,
            }
        );
        Ok(())
    }

    #[derive(Default)]
    struct Stats {
        frames: u64,
        timeouts: u64,
        cursor_only: u64,
        dropped: u64,
        access_lost: u64,
        fell_back: bool,
        dirty_rects: u64,
        packets: u64,
        keyframes: u64,
        bytes: u64,
        capture: Vec<Duration>,
        encode: Vec<Duration>,
    }

    struct Report {
        monitor: String,
        source: &'static str,
        encoder: String,
        active: Duration,
        stats: Stats,
    }

    impl std::fmt::Display for Report {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let s = &self.stats;
            writeln!(
                f,
                "== {}  capture: {}{}  encoder: {}",
                self.monitor,
                self.source,
                if s.fell_back {
                    " (fell back from dxgi)"
                } else {
                    ""
                },
                self.encoder
            )?;
            let secs = self.active.as_secs_f64().max(1e-6);
            writeln!(
                f,
                "   frames {:>6}   {:>6.1} fps   timeouts {}   cursor-only {}   compositor frames missed {}   access lost {}",
                s.frames,
                s.frames as f64 / secs,
                s.timeouts,
                s.cursor_only,
                s.dropped,
                s.access_lost
            )?;
            writeln!(
                f,
                "   dirty rects/frame {:.1}",
                if s.frames > 0 {
                    s.dirty_rects as f64 / s.frames as f64
                } else {
                    0.0
                }
            )?;
            writeln!(f, "   capture  {}", percentiles(&s.capture))?;
            if !s.encode.is_empty() {
                writeln!(f, "   encode   {}", percentiles(&s.encode))?;
                writeln!(
                    f,
                    "   output   {} packets, {} keyframes, {:.0} kbit/s, {:.1} KB/frame",
                    s.packets,
                    s.keyframes,
                    s.bytes as f64 * 8.0 / 1000.0 / secs,
                    if s.packets > 0 {
                        s.bytes as f64 / s.packets as f64 / 1024.0
                    } else {
                        0.0
                    }
                )?;
            }
            Ok(())
        }
    }

    fn percentiles(samples: &[Duration]) -> String {
        if samples.is_empty() {
            return "no samples".into();
        }
        let mut sorted: Vec<Duration> = samples.to_vec();
        sorted.sort();
        let pick = |p: f64| sorted[((sorted.len() - 1) as f64 * p) as usize];
        let mean = sorted.iter().sum::<Duration>() / u32::try_from(sorted.len()).unwrap_or(1);
        format!(
            "mean {:>6.2} ms   p50 {:>6.2}   p95 {:>6.2}   max {:>6.2}",
            mean.as_secs_f64() * 1e3,
            pick(0.5).as_secs_f64() * 1e3,
            pick(0.95).as_secs_f64() * 1e3,
            sorted[sorted.len() - 1].as_secs_f64() * 1e3
        )
    }

    struct CpuClock {
        wall: Instant,
        cpu_100ns: u64,
        cores: usize,
    }

    impl CpuClock {
        fn start() -> Self {
            Self {
                wall: Instant::now(),
                cpu_100ns: process_cpu_100ns(),
                cores: std::thread::available_parallelism().map_or(1, usize::from),
            }
        }

        /// CPU time as a percentage of one core over the elapsed wall time.
        fn percent(&self) -> f64 {
            let cpu = (process_cpu_100ns() - self.cpu_100ns) as f64 * 100e-9;
            cpu / self.wall.elapsed().as_secs_f64().max(1e-6) * 100.0
        }
    }

    fn process_cpu_100ns() -> u64 {
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: the pseudo-handle for the current process is always valid.
        unsafe {
            let _ = GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            );
        }
        let ft = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        ft(kernel) + ft(user)
    }
}
