//! Windows backend: DXGI Desktop Duplication on a Direct3D 11 device.

mod device;
mod dxgi;
mod gdi;
mod monitors;

pub use device::D3dDevice;
pub use dxgi::DxgiCapturer;
pub use gdi::GdiCapturer;
pub use monitors::enumerate_monitors;

/// Re-export so downstream crates share one `windows` version for texture types.
pub use ::windows;
