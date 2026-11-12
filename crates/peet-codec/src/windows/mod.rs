//! Windows backend: Media Foundation H.264 encoder/decoder and D3D11 colour conversion.

mod convert;
mod decode;
mod mf;

pub use convert::GpuConverter;
pub use decode::{DecodedFrame, MfH264Decoder};
pub use mf::{list_encoders, EncoderInfo, MfH264Encoder};

/// Re-export so downstream crates share one `windows` version for texture types.
pub use ::windows;
