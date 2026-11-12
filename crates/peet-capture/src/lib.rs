//! Screen capture.
//!
//! The platform-neutral types live here. The Windows backend ([`windows`]) uses DXGI Desktop
//! Duplication and keeps frames on the GPU so the encoder can consume them without a copy.
//! Linux and Android backends will plug in behind the same types later.

use std::time::Instant;

use peet_proto::{MonitorId, MonitorInfo, Rect};

#[cfg(windows)]
pub mod windows;

/// A monitor as seen by the capture backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorDesc {
    pub id: MonitorId,
    /// Device name, e.g. `\\.\DISPLAY1`.
    pub name: String,
    /// Position and size in virtual desktop coordinates (already rotated).
    pub bounds: Rect,
    /// DPI scale as a percentage (100, 125, 150, ...).
    pub scale_percent: u16,
    pub primary: bool,
    pub rotation: Rotation,
    /// Human-readable adapter name, e.g. `NVIDIA GeForce RTX 4080`.
    pub adapter_name: String,
    /// Backend-specific locator: graphics adapter index.
    pub adapter_index: u32,
    /// Backend-specific locator: output index on that adapter.
    pub output_index: u32,
}

impl MonitorDesc {
    pub fn to_proto(&self) -> MonitorInfo {
        MonitorInfo {
            id: self.id,
            name: self.name.clone(),
            bounds: self.bounds,
            scale_percent: self.scale_percent,
            primary: self.primary,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    Identity,
    Rotate90,
    Rotate180,
    Rotate270,
}

/// A frame copied to system memory as tightly described BGRA8 rows.
#[derive(Debug, Clone, Default)]
pub struct CpuFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row; at least `width * 4`.
    pub stride: usize,
    pub bgra: Vec<u8>,
}

/// Metadata for one acquired frame. The pixels live in the backend (GPU texture) until the
/// caller asks for them.
#[derive(Debug, Clone, Default)]
pub struct FrameMeta {
    /// When the capture backend handed us the frame.
    pub captured_at: Option<Instant>,
    /// True when the desktop image changed; false when only the cursor moved.
    pub desktop_updated: bool,
    /// Frames the compositor produced since our last acquire; more than one means we are
    /// not keeping up.
    pub accumulated_frames: u32,
    /// Changed regions, in monitor-local coordinates. Empty when unknown.
    pub dirty: Vec<Rect>,
    /// Regions that moved without changing content (scrolling, window drags).
    pub moved: Vec<MoveRect>,
    /// Cursor state, when the backend reported a change.
    pub cursor: Option<CursorState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveRect {
    pub source_x: i32,
    pub source_y: i32,
    pub destination: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorState {
    /// Position in monitor-local coordinates.
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    /// Present only when the shape changed.
    pub shape: Option<CursorShapeData>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShapeData {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    pub kind: CursorShapeKind,
    /// Bytes per row of `data`.
    pub pitch: u32,
    pub data: Vec<u8>,
}

impl CursorShapeData {
    /// Converts any shape kind to premultiplied BGRA8 suitable for drawing with alpha
    /// blending, as [`peet_proto::CursorShape`] carries it. XOR regions, which cannot be
    /// drawn without the pixels underneath, become mid-grey.
    pub fn to_proto(&self) -> peet_proto::CursorShape {
        let (width, height, bgra) = match self.kind {
            CursorShapeKind::Color => {
                let mut out = Vec::with_capacity(self.width as usize * self.height as usize * 4);
                for row in self.rows(self.height) {
                    out.extend_from_slice(&row[..self.width as usize * 4]);
                }
                (self.width, self.height, out)
            }
            CursorShapeKind::MaskedColor => {
                let mut out = Vec::with_capacity(self.width as usize * self.height as usize * 4);
                for row in self.rows(self.height) {
                    for px in row[..self.width as usize * 4].as_chunks::<4>().0 {
                        let xor = px[3] == 0xFF;
                        let colour_is_black = px[0] == 0 && px[1] == 0 && px[2] == 0;
                        if xor && colour_is_black {
                            out.extend_from_slice(&[0, 0, 0, 0]);
                        } else if xor {
                            out.extend_from_slice(&[128, 128, 128, 255]);
                        } else {
                            out.extend_from_slice(&[px[0], px[1], px[2], 255]);
                        }
                    }
                }
                (self.width, self.height, out)
            }
            CursorShapeKind::Monochrome => {
                let height = self.height / 2;
                let pitch = self.pitch as usize;
                let mut out = Vec::with_capacity(self.width as usize * height as usize * 4);
                for y in 0..height as usize {
                    let and_row = &self.data[y * pitch..];
                    let xor_row = &self.data[(y + height as usize) * pitch..];
                    for x in 0..self.width as usize {
                        let bit = 0x80 >> (x % 8);
                        let and = and_row[x / 8] & bit != 0;
                        let xor = xor_row[x / 8] & bit != 0;
                        out.extend_from_slice(&match (and, xor) {
                            (false, false) => [0, 0, 0, 255],
                            (false, true) => [255, 255, 255, 255],
                            (true, false) => [0, 0, 0, 0],
                            (true, true) => [128, 128, 128, 255],
                        });
                    }
                }
                (self.width, height, out)
            }
        };
        peet_proto::CursorShape {
            width,
            height,
            hotspot_x: self.hotspot_x,
            hotspot_y: self.hotspot_y,
            bgra,
        }
    }

    fn rows(&self, count: u32) -> impl Iterator<Item = &[u8]> {
        self.data
            .chunks(self.pitch.max(1) as usize)
            .take(count as usize)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShapeKind {
    /// 1 bpp AND mask followed by 1 bpp XOR mask; `height` covers both halves.
    Monochrome,
    /// BGRA8 with alpha.
    Color,
    /// BGRA8 where alpha 0xFF means XOR with the screen.
    MaskedColor,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// No new frame within the requested timeout. Not a failure.
    #[error("no frame within timeout")]
    Timeout,
    /// The duplication session died (mode change, fullscreen switch, desktop switch). The
    /// caller must recreate the capturer.
    #[error("desktop duplication access lost; recreate the capturer")]
    AccessLost,
    /// The desktop cannot be captured from this process (secure desktop, no session).
    #[error("access to the desktop was denied")]
    AccessDenied,
    #[error("capture unsupported: {0}")]
    Unsupported(String),
    #[error("monitor {0:?} not found")]
    MonitorNotFound(MonitorId),
    #[error("os error {code:#010x}: {message}")]
    Os { code: i32, message: String },
}

#[cfg(windows)]
impl From<::windows::core::Error> for CaptureError {
    fn from(err: ::windows::core::Error) -> Self {
        CaptureError::Os {
            code: err.code().0,
            message: err.message(),
        }
    }
}
