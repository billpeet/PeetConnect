//! Windows backend: Media Foundation H.264 encoder/decoder and D3D11 colour conversion.

mod convert;
mod decode;
mod mf;

pub use convert::GpuConverter;
pub use decode::{DecodedFrame, MfH264Decoder};
pub use mf::{list_encoders, EncoderInfo, MfH264Encoder};

/// Re-export so downstream crates share one `windows` version for texture types.
pub use ::windows;

use crate::CodecError;

/// Names the call an OS error came from, so a failure says which step broke rather than
/// only the HRESULT (`0x8000ffff` alone could be any of a dozen calls in one encode).
pub(crate) trait Step<T> {
    fn step(self, what: &str) -> Result<T, CodecError>;
}

impl<T> Step<T> for ::windows::core::Result<T> {
    fn step(self, what: &str) -> Result<T, CodecError> {
        self.map_err(|err| CodecError::Os {
            code: err.code().0,
            message: format!("{what}: {}", err.message()),
        })
    }
}
