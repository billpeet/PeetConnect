//! Attaching a worker thread to the interactive input desktop.
//!
//! When the host runs as a service worker in the console session, its capture and injection
//! threads must sit on whichever desktop currently has user input: `Default` normally, or the
//! `Winlogon` secure desktop during a UAC prompt or at the lock screen. `SetThreadDesktop` only
//! takes effect before a thread touches a device context or injects input, so each worker thread
//! calls [`attach_input_desktop`] once at start and again after a capture error, which is how a
//! desktop switch shows up.

use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS,
    DESKTOP_READOBJECTS, DESKTOP_WRITEOBJECTS, HDESK,
};

/// Moves the calling thread onto the current input desktop. Returns the desktop handle, which
/// the caller keeps alive for as long as the thread should stay attached, or `None` if the
/// desktop could not be opened (for the secure desktop this needs the SYSTEM account).
pub fn attach_input_desktop() -> Option<DesktopGuard> {
    // SAFETY: OpenInputDesktop returns an owned handle or an error we map to None.
    let handle = unsafe {
        OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(DESKTOP_READOBJECTS.0 | DESKTOP_WRITEOBJECTS.0),
        )
    };
    let Ok(handle) = handle else {
        tracing::debug!("OpenInputDesktop failed; staying on the current desktop");
        return None;
    };
    // SAFETY: `handle` is a valid desktop handle for this call.
    if unsafe { SetThreadDesktop(handle) }.is_err() {
        // SAFETY: closing the handle we just opened.
        unsafe {
            let _ = CloseDesktop(handle);
        }
        tracing::debug!("SetThreadDesktop failed");
        return None;
    }
    Some(DesktopGuard(handle))
}

/// Keeps a thread's input desktop handle alive and closes it on drop.
#[derive(Debug)]
pub struct DesktopGuard(HDESK);

impl Drop for DesktopGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenInputDesktop and is closed exactly once.
        unsafe {
            let _ = CloseDesktop(self.0);
        }
    }
}
