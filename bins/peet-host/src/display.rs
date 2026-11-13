//! Keeping the host's displays on while a viewer is connected.
//!
//! When the displays have gone to sleep, DWM composes only a few times a second: WGC then
//! delivers about 4 fps and Desktop Duplication delivers nothing at all, for monitors and
//! windows alike. Measured on the dev box after 13 hours idle: WGC 3.9 fps and DXGI 0 fps,
//! then 43.9 and 39.2 fps once a zero-distance mouse move had woken the displays.
//!
//! A power request only stops displays from going to sleep; it does not wake them. So a session
//! starts with a zero-distance mouse move (input is what wakes a display; the cursor does not
//! move), then holds a display-required and system-required power request until it ends, so a
//! viewer that only watches keeps getting video. The request shows in `powercfg /requests`.

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Power::{
    PowerClearRequest, PowerCreateRequest, PowerRequestDisplayRequired, PowerRequestSystemRequired,
    PowerSetRequest, POWER_REQUEST_TYPE,
};
use windows::Win32::System::Threading::{
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT,
};

/// Wakes sleeping displays with a zero-distance mouse move. `follow_desktop` attaches to the
/// input desktop first (service worker), so it also works on the secure desktop.
pub fn wake(follow_desktop: bool) {
    let _desktop = follow_desktop
        .then(crate::desktop::attach_input_desktop)
        .flatten();
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: MOUSEEVENTF_MOVE,
                ..Default::default()
            },
        },
    };
    // SAFETY: the array outlives the call and the size matches INPUT.
    let sent = unsafe {
        SendInput(
            &[input],
            i32::try_from(std::mem::size_of::<INPUT>()).unwrap_or(0),
        )
    };
    if sent == 0 {
        tracing::debug!(err = %windows::core::Error::from_thread(), "display wake input refused");
    }
}

/// Keeps the displays on and the machine awake until dropped.
pub struct KeepAwake {
    request: HANDLE,
}

// SAFETY: a power request handle is a kernel object handle, usable from any thread.
unsafe impl Send for KeepAwake {}

const KINDS: [POWER_REQUEST_TYPE; 2] = [PowerRequestDisplayRequired, PowerRequestSystemRequired];

impl KeepAwake {
    pub fn new() -> Option<Self> {
        let mut reason: Vec<u16> = "PeetConnect remote session"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let context = REASON_CONTEXT {
            Version: 0, // POWER_REQUEST_CONTEXT_VERSION
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: PWSTR(reason.as_mut_ptr()),
            },
        };
        // SAFETY: `context` and the string it points to live across the call, which copies
        // them; the handle is owned by the returned value and closed on drop.
        unsafe {
            let request = match PowerCreateRequest(&raw const context) {
                Ok(h) => h,
                Err(err) => {
                    tracing::warn!(%err, "could not create a power request; displays may sleep");
                    return None;
                }
            };
            for kind in KINDS {
                if let Err(err) = PowerSetRequest(request, kind) {
                    tracing::warn!(%err, ?kind, "could not set a power request");
                }
            }
            Some(Self { request })
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        // SAFETY: the handle came from PowerCreateRequest and is closed exactly once.
        unsafe {
            for kind in KINDS {
                let _ = PowerClearRequest(self.request, kind);
            }
            let _ = CloseHandle(self.request);
        }
    }
}
