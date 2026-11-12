//! PeetConnect wire protocol.
//!
//! Every message that crosses the network is defined here and nowhere else, so host and
//! viewer can be built and shipped independently. Messages are serialised with
//! [`postcard`] and sent as length-prefixed frames (see [`frame`]).
//!
//! Stream layout over QUIC (one connection per session):
//!
//! | Stream        | Direction        | Payload                                  |
//! |---------------|------------------|------------------------------------------|
//! | control       | bidirectional    | [`ClientHello`] / [`ServerHello`], then [`ControlMessage`] |
//! | video[n]      | host to viewer   | [`VideoFrame`] per monitor                |
//! | input         | viewer to host   | [`InputEvent`]                            |
//! | clipboard     | bidirectional    | [`ClipboardMessage`]                      |
//! | file          | bidirectional    | reserved                                  |
//! | audio         | host to viewer   | reserved                                  |
//!
//! This crate has no platform or async dependencies on purpose.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

pub mod frame;

/// Protocol version spoken by this build. Bump on any incompatible change.
pub const PROTOCOL_VERSION: u16 = 1;

/// Default UDP port the host listens on.
pub const DEFAULT_PORT: u16 = 7735;

/// ALPN identifier used during the QUIC/TLS handshake.
pub const ALPN: &[u8] = b"peetconnect/1";

/// Role of a QUIC stream. The opening side writes one tag byte before any frames so the
/// receiver knows how to interpret the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum StreamKind {
    Control = 0,
    Video = 1,
    Input = 2,
    Clipboard = 3,
    File = 4,
    Audio = 5,
}

impl StreamKind {
    pub fn tag(self) -> u8 {
        self as u8
    }

    pub fn from_tag(tag: u8) -> Option<Self> {
        Some(match tag {
            0 => Self::Control,
            1 => Self::Video,
            2 => Self::Input,
            3 => Self::Clipboard,
            4 => Self::File,
            5 => Self::Audio,
            _ => return None,
        })
    }
}

/// Identifies a monitor on the host. Stable for the lifetime of a session unless the host
/// sends [`ControlMessage::MonitorsChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MonitorId(pub u32);

/// Identifies a session on the host; useful for logs and for the local control API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub u64);

/// Video and still-image codecs. Negotiated per session; the host picks the first codec
/// in the viewer's preference list that it can also produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    /// Lossless still image; used for screenshots, never for streaming.
    Png,
    /// Uncompressed BGRA8, top-down rows. Loopback and testing only.
    RawBgra8,
}

/// Optional features either side may support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Feature {
    Clipboard,
    FileTransfer,
    Audio,
    /// Still screenshot requests independent of the video stream.
    Screenshot,
    /// Host accepts more than one simultaneous viewer.
    MultiViewer,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Codecs in order of preference.
    pub codecs: Vec<Codec>,
    pub features: Vec<Feature>,
}

/// Axis-aligned rectangle in pixels, origin top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Geometry of one host monitor in the host's virtual desktop coordinate space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub id: MonitorId,
    pub name: String,
    pub bounds: Rect,
    /// DPI scale as a percentage (100, 125, 150, ...).
    pub scale_percent: u16,
    pub primary: bool,
}

/// First message on the control stream, viewer to host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol_version: u16,
    /// Human-readable name shown in the host's connection list.
    pub client_name: String,
    pub capabilities: Capabilities,
    /// Optional secondary secret on top of Tailscale identity.
    pub pin: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    UnsupportedProtocolVersion,
    NotAuthorised,
    PinRequired,
    PinIncorrect,
    NoCommonCodec,
    Busy,
}

/// Reply to [`ClientHello`], host to viewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerHello {
    Accepted {
        session_id: SessionId,
        host_name: String,
        /// Codec the host will use for video streams.
        codec: Codec,
        capabilities: Capabilities,
        monitors: Vec<MonitorInfo>,
    },
    Rejected {
        reason: RejectReason,
    },
}

/// Messages on the control stream after the handshake, either direction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMessage {
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    /// Viewer asks the host to (re)start a monitor's video stream with the given limits.
    SetStream {
        monitor: MonitorId,
        enabled: bool,
        max_fps: Option<u16>,
        max_bitrate_kbps: Option<u32>,
    },
    /// Viewer asks for an IDR frame, typically after loss.
    RequestKeyframe {
        monitor: MonitorId,
    },
    /// Host reports a new monitor layout (resolution, DPI or topology change).
    MonitorsChanged {
        monitors: Vec<MonitorInfo>,
    },
    Cursor(CursorUpdate),
    Screenshot(ScreenshotRequest),
    ScreenshotResult(ScreenshotResult),
    Disconnect {
        reason: String,
    },
    /// Host-side stream statistics, sent about once a second per active monitor.
    StreamStats {
        monitor: MonitorId,
        fps: u16,
        bitrate_kbps: u32,
        /// Captured frames the host skipped because the network was not keeping up.
        skipped: u32,
    },
}

/// Cursor state is sent out of band so it can be drawn locally with zero latency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorUpdate {
    /// Position in host virtual desktop coordinates.
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    /// Present only when the shape changed since the last update.
    pub shape: Option<CursorShape>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    /// BGRA8 premultiplied, top-down rows.
    pub bgra: Vec<u8>,
}

/// Request a still of one monitor. Serviced independently of the video stream so a
/// headless agent never needs a video decoder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenshotRequest {
    pub request_id: u64,
    pub monitor: MonitorId,
    pub codec: Codec,
    /// Optional downscale; `None` means native resolution.
    pub max_width: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenshotResult {
    pub request_id: u64,
    pub result: Result<EncodedImage, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodedImage {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// One encoded video frame on a monitor's video stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFrame {
    pub monitor: MonitorId,
    /// Monotonic per-monitor sequence number; gaps mean loss.
    pub seq: u64,
    /// Host capture timestamp in microseconds since session start.
    pub pts_us: u64,
    pub keyframe: bool,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// Regions that changed since the previous frame, if the capture layer knows.
    pub dirty: Vec<Rect>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Input from viewer to host. Coordinates are in host virtual desktop space so the viewer is
/// responsible for mapping from its window to the monitor it is showing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseButton {
        button: MouseButton,
        pressed: bool,
    },
    /// Wheel deltas in 1/120 notch units (one notch = 120), matching Windows.
    MouseWheel {
        delta_x: i32,
        delta_y: i32,
    },
    /// Hardware scancode plus extended flag. Preferred over virtual keys because it is
    /// independent of keyboard layout.
    Key {
        scancode: u16,
        extended: bool,
        pressed: bool,
    },
    /// Unicode text, for characters the viewer cannot express as a scancode.
    Text {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardMessage {
    Text { text: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips() {
        let hello = ClientHello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "test".into(),
            capabilities: Capabilities {
                codecs: vec![Codec::H264, Codec::RawBgra8],
                features: vec![Feature::Screenshot],
            },
            pin: None,
        };
        let bytes = postcard::to_stdvec(&hello).unwrap();
        let back: ClientHello = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(hello, back);
    }

    #[test]
    fn video_frame_round_trips() {
        let frame = VideoFrame {
            monitor: MonitorId(1),
            seq: 7,
            pts_us: 16_666,
            keyframe: true,
            codec: Codec::H264,
            width: 1920,
            height: 1080,
            dirty: vec![Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            }],
            data: vec![0, 0, 0, 1, 0x65],
        };
        let bytes = postcard::to_stdvec(&frame).unwrap();
        assert_eq!(frame, postcard::from_bytes::<VideoFrame>(&bytes).unwrap());
    }
}
