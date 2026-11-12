//! Framed message streams over quinn.

use bytes::{Buf, BytesMut};
use peet_proto::frame::{self, Decoded};
use serde::{de::DeserializeOwned, Serialize};

use crate::{Result, TransportError};

/// Writes length-prefixed messages to a QUIC send stream.
#[derive(Debug)]
pub struct FramedSend {
    inner: quinn::SendStream,
}

impl FramedSend {
    pub(crate) fn new(inner: quinn::SendStream) -> Self {
        Self { inner }
    }

    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        let bytes = frame::encode(msg)?;
        self.inner.write_all(&bytes).await?;
        Ok(())
    }

    /// Sends a frame that has already been encoded with [`peet_proto::frame::encode`].
    pub async fn send_encoded(&mut self, bytes: &[u8]) -> Result<()> {
        self.inner.write_all(bytes).await?;
        Ok(())
    }

    /// Signals the end of the stream. Buffered data is still delivered.
    pub fn finish(&mut self) -> Result<()> {
        self.inner
            .finish()
            .map_err(|_| TransportError::Write(quinn::WriteError::ClosedStream))
    }

    /// Abandons the stream; unsent data is dropped.
    pub fn reset(&mut self, code: u32) {
        let _ = self.inner.reset(quinn::VarInt::from_u32(code));
    }

    pub fn id(&self) -> quinn::StreamId {
        self.inner.id()
    }
}

/// Reads length-prefixed messages from a QUIC receive stream.
#[derive(Debug)]
pub struct FramedRecv {
    inner: quinn::RecvStream,
    buf: BytesMut,
}

impl FramedRecv {
    pub(crate) fn new(inner: quinn::RecvStream) -> Self {
        Self {
            inner,
            buf: BytesMut::with_capacity(64 * 1024),
        }
    }

    /// Reads the next message. Returns `Ok(None)` when the peer finished the stream cleanly
    /// between frames.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        loop {
            match frame::decode::<T>(&self.buf)? {
                Decoded::Message { msg, consumed } => {
                    self.buf.advance(consumed);
                    return Ok(Some(msg));
                }
                Decoded::Incomplete { needed } => {
                    let want = needed.map_or(64 * 1024, |n| n.saturating_sub(self.buf.len()));
                    let Some(chunk) = self.inner.read_chunk(want.max(4096), true).await? else {
                        if self.buf.is_empty() {
                            return Ok(None);
                        }
                        return Err(TransportError::TruncatedFrame);
                    };
                    self.buf.extend_from_slice(&chunk.bytes);
                }
            }
        }
    }

    pub fn id(&self) -> quinn::StreamId {
        self.inner.id()
    }
}
