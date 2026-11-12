//! `SendInput`-backed injection.

use std::mem::size_of;

use peet_proto::{InputEvent, MouseButton};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// X mouse button identifiers (`WinUser.h`); not re-exported by the input module.
const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;

use crate::InputError;

#[derive(Debug, Default)]
pub struct WinInjector;

impl WinInjector {
    // `&mut self` is unused today but kept so a future injector can hold state, such as the set
    // of keys it has pressed, and release them on drop.
    #[allow(clippy::unused_self)]
    pub fn inject(&mut self, event: &InputEvent) -> Result<(), InputError> {
        match event {
            InputEvent::MouseMove { x, y } => send(&[mouse_move(*x, *y)]),
            InputEvent::MouseButton { button, pressed } => send(&[mouse_button(*button, *pressed)]),
            InputEvent::MouseWheel { delta_x, delta_y } => {
                let mut events = Vec::with_capacity(2);
                if *delta_y != 0 {
                    events.push(mouse_event(0, 0, *delta_y as u32, MOUSEEVENTF_WHEEL));
                }
                if *delta_x != 0 {
                    events.push(mouse_event(0, 0, *delta_x as u32, MOUSEEVENTF_HWHEEL));
                }
                if events.is_empty() {
                    return Ok(());
                }
                send(&events)
            }
            InputEvent::Key {
                scancode,
                extended,
                pressed,
            } => send(&[key_scancode(*scancode, *extended, *pressed)]),
            InputEvent::Text { text } => inject_text(text),
        }
    }
}

/// Types a string one UTF-16 unit at a time.
///
/// All Unicode injection rides the single `VK_PACKET` virtual key, so pushing a whole string in
/// one `SendInput` burst makes the target coalesce the rapid same-key messages and repeat the
/// last character. Sending each unit as its own call, with a short gap, keeps every character.
fn inject_text(text: &str) -> Result<(), InputError> {
    for (i, unit) in text.encode_utf16().enumerate() {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        send(&[key_unicode(unit, false)])?;
        std::thread::sleep(std::time::Duration::from_millis(5));
        send(&[key_unicode(unit, true)])?;
    }
    Ok(())
}

/// Virtual-desktop pixel `(x, y)` to the 0..=65535 absolute range `SendInput` expects.
fn mouse_move(x: i32, y: i32) -> INPUT {
    // SAFETY: GetSystemMetrics is a pure query with constant indices.
    let origin_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let origin_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);
    let nx = normalise(x - origin_x, width);
    let ny = normalise(y - origin_y, height);
    mouse_event(
        nx,
        ny,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )
}

fn normalise(pixel: i32, extent: i32) -> i32 {
    if extent <= 1 {
        return 0;
    }
    let clamped = pixel.clamp(0, extent - 1);
    ((i64::from(clamped) * 65535) / i64::from(extent - 1)) as i32
}

fn mouse_button(button: MouseButton, pressed: bool) -> INPUT {
    let (flags, data) = match (button, pressed) {
        (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
        (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
    };
    mouse_event(0, 0, data, flags)
}

fn mouse_event(
    dx: i32,
    dy: i32,
    data: u32,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_scancode(scancode: u16, extended: bool, pressed: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    key_event(0, scancode, flags)
}

fn key_unicode(unit: u16, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    key_event(0, unit, flags)
}

fn key_event(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(events: &[INPUT]) -> Result<(), InputError> {
    // SAFETY: `events` is a valid slice and we pass the matching struct size.
    let delivered = unsafe { SendInput(events, size_of::<INPUT>() as i32) };
    let expected = events.len() as u32;
    if delivered == expected {
        Ok(())
    } else {
        Err(InputError::Partial {
            delivered,
            expected,
        })
    }
}
