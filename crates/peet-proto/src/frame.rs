//! Length-prefixed framing for protocol messages.
//!
//! A frame is a little-endian `u32` byte length followed by a postcard-encoded message.
//! Frames are capped at [`MAX_FRAME_LEN`] so a hostile peer cannot force a huge allocation.

use serde::{de::DeserializeOwned, Serialize};

/// Upper bound on a single frame. Large enough for a 4K keyframe or a PNG screenshot.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

const HEADER_LEN: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds maximum of {MAX_FRAME_LEN}")]
    TooLarge(u32),
    #[error("message encoding failed: {0}")]
    Encode(#[from] postcard::Error),
    #[error("message decoding failed: {0}")]
    Decode(postcard::Error),
}

/// Encodes `msg` as a complete frame.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let mut out = postcard::to_extend(msg, vec![0u8; HEADER_LEN])?;
    let len = u32::try_from(out.len() - HEADER_LEN).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    out[..HEADER_LEN].copy_from_slice(&len.to_le_bytes());
    Ok(out)
}

/// Outcome of trying to decode one frame from the front of a buffer.
#[derive(Debug)]
pub enum Decoded<T> {
    /// A full frame was present; `consumed` bytes should be dropped from the buffer.
    Message { msg: T, consumed: usize },
    /// Not enough bytes yet; `needed` is the total frame size once the header is known.
    Incomplete { needed: Option<usize> },
}

/// Attempts to decode one frame from the start of `buf` without consuming it.
pub fn decode<T: DeserializeOwned>(buf: &[u8]) -> Result<Decoded<T>, FrameError> {
    if buf.len() < HEADER_LEN {
        return Ok(Decoded::Incomplete { needed: None });
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let total = HEADER_LEN + len as usize;
    if buf.len() < total {
        return Ok(Decoded::Incomplete {
            needed: Some(total),
        });
    }
    let msg = postcard::from_bytes(&buf[HEADER_LEN..total]).map_err(FrameError::Decode)?;
    Ok(Decoded::Message {
        msg,
        consumed: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ControlMessage;

    #[test]
    fn round_trip_and_partial_reads() {
        let msg = ControlMessage::Ping { nonce: 42 };
        let bytes = encode(&msg).unwrap();

        match decode::<ControlMessage>(&bytes[..2]).unwrap() {
            Decoded::Incomplete { needed: None } => {}
            other => panic!("unexpected {other:?}"),
        }
        match decode::<ControlMessage>(&bytes[..bytes.len() - 1]).unwrap() {
            Decoded::Incomplete { needed: Some(n) } => assert_eq!(n, bytes.len()),
            other => panic!("unexpected {other:?}"),
        }
        let mut with_trailing = bytes.clone();
        with_trailing.extend_from_slice(&[9, 9, 9]);
        match decode::<ControlMessage>(&with_trailing).unwrap() {
            Decoded::Message {
                msg: back,
                consumed,
            } => {
                assert_eq!(back, msg);
                assert_eq!(consumed, bytes.len());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_oversized_header() {
        let bytes = (MAX_FRAME_LEN + 1).to_le_bytes();
        assert!(matches!(
            decode::<ControlMessage>(&bytes),
            Err(FrameError::TooLarge(_))
        ));
    }
}
