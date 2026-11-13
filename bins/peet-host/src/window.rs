//! Bringing a host window to the front so injected input reaches it.
//!
//! Input is injected with `SendInput` in desktop coordinates, so it lands on whatever window is
//! on top at the pointer, and keystrokes go to the foreground window. A viewer streaming one
//! window asks for it to be focused before sending input.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindow, SetForegroundWindow,
    ShowWindow, SW_RESTORE,
};

/// Restores `id` if minimised and makes it the foreground window.
pub fn focus(id: u64) -> Result<(), String> {
    let hwnd = HWND(usize::try_from(id).unwrap_or(0) as *mut core::ffi::c_void);
    // SAFETY: plain window-manager calls; a stale handle fails IsWindow first.
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(format!("window {id:#x} not found"));
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else if same_process(GetForegroundWindow(), hwnd) {
            // Already in front, or one of its own popups (a menu, a dialog) is: leave it, so
            // an open menu is not dismissed before the click meant for it arrives.
            return Ok(());
        }
        if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
            return Ok(());
        }
        // Windows only lets the process that received the last input event change the
        // foreground window. Injecting a bare Alt tap makes us that process.
        tap_alt();
        if SetForegroundWindow(hwnd).as_bool() || GetForegroundWindow() == hwnd {
            Ok(())
        } else {
            Err("Windows refused to bring the window to the front".into())
        }
    }
}

fn same_process(a: HWND, b: HWND) -> bool {
    let (mut pa, mut pb) = (0u32, 0u32);
    // SAFETY: plain queries; a null or stale handle leaves the id at zero.
    unsafe {
        GetWindowThreadProcessId(a, Some(&mut pa));
        GetWindowThreadProcessId(b, Some(&mut pb));
    }
    pa != 0 && pa == pb
}

fn tap_alt() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_MENU,
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS::default()), key(KEYEVENTF_KEYUP)];
    // SAFETY: the array outlives the call and the size matches INPUT.
    unsafe {
        SendInput(
            &inputs,
            i32::try_from(std::mem::size_of::<INPUT>()).unwrap_or(0),
        );
    }
}
