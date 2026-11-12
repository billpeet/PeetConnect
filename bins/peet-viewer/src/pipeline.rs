//! Loopback pipeline: capture, encode, decode, and hand frames to a presenter.
//!
//! This is the milestone 2 test rig. Everything that will later sit on the host side
//! (capture and encode) and on the viewer side (decode and present) runs in one thread here,
//! with no network in between, so the cost of each stage can be measured in isolation.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gpui::RenderImage;
use image::{Frame, ImageBuffer};
use peet_capture::windows::{enumerate_monitors, Backend, D3dDevice, FrameSource};
use peet_capture::CaptureError;
use peet_codec::windows::{GpuConverter, MfH264Decoder, MfH264Encoder};
use peet_codec::{EncoderConfig, VideoEncoder};
use smallvec::SmallVec;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_FLAG, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

use crate::present::{Presenter, SharedPlacement};
use windows::Win32::Foundation::HWND;

/// A frame ready for the GPUI image path.
pub struct UiFrame {
    pub image: Arc<RenderImage>,
    pub captured_at: Instant,
    pub width: u32,
    pub height: u32,
}

pub enum Output {
    /// Read decoded frames back to system memory and send them to the UI as images.
    Gpui(SyncSender<UiFrame>),
    /// Present through DirectComposition once the UI tells us its window handle.
    D3d {
        window: Receiver<isize>,
        placement: SharedPlacement,
        vsync: bool,
    },
}

pub struct Config {
    pub monitor: Option<u32>,
    pub backend: Backend,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Scale captured frames to this size before encoding, to simulate larger desktops.
    pub upscale: Option<(u32, u32)>,
    pub hardware_only: bool,
    pub output: Output,
}

/// Runs until the process exits. Prints stage timings every two seconds.
pub fn run(config: Config) -> Result<()> {
    let monitor = enumerate_monitors()?
        .into_iter()
        .find(|m| {
            config.monitor.is_none_or(|id| m.id.0 == id) && (config.monitor.is_some() || m.primary)
        })
        .or_else(|| enumerate_monitors().ok()?.into_iter().next())
        .context("no monitor to capture")?;
    let device = D3dDevice::for_adapter(monitor.adapter_index)?;
    let mut source = FrameSource::new(device.clone(), &monitor, config.backend, config.fps)?;
    let captured = source.size();
    let encoded = config.upscale.unwrap_or(captured);

    let mut scaler = (encoded != captured)
        .then(|| GpuConverter::bgra_scaler(&device.device, &device.context, captured, encoded))
        .transpose()?;

    let enc_config = EncoderConfig {
        width: encoded.0,
        height: encoded.1,
        fps: config.fps,
        bitrate_kbps: config.bitrate_kbps,
        keyframe_interval: config.fps * 2,
    };
    let mut enc =
        match MfH264Encoder::new(&enc_config, true, Some((&device.device, &device.context))) {
            Ok(enc) if enc.accepts_textures() => enc,
            Ok(_) | Err(_) if config.hardware_only => {
                anyhow::bail!("no hardware encoder with D3D11 input")
            }
            Ok(_) => anyhow::bail!("hardware encoder refused textures"),
            Err(err) => {
                tracing::warn!(%err, "no hardware encoder; using software MFT");
                MfH264Encoder::new(&enc_config, false, Some((&device.device, &device.context)))?
            }
        };
    anyhow::ensure!(enc.accepts_textures(), "encoder cannot take textures");
    let mut decoder = MfH264Decoder::new(encoded.0, encoded.1, &device.device)?;

    let mut sink = match config.output {
        Output::Gpui(tx) => Sink::Gpui(Readback::new(device.clone(), tx)),
        Output::D3d {
            window,
            placement,
            vsync,
        } => {
            let hwnd = window
                .recv()
                .context("UI never provided its window handle")?;
            let hwnd = HWND(hwnd as *mut core::ffi::c_void);
            Sink::D3d(Presenter::new(device.clone(), hwnd, placement, vsync)?)
        }
    };
    tracing::info!(
        monitor = %monitor.name, capture = source.name(), encoder = enc.name(),
        ?captured, ?encoded, sink = sink.name(), "pipeline running"
    );

    let start = Instant::now();
    let mut stats = Stats::default();
    let mut last_report = Instant::now();
    loop {
        source.wait_for_slot();
        let t0 = Instant::now();
        let meta = match source.acquire(Duration::from_millis(100)) {
            Ok(meta) => meta,
            Err(CaptureError::Timeout | CaptureError::AccessLost) => continue,
            Err(err) => return Err(err.into()),
        };
        if !meta.desktop_updated {
            continue;
        }
        let t_cap = Instant::now();
        let pts_us = u64::try_from(start.elapsed().as_micros()).unwrap_or(0);

        let texture: &ID3D11Texture2D = match scaler.as_mut() {
            Some(scaler) => scaler.convert(source.texture(), 0)?,
            None => source.texture(),
        };
        let packets = enc.encode_texture(texture, pts_us)?;
        let t_enc = Instant::now();

        let mut frames = Vec::new();
        for packet in &packets {
            stats.bytes += packet.data.len() as u64;
            frames.extend(decoder.decode(&packet.data, packet.pts_us)?);
        }
        let t_dec = Instant::now();

        for frame in &frames {
            sink.deliver(frame, t0)?;
        }
        let t_out = Instant::now();

        stats.frames += 1;
        stats.capture += t_cap - t0;
        stats.encode += t_enc - t_cap;
        stats.decode += t_dec - t_enc;
        stats.output += t_out - t_dec;
        if !frames.is_empty() {
            stats.delivered += frames.len() as u64;
            stats.end_to_end += t_out - t0;
        }
        if last_report.elapsed() >= Duration::from_secs(2) {
            stats.report(last_report.elapsed(), sink.name());
            stats = Stats::default();
            last_report = Instant::now();
        }
    }
}

enum Sink {
    Gpui(Readback),
    D3d(Presenter),
}

impl Sink {
    fn name(&self) -> &'static str {
        match self {
            Sink::Gpui(_) => "gpui-image",
            Sink::D3d(_) => "d3d-dcomp",
        }
    }

    fn deliver(
        &mut self,
        frame: &peet_codec::windows::DecodedFrame,
        captured_at: Instant,
    ) -> Result<()> {
        match self {
            Sink::Gpui(readback) => readback.deliver(frame, captured_at),
            Sink::D3d(presenter) => presenter.present(frame),
        }
    }
}

/// NV12 texture to BGRA system memory to a GPUI `RenderImage`.
struct Readback {
    device: D3dDevice,
    tx: SyncSender<UiFrame>,
    converter: Option<GpuConverter>,
    staging: Option<ID3D11Texture2D>,
    dropped: u64,
}

impl Readback {
    fn new(device: D3dDevice, tx: SyncSender<UiFrame>) -> Self {
        Self {
            device,
            tx,
            converter: None,
            staging: None,
            dropped: 0,
        }
    }

    fn deliver(
        &mut self,
        frame: &peet_codec::windows::DecodedFrame,
        captured_at: Instant,
    ) -> Result<()> {
        let size = (frame.width, frame.height);
        if self.converter.as_ref().is_none_or(|c| c.in_size() != size) {
            self.converter = Some(GpuConverter::nv12_to_bgra(
                &self.device.device,
                &self.device.context,
                size,
                size,
            )?);
            self.staging = Some(self.device.create_texture(
                size.0,
                size.1,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_USAGE_STAGING,
                D3D11_BIND_FLAG(0),
                D3D11_CPU_ACCESS_READ,
            )?);
        }
        let converter = self.converter.as_mut().expect("converter ensured");
        let staging = self.staging.as_ref().expect("staging ensured");
        let bgra = converter.convert(&frame.texture, frame.subresource)?;

        let row_bytes = size.0 as usize * 4;
        let mut pixels = vec![0u8; row_bytes * size.1 as usize];
        let ctx = &self.device.context;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging matches the converter output; Map/Unmap are paired.
        unsafe {
            ctx.CopyResource(staging, bgra);
            ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let src =
                std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * size.1 as usize);
            for (y, row) in pixels.chunks_exact_mut(row_bytes).enumerate() {
                row.copy_from_slice(&src[y * pitch..y * pitch + row_bytes]);
            }
            ctx.Unmap(staging, 0);
        }

        // GPUI's RenderImage wants BGRA bytes in an RGBA-typed buffer.
        let buffer =
            ImageBuffer::from_raw(size.0, size.1, pixels).context("image buffer size mismatch")?;
        let image = Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(buffer), 1)));
        match self.tx.try_send(UiFrame {
            image,
            captured_at,
            width: size.0,
            height: size.1,
        }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.dropped += 1;
                if self.dropped.is_multiple_of(60) {
                    tracing::warn!(
                        dropped = self.dropped,
                        "UI is not keeping up; dropping frames"
                    );
                }
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => anyhow::bail!("UI closed"),
        }
    }
}

#[derive(Default)]
struct Stats {
    frames: u64,
    delivered: u64,
    bytes: u64,
    capture: Duration,
    encode: Duration,
    decode: Duration,
    output: Duration,
    end_to_end: Duration,
}

impl Stats {
    fn report(&self, elapsed: Duration, sink: &str) {
        if self.frames == 0 {
            tracing::info!("no frames in the last {elapsed:?}");
            return;
        }
        let per = |d: Duration| d.as_secs_f64() * 1e3 / self.frames as f64;
        let e2e = if self.delivered > 0 {
            self.end_to_end.as_secs_f64() * 1e3 / self.delivered as f64
        } else {
            0.0
        };
        tracing::info!(
            "{:.1} fps | capture {:.2} ms | encode {:.2} | decode {:.2} | {} {:.2} | capture-to-{} {:.2} ms | {:.0} kbit/s",
            self.frames as f64 / elapsed.as_secs_f64(),
            per(self.capture),
            per(self.encode),
            per(self.decode),
            sink,
            per(self.output),
            if sink == "d3d-dcomp" { "present" } else { "image" },
            e2e,
            self.bytes as f64 * 8.0 / 1000.0 / elapsed.as_secs_f64(),
        );
    }
}
