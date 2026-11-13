//! Windows backend on a Direct3D 11 device: DXGI Desktop Duplication, Windows.Graphics.Capture
//! (monitors and single windows) and GDI.

mod cursor;
mod device;
mod dxgi;
mod gdi;
mod monitors;
mod source;
mod toplevel;
mod wgc;

pub use cursor::CursorTracker;
pub use device::D3dDevice;
pub use dxgi::DxgiCapturer;
pub use gdi::GdiCapturer;
pub use monitors::enumerate_monitors;
pub use source::{Backend, FrameSource, DXGI_GRACE};
pub use toplevel::{describe_window, enumerate_windows, window_rect_raw};
pub use wgc::{wgc_supported, WgcApplied, WgcCapturer, WgcOptions, WgcTarget};

/// Re-export so downstream crates share one `windows` version for texture types.
pub use ::windows;
