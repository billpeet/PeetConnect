//! `peet-bench`: exercises capture and encode without any networking.
//!
//! The `capture` command grabs every monitor (or one), encodes what it sees for a few
//! seconds, writes raw Annex B H.264 per monitor, and prints timing statistics. Animated
//! windows are opened on each monitor so there is always something changing.

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
    /// Capture and encode for a while, writing .h264 files and timing stats.
    Capture {
        /// How long to run.
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Output directory for monitor-<id>.h264 files.
        #[arg(long, default_value = "bench-out")]
        out: std::path::PathBuf,
        /// Capture backend. `auto` starts with DXGI and falls back to GDI if no frames arrive.
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

    use anyhow::{Context, Result};
    use peet_capture::windows::{enumerate_monitors, D3dDevice, DxgiCapturer, GdiCapturer};
    use peet_capture::{CaptureError, CpuFrame, FrameMeta, MonitorDesc};
    use peet_codec::windows::{list_encoders, MfH264Encoder};
    use peet_codec::{EncoderConfig, EncoderKind, VideoEncoder};
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
        D3D11_CPU_ACCESS_FLAG, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };

    use super::{CaptureBackend, Command, Encoder, Path};

    /// How long `auto` waits for DXGI to produce a desktop frame before switching to GDI.
    const DXGI_GRACE: Duration = Duration::from_millis(1500);

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

    /// A frame source that always ends with the latest frame in a GPU texture.
    enum Source {
        Dxgi(DxgiCapturer),
        Gdi {
            cap: GdiCapturer,
            device: D3dDevice,
            texture: ID3D11Texture2D,
            interval: Duration,
            next: Instant,
        },
    }

    impl Source {
        fn dxgi(device: &D3dDevice, monitor: &MonitorDesc) -> Result<Self> {
            Ok(Source::Dxgi(
                DxgiCapturer::new(device.clone(), monitor)
                    .with_context(|| format!("duplicating {}", monitor.name))?,
            ))
        }

        fn gdi(device: &D3dDevice, monitor: &MonitorDesc, fps: u32) -> Result<Self> {
            let cap = GdiCapturer::new(monitor)?;
            let (w, h) = cap.size();
            let texture = device.create_texture(
                w,
                h,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_USAGE_DEFAULT,
                D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
                D3D11_CPU_ACCESS_FLAG(0),
            )?;
            Ok(Source::Gdi {
                cap,
                device: device.clone(),
                texture,
                interval: Duration::from_secs(1) / fps.max(1),
                next: Instant::now(),
            })
        }

        fn name(&self) -> &'static str {
            match self {
                Source::Dxgi(_) => "dxgi",
                Source::Gdi { .. } => "gdi",
            }
        }

        fn size(&self) -> (u32, u32) {
            match self {
                Source::Dxgi(c) => c.size(),
                Source::Gdi { cap, .. } => cap.size(),
            }
        }

        /// Blocks until the next frame slot for paced sources. Not counted as capture time.
        fn wait_for_slot(&mut self) {
            if let Source::Gdi { interval, next, .. } = self {
                let now = Instant::now();
                if *next > now {
                    std::thread::sleep(*next - now);
                }
                let floor = Instant::now().checked_sub(*interval).unwrap_or(now);
                *next = (*next + *interval).max(floor);
            }
        }

        fn acquire(&mut self, timeout: Duration) -> Result<FrameMeta, CaptureError> {
            match self {
                Source::Dxgi(c) => c.acquire(timeout),
                Source::Gdi {
                    cap,
                    device,
                    texture,
                    ..
                } => {
                    let meta = cap.capture()?;
                    cap.upload(device, texture);
                    Ok(meta)
                }
            }
        }

        fn texture(&self) -> &ID3D11Texture2D {
            match self {
                Source::Dxgi(c) => c.texture(),
                Source::Gdi { texture, .. } => texture,
            }
        }

        fn read_cpu(&mut self, out: &mut CpuFrame) -> Result<(), CaptureError> {
            match self {
                Source::Dxgi(c) => c.read_cpu(out),
                Source::Gdi { cap, .. } => {
                    cap.read_cpu(out);
                    Ok(())
                }
            }
        }
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
        let mut source = match opts.capture {
            CaptureBackend::Gdi => Source::gdi(&device, monitor, opts.fps)?,
            CaptureBackend::Dxgi | CaptureBackend::Auto => Source::dxgi(&device, monitor)?,
        };
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
            if opts.capture == CaptureBackend::Auto
                && first_frame_at.is_none()
                && matches!(source, Source::Dxgi(_))
                && start.elapsed() > DXGI_GRACE
            {
                tracing::warn!(
                    monitor = %monitor.name,
                    "DXGI delivered no frames in {DXGI_GRACE:?}; switching to GDI capture"
                );
                source = Source::gdi(&device, monitor, opts.fps)?;
                stats.fell_back = true;
            }
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
                    source = Source::dxgi(&device, monitor)?;
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
        Ok(Report {
            monitor: format!("{} ({}x{})", monitor.name, width, height),
            source: source.name(),
            encoder: encoder.name(),
            active,
            stats,
        })
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
