//! Windows backend: Media Foundation H.264 encoder and D3D11 colour conversion.

mod convert;
mod mf;

pub use convert::BgraToNv12;
pub use mf::{list_encoders, EncoderInfo, MfH264Encoder};

/// Re-export so downstream crates share one `windows` version for texture types.
pub use ::windows;
