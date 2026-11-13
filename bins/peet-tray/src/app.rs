//! The tray window: a hidden top-level window that owns the notification-area icon, its
//! menu and its balloon notifications. Everything here runs on the UI thread; the control
//! link thread hands over state through [`Shared`] and wakes us with [`WM_APP_UPDATE`].

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use peet_control::{HostMode, HostStatus, Notice, Request};
use tokio::sync::mpsc::UnboundedSender;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_LARGE_ICON,
    NIIF_USER, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NINF_KEY, NIN_SELECT,
    NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetMessageW, GetSystemMetrics, KillTimer, LoadImageW, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetTimer,
    TrackPopupMenu, TranslateMessage, HICON, HMENU, IMAGE_ICON, LR_DEFAULTCOLOR, MF_CHECKED,
    MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, SM_CXICON, SM_CXSMICON, TPM_BOTTOMALIGN, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CONTEXTMENU,
    WM_DESTROY, WM_ENDSESSION, WM_NULL, WM_TIMER, WNDCLASSW,
};

use crate::host;
use crate::link::{self, Shared};

/// Notification-area callback message.
const WM_APP_TRAY: u32 = WM_APP + 1;
/// The link thread changed [`Shared`].
const WM_APP_UPDATE: u32 = WM_APP + 2;
const TRAY_ID: u32 = 1;
const VERSION: &str = env!("CARGO_PKG_VERSION");

const TIMER_AUTOSTART: usize = 1;
const TIMER_RETRY_UPDATE: usize = 2;
/// How long to wait for an existing host (a service worker may be starting) before starting
/// a console host.
const AUTOSTART_GRACE_MS: u32 = 1500;

// Menu command IDs. Sessions use SESSION_BASE + index.
const CMD_PAUSE: usize = 1;
const CMD_START: usize = 2;
const CMD_STOP: usize = 3;
const CMD_RESTART: usize = 4;
const CMD_CONFIG: usize = 5;
const CMD_LOG: usize = 6;
const CMD_AUTOSTART: usize = 7;
const CMD_QUIT: usize = 8;
const SESSION_BASE: usize = 100;

/// Icon resources embedded by build.rs.
struct Icons {
    normal: HICON,
    active: HICON,
    paused: HICON,
    offline: HICON,
    large: HICON,
}

impl Icons {
    fn load(instance: HINSTANCE) -> Result<Self> {
        // LoadImageW rather than LoadIconMetric: the latter lives only in Common Controls v6,
        // which the loader binds just for executables whose manifest asks for it. The process
        // is DPI aware, so the system metrics already reflect the display scale.
        let load = |id: u16, metric| {
            // SAFETY: loads an icon resource from our own module by integer ID.
            unsafe {
                let size = GetSystemMetrics(metric);
                LoadImageW(
                    Some(instance),
                    PCWSTR(usize::from(id) as *const u16),
                    IMAGE_ICON,
                    size,
                    size,
                    LR_DEFAULTCOLOR,
                )
            }
            .map(|handle| HICON(handle.0))
            .with_context(|| format!("loading icon resource {id}"))
        };
        Ok(Self {
            normal: load(1, SM_CXSMICON)?,
            active: load(2, SM_CXSMICON)?,
            paused: load(3, SM_CXSMICON)?,
            offline: load(4, SM_CXSMICON)?,
            large: load(1, SM_CXICON)?,
        })
    }
}

struct App {
    hwnd: HWND,
    icons: Icons,
    shared: Arc<Mutex<Shared>>,
    requests: UnboundedSender<Request>,
    /// Broadcast by Explorer when the taskbar is (re)created; the icon must be re-added.
    taskbar_created: u32,
    /// A console host this tray started; stopped when the tray quits.
    child: Option<Child>,
    /// Session IDs behind the session menu items of the menu currently shown.
    menu_sessions: Vec<u64>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app state. Returns `None` when there is no app yet or it is already
/// borrowed further up this thread's stack, which happens when a call made while holding it
/// (a shell call, a cross-process send) pumps messages back into `window_proc`. Callers
/// either retry later or treat the message as handled; never panic here, since a panic in a
/// window procedure aborts the process.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|app| app.try_borrow_mut().ok()?.as_mut().map(f))
}

/// Work done after the app borrow is released because it may pump messages.
enum Deferred {
    Open(PathBuf),
    EditElevated(PathBuf),
    RestartService,
}

impl Deferred {
    fn run(self) {
        match self {
            Deferred::Open(path) => host::open(&path),
            Deferred::EditElevated(path) => host::edit_elevated(&path),
            Deferred::RestartService => host::restart_service(),
        }
    }
}

/// The host's log: the one it reports (the service worker writes its own), else the file the
/// tray sends a console host's output to.
fn log_path(status: Option<&HostStatus>) -> Option<PathBuf> {
    status
        .and_then(|s| s.log_path.as_ref().map(PathBuf::from))
        .or_else(host::log_path)
}

/// Appends panics to `%LOCALAPPDATA%\PeetConnect\tray.log`: the tray has no console, and a
/// crash report alone says nothing about where it happened.
fn install_panic_log() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(path) = host::log_path().map(|p| p.with_file_name("tray.log")) {
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let backtrace = std::backtrace::Backtrace::force_capture();
                let _ = writeln!(file, "{} panic: {info}\n{backtrace}", now_unix_ms());
            }
        }
        default(info);
    }));
}

pub fn run(start_host: bool) -> Result<()> {
    install_panic_log();
    // SAFETY: plain Win32 calls with constant arguments.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // One tray per sign-in session. The handle is deliberately kept for the process lifetime.
        let _mutex = CreateMutexW(None, true, w!(r"Local\PeetConnectTray"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }
    }
    // SAFETY: querying our own module handle.
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
    let class = w!("PeetConnectTray");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class,
        ..Default::default()
    };
    // SAFETY: `wc` is fully initialised and its strings are static.
    if unsafe { RegisterClassW(&wc) } == 0 {
        anyhow::bail!("RegisterClassW failed");
    }
    // A hidden ordinary top-level window rather than a message-only one: only top-level
    // windows receive the TaskbarCreated broadcast.
    // SAFETY: the class was registered above.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("PeetConnect"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    }?;

    let shared = Arc::new(Mutex::new(Shared::default()));
    let hwnd_raw = hwnd.0 as isize;
    let requests = link::spawn(shared.clone(), move || {
        // SAFETY: posting to our own window; a stale handle at shutdown just fails.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(hwnd_raw as *mut _)),
                WM_APP_UPDATE,
                WPARAM(0),
                LPARAM(0),
            )
        };
    })
    .context("starting the control link")?;

    let app = App {
        hwnd,
        icons: Icons::load(instance)?,
        shared,
        requests,
        // SAFETY: registering a well-known message name.
        taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        child: None,
        menu_sessions: Vec::new(),
    };
    app.add_icon();
    app.refresh();
    APP.with(|slot| *slot.borrow_mut() = Some(app));
    if start_host {
        // SAFETY: timer on our own window.
        unsafe { SetTimer(Some(hwnd), TIMER_AUTOSTART, AUTOSTART_GRACE_MS, None) };
    }

    let mut msg = MSG::default();
    // SAFETY: standard message loop on the thread that owns the window.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    if let Some(mut app) = APP.with(|slot| slot.borrow_mut().take()) {
        app.shutdown();
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // A panic cannot unwind out of a window procedure (the process would abort), so contain
    // it: the panic hook has logged it, and the tray carries on.
    std::panic::catch_unwind(|| handle_message(hwnd, msg, wparam, lparam)).unwrap_or(LRESULT(0))
}

fn handle_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_TRAY => {
            // NOTIFYICON_VERSION_4: the event is in the low word of lParam.
            let event = (lparam.0 & 0xffff) as u32;
            // NIN_KEYSELECT (keyboard activation) is NIN_SELECT | NINF_KEY.
            if event == WM_CONTEXTMENU || event == NIN_SELECT || event == (NIN_SELECT | NINF_KEY) {
                show_menu();
            }
            LRESULT(0)
        }
        WM_APP_UPDATE => {
            let handled = with_app(|app| {
                app.refresh();
                app.show_notices();
            });
            if handled.is_none() {
                // Busy further up the stack; try again shortly.
                // SAFETY: timer on our own window.
                unsafe { SetTimer(Some(hwnd), TIMER_RETRY_UPDATE, 100, None) };
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_RETRY_UPDATE => {
            // SAFETY: our own timer and window.
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_RETRY_UPDATE);
                let _ = PostMessageW(Some(hwnd), WM_APP_UPDATE, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_AUTOSTART => {
            // SAFETY: our own timer.
            let _ = unsafe { KillTimer(Some(hwnd), TIMER_AUTOSTART) };
            with_app(|app| {
                if app.status().is_none() {
                    app.start_host();
                }
            });
            LRESULT(0)
        }
        // Sign-out, shutdown, or Restart Manager closing us for an install: the process may be
        // terminated as soon as this returns, so stop the host we own now, not after the loop.
        WM_ENDSESSION if wparam.0 != 0 => {
            if let Some(mut app) = APP.with(|slot| slot.try_borrow_mut().ok()?.take()) {
                app.shutdown();
            }
            std::process::exit(0);
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => {
            if with_app(|app| msg == app.taskbar_created && msg != 0) == Some(true) {
                with_app(|app| {
                    app.add_icon();
                    app.refresh();
                });
                return LRESULT(0);
            }
            // SAFETY: default handling for everything else.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

/// Builds and shows the menu, then acts on the choice. The `App` borrow is released while
/// the menu is open: `TrackPopupMenu` runs a modal loop that dispatches our own messages.
fn show_menu() {
    let Some(Ok((menu, hwnd))) = with_app(App::build_menu) else {
        return;
    };
    let mut point = POINT::default();
    // SAFETY: standard tray-menu sequence on our own window; the menu is destroyed after use.
    let choice = unsafe {
        let _ = GetCursorPos(&mut point);
        // Without this the menu does not close when the user clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let choice = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RETURNCMD | TPM_NONOTIFY,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        choice.0 as usize
    };
    if choice != 0 {
        if let Some(deferred) = with_app(|app| app.command(choice)).flatten() {
            deferred.run();
        }
    }
}

impl App {
    fn status(&self) -> Option<HostStatus> {
        self.shared.lock().expect("shared poisoned").status.clone()
    }

    fn notify_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: u32::try_from(std::mem::size_of::<NOTIFYICONDATAW>()).unwrap_or(0),
            hWnd: self.hwnd,
            uID: TRAY_ID,
            ..Default::default()
        }
    }

    fn add_icon(&self) {
        let mut data = self.notify_data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = WM_APP_TRAY;
        data.hIcon = self.icons.offline;
        copy_wide(&mut data.szTip, "PeetConnect");
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // SAFETY: `data` is a fully initialised NOTIFYICONDATAW for our window.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &data);
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
        }
    }

    /// Updates the icon and tooltip from the current status.
    fn refresh(&self) {
        let status = self.status();
        let (icon, tip) = match &status {
            None => (
                self.icons.offline,
                "PeetConnect\nHost not running".to_string(),
            ),
            Some(s) => {
                let icon = if s.paused {
                    self.icons.paused
                } else if s.sessions.is_empty() {
                    self.icons.normal
                } else {
                    self.icons.active
                };
                (icon, tooltip(s))
            }
        };
        let mut data = self.notify_data();
        data.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.hIcon = icon;
        copy_wide(&mut data.szTip, &tip);
        // SAFETY: modifying our own icon.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }

    fn show_notices(&self) {
        let notices = std::mem::take(&mut self.shared.lock().expect("shared poisoned").notices);
        let Some((title, text)) = notice_text(&notices) else {
            return;
        };
        self.balloon(&title, &text);
    }

    fn balloon(&self, title: &str, text: &str) {
        let mut data = self.notify_data();
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
        data.hBalloonIcon = self.icons.large;
        copy_wide(&mut data.szInfoTitle, title);
        copy_wide(&mut data.szInfo, text);
        // SAFETY: modifying our own icon.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }

    fn build_menu(&mut self) -> Result<(HMENU, HWND)> {
        let status = self.status();
        let owns_child = self.child_running();
        // SAFETY: a fresh popup menu, destroyed by `show_menu`.
        let menu = unsafe { CreatePopupMenu() }?;
        let item = |flags, id: usize, text: &str| {
            let wide = wide(text);
            // SAFETY: `wide` outlives the call; the menu copies the text.
            unsafe { AppendMenuW(menu, flags, id, PCWSTR(wide.as_ptr())) }
        };
        let separator = || {
            // SAFETY: as above.
            unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) }
        };
        let check = |on: bool| {
            if on {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            }
        };

        self.menu_sessions.clear();
        match &status {
            None => {
                item(
                    MF_STRING | MF_GRAYED,
                    0,
                    &format!("PeetConnect {VERSION}: host not running"),
                )?;
            }
            Some(s) => {
                let service = if s.mode == HostMode::ServiceWorker {
                    " (service)"
                } else {
                    ""
                };
                // The host is normally the same version; say so when it is not.
                let host_version = if s.version == VERSION {
                    String::new()
                } else {
                    format!(", host {}", s.version)
                };
                item(
                    MF_STRING | MF_GRAYED,
                    0,
                    &format!("PeetConnect {VERSION}: {}{service}{host_version}", s.name),
                )?;
                item(MF_STRING | MF_GRAYED, 0, &state_line(s))?;
                separator()?;
                if s.sessions.is_empty() {
                    item(MF_STRING | MF_GRAYED, 0, "No viewers connected")?;
                }
                let now = now_unix_ms();
                for (i, session) in s.sessions.iter().enumerate() {
                    let minutes = now.saturating_sub(session.started_unix_ms) / 60_000;
                    item(
                        MF_STRING,
                        SESSION_BASE + i,
                        &format!("Disconnect {}\t{}", session.describe(), duration(minutes)),
                    )?;
                    self.menu_sessions.push(session.id);
                }
            }
        }
        separator()?;
        let connected = status.is_some();
        let paused = status.as_ref().is_some_and(|s| s.paused);
        let console = status.as_ref().is_some_and(|s| s.mode == HostMode::Console);
        item(
            if connected {
                check(paused)
            } else {
                MF_STRING | MF_GRAYED
            },
            CMD_PAUSE,
            "Pause new connections",
        )?;
        if connected {
            if console {
                if owns_child {
                    item(MF_STRING, CMD_RESTART, "Restart host")?;
                }
                item(MF_STRING, CMD_STOP, "Stop host")?;
            } else {
                item(MF_STRING, CMD_RESTART, "Restart service")?;
            }
        } else {
            item(MF_STRING, CMD_START, "Start host")?;
        }
        separator()?;
        item(MF_STRING, CMD_CONFIG, "Open configuration")?;
        let log_exists = log_path(status.as_ref()).is_some_and(|p| p.exists());
        item(
            if log_exists {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            },
            CMD_LOG,
            "Open host log",
        )?;
        item(
            check(host::autostart_enabled()),
            CMD_AUTOSTART,
            "Start with Windows",
        )?;
        separator()?;
        item(
            MF_STRING,
            CMD_QUIT,
            if owns_child {
                "Quit (stops the host)"
            } else {
                "Quit"
            },
        )?;
        Ok((menu, self.hwnd))
    }

    /// Acts on a menu choice. Anything that may pump messages (opening files through the
    /// shell) is returned for the caller to run once the app borrow is released.
    fn command(&mut self, id: usize) -> Option<Deferred> {
        let status = self.status();
        match id {
            CMD_PAUSE => {
                if let Some(s) = status {
                    self.request(Request::SetPaused { paused: !s.paused });
                }
            }
            CMD_START => self.start_host(),
            CMD_STOP => self.stop_host(),
            CMD_RESTART => {
                if status.is_some_and(|s| s.mode == HostMode::ServiceWorker) {
                    return Some(Deferred::RestartService);
                }
                self.stop_host();
                self.start_host();
            }
            CMD_CONFIG => {
                if let Some(s) = status
                    .as_ref()
                    .filter(|s| s.mode == HostMode::ServiceWorker)
                {
                    // Machine-wide and admin-only; the service created it at install.
                    return s
                        .config_path
                        .as_ref()
                        .map(|p| Deferred::EditElevated(PathBuf::from(p)));
                }
                let path = status
                    .and_then(|s| s.config_path.map(PathBuf::from))
                    .or_else(host::default_config_path);
                if let Some(path) = path {
                    match host::ensure_config(&path) {
                        Ok(()) => return Some(Deferred::Open(path)),
                        Err(err) => {
                            self.balloon("Could not open the configuration", &format!("{err:#}"));
                        }
                    }
                }
            }
            CMD_LOG => return log_path(status.as_ref()).map(Deferred::Open),
            CMD_AUTOSTART => {
                if let Err(err) = host::set_autostart(!host::autostart_enabled()) {
                    self.balloon("Could not change Start with Windows", &format!("{err:#}"));
                }
            }
            CMD_QUIT => {
                // SAFETY: ends the message loop; cleanup happens after it returns.
                unsafe { PostQuitMessage(0) };
            }
            id if id >= SESSION_BASE => {
                if let Some(&session) = self.menu_sessions.get(id - SESSION_BASE) {
                    self.request(Request::Disconnect { session });
                }
            }
            _ => {}
        }
        None
    }

    fn request(&self, request: Request) {
        let _ = self.requests.send(request);
    }

    fn child_running(&mut self) -> bool {
        match &mut self.child {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn start_host(&mut self) {
        if self.child_running() {
            return;
        }
        match host::start() {
            Ok(child) => self.child = Some(child),
            Err(err) => self.balloon("Could not start the host", &format!("{err:#}")),
        }
    }

    /// Asks the host to stop and, if it is ours, waits for it (killing it after a grace
    /// period so a wedged host cannot keep sharing the screen).
    fn stop_host(&mut self) {
        self.request(Request::Shutdown);
        if let Some(mut child) = self.child.take() {
            host::wait_or_kill(&mut child, Duration::from_secs(3));
        }
    }

    fn shutdown(&mut self) {
        if self.child_running() {
            self.stop_host();
        }
        let data = self.notify_data();
        // SAFETY: removing our own icon.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }
}

fn state_line(s: &HostStatus) -> String {
    let port = s.listen.rsplit(':').next().unwrap_or(&s.listen);
    if !s.admits_anyone {
        "Nobody is allowed to connect: open the configuration".to_string()
    } else if s.paused {
        "Paused: new viewers are turned away".to_string()
    } else {
        let pin = if s.pin_required { ", PIN required" } else { "" };
        format!("Accepting viewers on port {port}{pin}")
    }
}

fn tooltip(s: &HostStatus) -> String {
    let detail = match s.sessions.as_slice() {
        [] if !s.admits_anyone => "Nobody is allowed to connect".to_string(),
        [] if s.paused => "Paused".to_string(),
        [] => "No viewers".to_string(),
        [one] => format!("Viewed by {}", one.describe()),
        many => format!("{} viewers connected", many.len()),
    };
    format!("PeetConnect: {}\n{detail}", s.name)
}

/// Title and body for a batch of notices, or `None` when there is nothing to say.
fn notice_text(notices: &[Notice]) -> Option<(String, String)> {
    let line = |notice: &Notice| match notice {
        Notice::Connected { session } => (
            "Viewer connected",
            format!("{} is viewing this computer", session.describe()),
        ),
        Notice::Disconnected { session, reason } => (
            "Viewer disconnected",
            format!("{} left ({reason})", session.describe()),
        ),
        Notice::Rejected { who, reason } => ("Connection refused", format!("{who}: {reason}")),
    };
    match notices {
        [] => None,
        [one] => {
            let (title, text) = line(one);
            Some((title.to_string(), text))
        }
        many => Some((
            "PeetConnect".to_string(),
            many.iter()
                .map(|n| line(n).1)
                .collect::<Vec<_>>()
                .join("\n"),
        )),
    }
}

fn duration(minutes: u64) -> String {
    match minutes {
        0 => "just now".to_string(),
        m if m < 60 => format!("{m} min"),
        m => format!("{} h {} min", m / 60, m % 60),
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copies `text` into a fixed NUL-terminated UTF-16 buffer, truncating with an ellipsis.
fn copy_wide(buf: &mut [u16], text: &str) {
    let units: Vec<u16> = text.encode_utf16().collect();
    let max = buf.len() - 1;
    let (body, ellipsis) = if units.len() > max {
        (&units[..max - 1], true)
    } else {
        (&units[..], false)
    };
    buf[..body.len()].copy_from_slice(body);
    let mut end = body.len();
    if ellipsis {
        buf[end] = 0x2026; // '…'
        end += 1;
    }
    buf[end] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use peet_control::SessionInfo;

    fn session(user: Option<&str>) -> SessionInfo {
        SessionInfo {
            id: 1,
            node: "laptop".into(),
            user: user.map(Into::into),
            remote: String::new(),
            started_unix_ms: 0,
        }
    }

    #[test]
    fn copy_wide_truncates_with_ellipsis() {
        let mut buf = [0u16; 6];
        copy_wide(&mut buf, "abcdefgh");
        assert_eq!(String::from_utf16_lossy(&buf[..5]), "abcd\u{2026}");
        assert_eq!(buf[5], 0);
        copy_wide(&mut buf, "ab");
        assert_eq!(&buf[..3], &[u16::from(b'a'), u16::from(b'b'), 0]);
    }

    #[test]
    fn notices_read_naturally() {
        let one = [Notice::Connected {
            session: session(Some("a@b.c")),
        }];
        assert_eq!(
            notice_text(&one),
            Some((
                "Viewer connected".into(),
                "a@b.c on laptop is viewing this computer".into()
            ))
        );
        let two = [
            Notice::Rejected {
                who: "100.64.0.9".into(),
                reason: "not allowed".into(),
            },
            Notice::Disconnected {
                session: session(None),
                reason: "viewer disconnected".into(),
            },
        ];
        let (title, text) = notice_text(&two).unwrap();
        assert_eq!(title, "PeetConnect");
        assert_eq!(
            text,
            "100.64.0.9: not allowed\nlaptop left (viewer disconnected)"
        );
        assert!(notice_text(&[]).is_none());
    }

    #[test]
    fn durations() {
        assert_eq!(duration(0), "just now");
        assert_eq!(duration(12), "12 min");
        assert_eq!(duration(125), "2 h 5 min");
    }
}
