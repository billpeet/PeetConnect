//! Synthesises mouse and keyboard input from [`peet_proto::InputEvent`] values.
//!
//! On Windows this drives `SendInput`. The caller must run in an interactive desktop session
//! (the user's `winsta0\default` desktop, or a secure desktop handed over by the service in
//! milestone 4); `SendInput` silently targets the desktop the calling thread is attached to.

pub mod keys;

use peet_proto::InputEvent;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("SendInput delivered {delivered} of {expected} events")]
    Partial { delivered: u32, expected: u32 },
    #[error("input injection is only implemented on Windows")]
    Unsupported,
}

#[cfg(windows)]
mod windows;

/// Injects synthetic input into the current desktop session.
#[derive(Debug, Default)]
pub struct Injector {
    #[cfg(windows)]
    inner: windows::WinInjector,
}

impl Injector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one event. Mouse coordinates are host virtual-desktop pixels.
    pub fn inject(&mut self, event: &InputEvent) -> Result<(), InputError> {
        #[cfg(windows)]
        {
            self.inner.inject(event)
        }
        #[cfg(not(windows))]
        {
            let _ = event;
            Err(InputError::Unsupported)
        }
    }
}
