//! Top-level window enumeration for single-window capture.
//!
//! Lists the windows a user would recognise as applications: visible, unowned, titled, not tool
//! windows, and not cloaked (cloaked windows live on another virtual desktop or are suspended
//! UWP frames, and DWM does not render them).

use peet_proto::Rect;
use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowLongW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, GWL_EXSTYLE, GW_OWNER,
    WS_EX_TOOLWINDOW,
};

use crate::{CaptureError, WindowDesc};

/// Lists capturable application windows, front to back.
pub fn enumerate_windows() -> Result<Vec<WindowDesc>, CaptureError> {
    let mut hwnds: Vec<HWND> = Vec::new();
    // SAFETY: the callback only runs during this call and `hwnds` outlives it.
    unsafe {
        EnumWindows(
            Some(collect),
            LPARAM(std::ptr::from_mut(&mut hwnds) as isize),
        )?;
    }
    Ok(hwnds
        .into_iter()
        .filter(|&hwnd| is_app_window(hwnd))
        .filter_map(|hwnd| describe_window(hwnd_id(hwnd)))
        .collect())
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the `&mut Vec<HWND>` passed by `enumerate_windows`.
    let hwnds = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    hwnds.push(hwnd);
    BOOL(1)
}

fn is_app_window(hwnd: HWND) -> bool {
    // SAFETY: read-only queries on a window handle; a stale handle just fails them.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        if GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.is_invalid()) {
            return false;
        }
        if GetWindowLongW(hwnd, GWL_EXSTYLE).cast_unsigned() & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        if GetWindowTextLengthW(hwnd) == 0 {
            return false;
        }
    }
    !is_cloaked(hwnd)
}

/// Describes any top-level window by id, including ones [`enumerate_windows`] filters out
/// (tool windows, for example). `None` when the window no longer exists.
pub fn describe_window(id: u64) -> Option<WindowDesc> {
    let hwnd = hwnd_from_id(id);
    // SAFETY: read-only queries; IsWindow guards against a stale id.
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        Some(WindowDesc {
            id,
            title: window_title(hwnd),
            process: process_name(pid),
            pid,
            bounds: window_bounds(hwnd)?,
            minimized: IsIconic(hwnd).as_bool(),
        })
    }
}

/// Visible window bounds in desktop coordinates. Prefers DWM's extended frame bounds, which
/// exclude the invisible resize borders that `GetWindowRect` includes on Windows 10 and 11.
pub(crate) fn window_bounds(hwnd: HWND) -> Option<Rect> {
    let mut rc = RECT::default();
    // SAFETY: the out-pointer is a RECT of the size we pass.
    let dwm = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            std::ptr::from_mut(&mut rc).cast(),
            u32::try_from(std::mem::size_of::<RECT>()).unwrap_or(16),
        )
    };
    // SAFETY: as above.
    if dwm.is_err() && unsafe { GetWindowRect(hwnd, &mut rc) }.is_err() {
        return None;
    }
    Some(Rect {
        x: rc.left,
        y: rc.top,
        width: (rc.right - rc.left).max(0).cast_unsigned(),
        height: (rc.bottom - rc.top).max(0).cast_unsigned(),
    })
}

/// The raw `GetWindowRect`, invisible borders included. Exposed for the bench, which compares
/// it against what capture delivers.
pub fn window_rect_raw(id: u64) -> Option<Rect> {
    let mut rc = RECT::default();
    // SAFETY: plain query with a valid out-pointer.
    unsafe { GetWindowRect(hwnd_from_id(id), &mut rc) }.ok()?;
    Some(Rect {
        x: rc.left,
        y: rc.top,
        width: (rc.right - rc.left).max(0).cast_unsigned(),
        height: (rc.bottom - rc.top).max(0).cast_unsigned(),
    })
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: DWMWA_CLOAKED writes a DWORD.
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            std::ptr::from_mut(&mut cloaked).cast(),
            4,
        )
    }
    .is_ok()
        && cloaked != 0
}

fn window_title(hwnd: HWND) -> String {
    // SAFETY: the buffer is sized from GetWindowTextLengthW plus the terminator.
    unsafe {
        let len = usize::try_from(GetWindowTextLengthW(hwnd)).unwrap_or(0);
        let mut buf = vec![0u16; len + 1];
        let copied = usize::try_from(GetWindowTextW(hwnd, &mut buf)).unwrap_or(0);
        String::from_utf16_lossy(&buf[..copied.min(len)])
    }
}

fn process_name(pid: u32) -> String {
    // SAFETY: the process handle is closed before returning; the buffer length is passed in.
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut len = u32::try_from(buf.len()).unwrap_or(0);
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(process);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().unwrap_or_default().to_owned()
    }
}

pub(crate) fn hwnd_id(hwnd: HWND) -> u64 {
    hwnd.0 as usize as u64
}

pub(crate) fn hwnd_from_id(id: u64) -> HWND {
    HWND(usize::try_from(id).unwrap_or(0) as *mut core::ffi::c_void)
}

pub(crate) fn exists(id: u64) -> bool {
    // SAFETY: read-only query.
    unsafe { IsWindow(Some(hwnd_from_id(id))) }.as_bool()
}

pub(crate) fn is_minimized(id: u64) -> bool {
    // SAFETY: read-only query.
    unsafe { IsIconic(hwnd_from_id(id)) }.as_bool()
}
