//! Animated test windows so Desktop Duplication has something to deliver.
//!
//! One borderless, topmost window per monitor repaints a bouncing block over a slowly
//! cycling background at roughly 60 Hz. The windows close when the [`Stimulator`] drops.
//! [`occluder`] opens a plain grey window on top, to test capture of covered windows, and
//! [`test_window`] an ordinary titled, resizable animated window for single-window streams.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, OnceLock};
use std::thread::JoinHandle;

use peet_capture::MonitorDesc;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetCursorPos,
    GetMessageW, GetWindowLongPtrW, PostMessageW, PostQuitMessage, RegisterClassW, SetCursorPos,
    SetTimer, SetWindowLongPtrW, TranslateMessage, CS_HREDRAW, GWLP_USERDATA, MSG, WINDOW_EX_STYLE,
    WM_CLOSE, WM_DESTROY, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Borderless topmost tool window with the bouncing block.
    Animated,
    /// Borderless topmost plain grey window.
    Solid,
    /// Ordinary titled, resizable window with the bouncing block.
    Normal,
}

static STOP: AtomicBool = AtomicBool::new(false);
static TICK: AtomicU32 = AtomicU32::new(0);
static CLASS: OnceLock<u16> = OnceLock::new();

pub struct Stimulator {
    threads: Vec<JoinHandle<()>>,
    /// The animated windows, one per monitor in the order given to [`start`], as raw `HWND`
    /// values. Zero where a window could not be created.
    pub hwnds: Vec<u64>,
}

/// A solid window on top of everything else; closes on drop.
pub struct Occluder {
    thread: Option<JoinHandle<()>>,
    hwnd: u64,
}

impl Drop for Occluder {
    fn drop(&mut self) {
        if self.hwnd != 0 {
            // SAFETY: posting to a window owned by our thread; a stale handle just fails.
            unsafe {
                let _ = PostMessageW(Some(hwnd(self.hwnd)), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Opens a plain grey topmost window over the given desktop rectangle.
pub fn occluder(x: i32, y: i32, w: i32, h: i32) -> Occluder {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || window_thread(x, y, w, h, Kind::Solid, &tx));
    Occluder {
        thread: Some(thread),
        hwnd: rx.recv().unwrap_or(0),
    }
}

/// Opens an ordinary titled, resizable animated window, "PeetConnect test window". Returns
/// its message-loop thread, which ends when the window is closed, and its `HWND`.
pub fn test_window(x: i32, y: i32, w: i32, h: i32) -> (JoinHandle<()>, u64) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || window_thread(x, y, w, h, Kind::Normal, &tx));
    (thread, rx.recv().unwrap_or(0))
}

fn hwnd(id: u64) -> HWND {
    HWND(usize::try_from(id).unwrap_or(0) as *mut core::ffi::c_void)
}

/// Opens one animated window covering the middle 60% of each monitor, and nudges the mouse
/// cursor every 50 ms so cursor-only updates are exercised too.
pub fn start(monitors: &[MonitorDesc]) -> Stimulator {
    STOP.store(false, Ordering::SeqCst);
    let mut threads: Vec<JoinHandle<()>> = vec![std::thread::spawn(cursor_thread)];
    let mut hwnds = Vec::new();
    for m in monitors {
        let b = m.bounds;
        let w = i32::try_from(b.width).unwrap_or(0) * 6 / 10;
        let h = i32::try_from(b.height).unwrap_or(0) * 6 / 10;
        let x = b.x + (i32::try_from(b.width).unwrap_or(0) - w) / 2;
        let y = b.y + (i32::try_from(b.height).unwrap_or(0) - h) / 2;
        let (tx, rx) = mpsc::channel();
        threads.push(std::thread::spawn(move || {
            window_thread(x, y, w, h, Kind::Animated, &tx);
        }));
        hwnds.push(rx.recv().unwrap_or(0));
    }
    Stimulator { threads, hwnds }
}

fn cursor_thread() {
    let mut step = 0i32;
    while !STOP.load(Ordering::SeqCst) {
        let mut pos = windows::Win32::Foundation::POINT::default();
        // SAFETY: plain cursor queries/moves on the interactive desktop.
        unsafe {
            if GetCursorPos(&mut pos).is_ok() {
                let dx = if step % 2 == 0 { 2 } else { -2 };
                let _ = SetCursorPos(pos.x + dx, pos.y);
            }
        }
        step += 1;
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

impl Drop for Stimulator {
    fn drop(&mut self) {
        STOP.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn register_class() -> u16 {
    *CLASS.get_or_init(|| {
        // SAFETY: the class struct is fully initialised and the name is a static literal.
        unsafe {
            let instance = GetModuleHandleW(None).unwrap_or_default();
            let class = WNDCLASSW {
                style: CS_HREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: w!("PeetBenchStimulator"),
                ..Default::default()
            };
            RegisterClassW(&class)
        }
    })
}

/// Runs one window's message loop. Sends the window handle (0 on failure) once created.
fn window_thread(x: i32, y: i32, w: i32, h: i32, kind: Kind, created: &mpsc::Sender<u64>) {
    if register_class() == 0 {
        tracing::warn!("could not register stimulator window class");
        let _ = created.send(0);
        return;
    }
    // SAFETY: standard Win32 window lifecycle on this thread; the message loop ends when the
    // window is destroyed from WM_TIMER.
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let (ex_style, style, title) = match kind {
            Kind::Normal => (
                WINDOW_EX_STYLE(0),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                w!("PeetConnect test window"),
            ),
            Kind::Animated | Kind::Solid => (
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                WS_POPUP | WS_VISIBLE,
                w!("peet-bench"),
            ),
        };
        let hwnd = match CreateWindowExW(
            ex_style,
            w!("PeetBenchStimulator"),
            title,
            style,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(instance.into()),
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(err) => {
                tracing::warn!(%err, "could not create stimulator window");
                let _ = created.send(0);
                return;
            }
        };
        if kind == Kind::Solid {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 1);
        }
        let _ = created.send(hwnd.0 as usize as u64);
        tracing::info!(?hwnd, x, y, w, h, ?kind, "stimulator window created");
        SetTimer(Some(hwnd), 1, 16, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: called by the window system with a valid HWND for messages we registered for.
    unsafe {
        match msg {
            WM_TIMER => {
                if STOP.load(Ordering::SeqCst) {
                    let _ = DestroyWindow(hwnd);
                } else if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != 1 {
                    TICK.fetch_add(1, Ordering::Relaxed);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn paint(hwnd: HWND) {
    let tick = TICK.load(Ordering::Relaxed);
    let mut client = RECT::default();
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: BeginPaint/EndPaint are paired; brushes are deleted after use.
    unsafe {
        let _ = GetClientRect(hwnd, &mut client);
        let hdc = BeginPaint(hwnd, &mut ps);
        if GetWindowLongPtrW(hwnd, GWLP_USERDATA) == 1 {
            let grey = CreateSolidBrush(COLORREF(0x0080_8080));
            FillRect(hdc, &client, grey);
            let _ = DeleteObject(grey.into());
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        let bg = CreateSolidBrush(COLORREF(hue(tick / 4)));
        FillRect(hdc, &client, bg);
        let _ = DeleteObject(bg.into());

        let width = client.right - client.left;
        let height = client.bottom - client.top;
        let size = (width.min(height) / 4).max(8);
        let span_x = (width - size).max(1);
        let span_y = (height - size).max(1);
        let bx = bounce(i32::try_from(tick).unwrap_or(0) * 7, span_x);
        let by = bounce(i32::try_from(tick).unwrap_or(0) * 5, span_y);
        let block = RECT {
            left: bx,
            top: by,
            right: bx + size,
            bottom: by + size,
        };
        let fg = CreateSolidBrush(COLORREF(hue(tick / 4 + 128)));
        FillRect(hdc, &block, fg);
        let _ = DeleteObject(fg.into());
        let _ = EndPaint(hwnd, &ps);
    }
}

/// Triangle wave between 0 and `span`.
fn bounce(t: i32, span: i32) -> i32 {
    let period = span * 2;
    let m = t.rem_euclid(period);
    if m < span {
        m
    } else {
        period - m
    }
}

/// A fully saturated colour from a 0..=255 hue, as a `COLORREF` (0x00BBGGRR).
fn hue(h: u32) -> u32 {
    let h = h % 256;
    let seg = h / 43;
    let f = (h % 43) * 6;
    let (r, g, b) = match seg {
        0 => (255, f, 0),
        1 => (255 - f, 255, 0),
        2 => (0, 255, f),
        3 => (0, 255 - f, 255),
        4 => (f, 0, 255),
        _ => (255, 0, 255 - f),
    };
    (b.min(255) << 16) | (g.min(255) << 8) | r.min(255)
}
