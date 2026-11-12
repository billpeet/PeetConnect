//! Still screenshots, independent of the video stream.
//!
//! Uses GDI so it works even when Desktop Duplication is unavailable, and because a still
//! does not need the GPU path. Runs on a blocking thread.

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ImageEncoder, RgbaImage};
use peet_capture::windows::GdiCapturer;
use peet_capture::MonitorDesc;
use peet_proto::{Codec, EncodedImage};

pub fn capture(
    monitor: &MonitorDesc,
    codec: Codec,
    max_width: Option<u32>,
) -> Result<EncodedImage, String> {
    let mut cap = GdiCapturer::new(monitor).map_err(|e| e.to_string())?;
    cap.capture().map_err(|e| e.to_string())?;
    let (width, height) = cap.size();
    let mut rgba = cap.pixels().to_vec();
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
