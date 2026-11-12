//! Video encoding.
//!
//! Two families of encoder:
//!
//! * [`windows::MfH264Encoder`]: Media Foundation H.264, hardware (NVENC, AMF, Quick Sync via
//!   their MFTs) or Microsoft's software MFT. Accepts GPU textures for a zero-copy path.
//! * [`OpenH264Encoder`]: Cisco's openh264, pure CPU, works everywhere. The fallback.
//!
//! Output is Annex B H.264 (start-code delimited NAL units), one access unit per packet.

pub mod nv12;
mod openh264_enc;
#[cfg(windows)]
pub mod windows;

pub use openh264_enc::OpenH264Encoder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Maximum distance between keyframes, in frames. `0` means encoder default.
    pub keyframe_interval: u32,
}

impl EncoderConfig {
    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_kbps.saturating_mul(1000)
    }
}

/// One encoded access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_us: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("no suitable encoder: {0}")]
    Unavailable(String),
    #[error("encoder error: {0}")]
    Encoder(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("os error {code:#010x}: {message}")]
    Os { code: i32, message: String },
}

#[cfg(windows)]
impl From<::windows::core::Error> for CodecError {
    fn from(err: ::windows::core::Error) -> Self {
        CodecError::Os {
            code: err.code().0,
            message: err.message(),
        }
    }
}

impl From<openh264::Error> for CodecError {
    fn from(err: openh264::Error) -> Self {
        CodecError::Encoder(err.to_string())
    }
}

/// An encoder that accepts BGRA8 frames from system memory.
pub trait VideoEncoder: Send {
    /// Human-readable encoder name, e.g. `NVIDIA H264 Encoder MFT`.
    fn name(&self) -> &str;
    fn is_hardware(&self) -> bool;
    /// Encodes one frame. `stride` is bytes per row. May return zero packets (encoder is
    /// pipelining or skipped the frame) or more than one (catching up).
    fn encode_bgra(
        &mut self,
        bgra: &[u8],
        stride: usize,
        pts_us: u64,
    ) -> Result<Vec<EncodedPacket>, CodecError>;
    /// The next encoded frame will be an IDR frame.
    fn request_keyframe(&mut self);
}

/// Which encoder to construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderKind {
    /// Hardware Media Foundation, then software Media Foundation, then openh264.
    #[default]
    Auto,
    MediaFoundationHardware,
    MediaFoundationSoftware,
    OpenH264,
}

/// Builds a system-memory encoder of the requested kind.
pub fn create_encoder(
    kind: EncoderKind,
    config: &EncoderConfig,
) -> Result<Box<dyn VideoEncoder>, CodecError> {
    match kind {
        EncoderKind::OpenH264 => Ok(Box::new(OpenH264Encoder::new(config)?)),
        #[cfg(windows)]
        EncoderKind::MediaFoundationHardware => {
            Ok(Box::new(windows::MfH264Encoder::new(config, true, None)?))
        }
        #[cfg(windows)]
        EncoderKind::MediaFoundationSoftware => {
            Ok(Box::new(windows::MfH264Encoder::new(config, false, None)?))
        }
        #[cfg(windows)]
        EncoderKind::Auto => {
            for hardware in [true, false] {
                match windows::MfH264Encoder::new(config, hardware, None) {
                    Ok(enc) => return Ok(Box::new(enc)),
                    Err(err) => {
                        tracing::debug!(hardware, %err, "Media Foundation encoder unavailable");
                    }
                }
            }
            Ok(Box::new(OpenH264Encoder::new(config)?))
        }
        #[cfg(not(windows))]
        EncoderKind::Auto => Ok(Box::new(OpenH264Encoder::new(config)?)),
        #[cfg(not(windows))]
        EncoderKind::MediaFoundationHardware | EncoderKind::MediaFoundationSoftware => Err(
            CodecError::Unavailable("Media Foundation is only available on Windows".into()),
        ),
    }
}

/// True if the Annex B stream contains an IDR slice (NAL type 5).
pub fn contains_idr(annex_b: &[u8]) -> bool {
    nal_types(annex_b).any(|t| t == 5)
}

/// Iterates NAL unit types in an Annex B stream.
fn nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut i = 0usize;
    std::iter::from_fn(move || {
        while i + 3 <= data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                let header = *data.get(i + 3)?;
                i += 3;
                return Some(header & 0x1f);
            }
            i += 1;
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_idr_nal() {
        let sps_pps_idr = [
            0, 0, 0, 1, 0x67, 0xaa, 0, 0, 0, 1, 0x68, 0xbb, 0, 0, 1, 0x65, 0xcc,
        ];
        assert!(contains_idr(&sps_pps_idr));
        let p_slice = [0, 0, 0, 1, 0x41, 0xaa];
        assert!(!contains_idr(&p_slice));
        assert_eq!(nal_types(&sps_pps_idr).collect::<Vec<_>>(), vec![7, 8, 5]);
    }
}
