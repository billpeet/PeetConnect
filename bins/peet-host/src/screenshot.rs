//! Still screenshots, independent of the video stream.
//!
//! Monitors use GDI so it works even when Desktop Duplication is unavailable, and because a
//! still does not need the GPU path. Windows use Windows.Graphics.Capture, which sees a window
//! even when others cover it; where WGC is unavailable (the SYSTEM service) the window is cut
//! out of a GDI capture of its monitor. Runs on a blocking thread.

use std::time::Duration;

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ImageEncoder, RgbaImage};
use peet_capture::windows::{GdiCapturer, WgcCapturer, WgcOptions, WgcTarget};
use peet_capture::{CaptureError, CpuFrame, MonitorDesc};
use peet_proto::{Codec, EncodedImage};

pub fn capture(
    monitor: &MonitorDesc,
    codec: Codec,
    max_width: Option<u32>,
) -> Result<EncodedImage, String> {
    let mut cap = GdiCapturer::new(monitor).map_err(|e| e.to_string())?;
    cap.capture().map_err(|e| e.to_string())?;
    let (width, height) = cap.size();
    encode(cap.pixels().to_vec(), width, height, codec, max_width)
}

/// A still of one window. Fails for a minimised window, which DWM does not render.
pub fn capture_window(
    id: u64,
    adapter_index: u32,
    codec: Codec,
    max_width: Option<u32>,
    popups: bool,
    crop: bool,
) -> Result<EncodedImage, String> {
    if crop || !crate::crop::wgc_usable() {
        let (bgra, width, height) = crate::crop::still(id)?;
        return encode(bgra, width, height, codec, max_width);
    }
    let device =
        peet_capture::windows::D3dDevice::for_adapter(adapter_index).map_err(|e| e.to_string())?;
    let options = WgcOptions {
        min_interval: None,
        include_secondary_windows: popups,
        ..WgcOptions::default()
    };
    let mut cap =
        WgcCapturer::new(device, WgcTarget::Window(id), options).map_err(|e| match e {
            CaptureError::Closed => format!("window {id:#x} not found"),
            other => other.to_string(),
        })?;
    // WGC delivers the current image as the first frame straight after starting.
    match cap.acquire(Duration::from_secs(2)) {
        Ok(_) => {}
        Err(CaptureError::Timeout) if cap.minimized() => {
            return Err("the window is minimised; focus it first".into());
        }
        Err(err) => return Err(err.to_string()),
    }
    let mut frame = CpuFrame::default();
    cap.read_cpu(&mut frame).map_err(|e| e.to_string())?;
    // The texture is the content size rounded down to even numbers; crop to whichever is
    // smaller.
    let (cw, ch) = cap.content_size();
    let width = cw.min(frame.width);
    let height = ch.min(frame.height);
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for row in frame.bgra.chunks_exact(frame.stride).take(height as usize) {
        bgra.extend_from_slice(&row[..width as usize * 4]);
    }
    encode(bgra, width, height, codec, max_width)
}

/// Encodes tightly packed BGRA8 rows as the requested still format.
fn encode(
    mut rgba: Vec<u8>,
    width: u32,
    height: u32,
    codec: Codec,
    max_width: Option<u32>,
) -> Result<EncodedImage, String> {
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
        px[3] = 255;
    }
    let mut img = RgbaImage::from_raw(width, height, rgba).ok_or("pixel buffer size mismatch")?;
    if let Some(max_width) = max_width.filter(|&w| w > 0 && w < width) {
        let new_height =
            (u64::from(height) * u64::from(max_width) / u64::from(width)).max(1) as u32;
        img = image::imageops::resize(
            &img,
            max_width,
            new_height,
            image::imageops::FilterType::Triangle,
        );
    }
    let (width, height) = img.dimensions();
    match codec {
        Codec::Png => {
            let mut out = Vec::with_capacity(width as usize * height as usize);
            PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive)
                .write_image(img.as_raw(), width, height, image::ExtendedColorType::Rgba8)
                .map_err(|e| e.to_string())?;
            Ok(EncodedImage {
                codec: Codec::Png,
                width,
                height,
                data: out,
            })
        }
        Codec::RawBgra8 => {
            let mut data = img.into_raw();
            for px in data.as_chunks_mut::<4>().0 {
                px.swap(0, 2);
            }
            Ok(EncodedImage {
                codec: Codec::RawBgra8,
                width,
                height,
                data,
            })
        }
        other => Err(format!("screenshot codec {other:?} not supported")),
    }
}
