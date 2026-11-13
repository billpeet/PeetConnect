//! Running `peet-host` as a Windows service for unattended access.
//!
//! The service runs as LocalSystem in session 0, where it cannot see or drive the user's screen.
//! So it launches a worker copy of itself (`--session-worker`) into the active console session,
//! still as SYSTEM, which is what lets the worker use Desktop Duplication and reach the secure
//! desktop. The service watches for console session changes and relaunches the worker so the
//! stream follows the user from sign-in screen to desktop and back.
//!
//! None of this path is exercised by the test machine, which has no administrator rights to
//! install a service; it is implemented to the documented Win32 contracts and verified only by
//! compilation. The interactive `--console` mode remains the tested one.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};

use anyhow::{Context, Result};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityIdentification, SetTokenInformation, TokenPrimary, TokenSessionId,
    TOKEN_ACCESS_MASK, TOKEN_ALL_ACCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
    ENUM_SERVICE_TYPE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE, SERVICE_ACCEPT_SESSIONCHANGE,
    SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW,
    SERVICE_ERROR_NORMAL, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, TerminateProcess,
    WaitForSingleObject, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
    STARTUPINFOW,
};

use crate::config::{AllowConfig, FileConfig};
use crate::Cli;

const SERVICE_NAME: &str = "PeetConnect";
const DISPLAY_NAME: &str = "PeetConnect Host";
const DESCRIPTION: &str = "Serves this machine's screens to authorised tailnet viewers.";

/// Status handle, shared with the control handler. Zero until the service starts.
static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);

fn to_wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The command line the service uses to launch a worker: this executable's arguments with
/// `--service` swapped for `--session-worker`.
fn worker_command_line() -> Result<Vec<u16>> {
    let exe = std::env::current_exe().context("finding our own path")?;
    let mut parts = vec![quote(&exe.to_string_lossy())];
    for arg in std::env::args().skip(1) {
        let mapped = if arg == "--service" {
            "--session-worker".to_string()
        } else {
            arg
        };
        parts.push(quote(&mapped));
    }
    Ok(to_wide(&parts.join(" ")))
}

/// The command line stored in the service's binary path. Settings live in the configuration
/// file, so only an explicit `--config` is carried over.
fn service_bin_path(cli: &Cli) -> Result<String> {
    let exe = std::env::current_exe().context("finding our own path")?;
    let mut parts = vec![quote(&exe.to_string_lossy()), "--service".to_string()];
    if let Some(config) = &cli.config {
        let config = std::path::absolute(config).context("resolving --config")?;
        parts.push("--config".to_string());
        parts.push(quote(&config.to_string_lossy()));
    }
    Ok(parts.join(" "))
}

/// Secures `%ProgramData%\PeetConnect` and writes the configuration file from the command-line
/// flags if it does not exist. An existing file is left alone; flags alongside it are an error,
/// since they would otherwise be silently ignored.
fn prepare_config(cli: &Cli) -> Result<()> {
    crate::machine::secure()?;
    let path = cli
        .config
        .clone()
        .unwrap_or_else(crate::machine::config_path);
    let flags = FileConfig::from_cli(cli);
    if path.exists() {
        anyhow::ensure!(
            flags == FileConfig::default(),
            "{} already exists; edit it instead of passing settings to --install",
            path.display()
        );
        tracing::info!(path = %path.display(), "keeping the existing configuration");
    } else {
        if flags.allow == AllowConfig::default() {
            tracing::warn!(
                "no --allow-* rule given: the service will admit nobody until {} has one",
                path.display()
            );
        }
        std::fs::write(&path, flags.to_file_text()?)
            .with_context(|| format!("writing {}", path.display()))?;
        tracing::info!(path = %path.display(), "wrote the service configuration");
    }
    if path.starts_with(crate::machine::dir()) {
        crate::machine::secure_config(&path)?;
    }
    Ok(())
}

fn quote(s: &str) -> String {
    if s.contains([' ', '\t', '"']) {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Registers the service with the SCM, set to start at boot as LocalSystem.
pub fn install(cli: &Cli) -> Result<()> {
    prepare_config(cli)?;
    let bin_path = service_bin_path(cli)?;
    // SAFETY: Win32 SCM calls with owned wide strings; handles are closed on drop paths below.
    unsafe {
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CREATE_SERVICE)
            .context("OpenSCManager (are you elevated?)")?;
        let name = to_wide(SERVICE_NAME);
        let display = to_wide(DISPLAY_NAME);
        let bin = to_wide(&bin_path);
        let service = CreateServiceW(
            scm,
            PCWSTR(name.as_ptr()),
            PCWSTR(display.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(bin.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(), // LocalSystem
            PCWSTR::null(),
        );
        let service = match service {
            Ok(h) => h,
            Err(err) => {
                let _ = CloseServiceHandle(scm);
                return Err(err).context("CreateService (already installed?)");
            }
        };
        let mut desc_text = to_wide(DESCRIPTION);
        let mut desc = SERVICE_DESCRIPTIONW {
            lpDescription: PWSTR(desc_text.as_mut_ptr()),
        };
        let _ = ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_DESCRIPTION,
            Some(std::ptr::from_mut(&mut desc).cast()),
        );
        let _ = CloseServiceHandle(service);
        let _ = CloseServiceHandle(scm);
    }
    tracing::info!("installed service {SERVICE_NAME}; start it with `sc start {SERVICE_NAME}`");
    Ok(())
}

/// Stops the service if running and removes it from the SCM.
pub fn uninstall() -> Result<()> {
    // SAFETY: Win32 SCM calls with owned wide strings.
    unsafe {
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)
            .context("OpenSCManager (are you elevated?)")?;
        let name = to_wide(SERVICE_NAME);
        let service = OpenServiceW(scm, PCWSTR(name.as_ptr()), SERVICE_ALL_ACCESS);
        let service = match service {
            Ok(h) => h,
            Err(err) => {
                let _ = CloseServiceHandle(scm);
                return Err(err).context("OpenService (not installed?)");
            }
        };
        let mut status = SERVICE_STATUS::default();
        let _ = ControlService(service, SERVICE_CONTROL_STOP, &mut status);
        let result = DeleteService(service);
        let _ = CloseServiceHandle(service);
        let _ = CloseServiceHandle(scm);
        result.context("DeleteService")?;
    }
    tracing::info!("removed service {SERVICE_NAME}");
    Ok(())
}

/// Enters the SCM dispatcher. Blocks until the service stops.
pub fn run_service() -> Result<()> {
    let mut name = to_wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: the table lives for the duration of the blocking call.
    let result = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
    result.context(
        "StartServiceCtrlDispatcher failed; --service is only valid when launched by the \
         Service Control Manager. Use --console to run in the foreground.",
    )?;
    Ok(())
}

/// Messages from the control handler to the service loop.
enum ServiceMsg {
    Stop,
    SessionChange,
    WorkerExited,
}

/// SCM service entry point.
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let (tx, rx) = std::sync::mpsc::channel::<ServiceMsg>();
    // The handler needs the sender; hand it a raw pointer that lives for the service lifetime.
    let ctx = Box::into_raw(Box::new(tx.clone())).cast::<c_void>();
    let name = to_wide(SERVICE_NAME);
    // SAFETY: handler and context are valid for the service lifetime.
    let handle =
        unsafe { RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(handler), Some(ctx)) };
    let Ok(handle) = handle else {
        return;
    };
    STATUS_HANDLE.store(handle.0 as isize, Ordering::SeqCst);
    set_status(SERVICE_START_PENDING, 0);
    set_status(
        SERVICE_RUNNING,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SESSIONCHANGE,
    );

    if let Err(err) = service_loop(&tx, &rx) {
        tracing::error!(%err, "service loop failed");
    }

    set_status(SERVICE_STOPPED, 0);
}

/// Spawns the worker for the active console session and respawns it on session change or crash.
fn service_loop(tx: &Sender<ServiceMsg>, rx: &Receiver<ServiceMsg>) -> Result<()> {
    let cmdline = worker_command_line()?;
    let mut worker = spawn_worker_for_active_session(&cmdline);

    while let Ok(msg) = rx.recv() {
        match msg {
            ServiceMsg::Stop => {
                if let Some(worker) = worker.take() {
                    worker.terminate();
                }
                break;
            }
            ServiceMsg::SessionChange | ServiceMsg::WorkerExited => {
                if let Some(worker) = worker.take() {
                    worker.terminate();
                }
                worker = spawn_worker_for_active_session(&cmdline);
            }
        }
        // A fresh worker needs a watcher so its exit comes back as a message.
        if let Some(w) = worker.as_ref() {
            w.watch(tx.clone());
        }
    }
    Ok(())
}

fn spawn_worker_for_active_session(cmdline: &[u16]) -> Option<Worker> {
    // SAFETY: plain query.
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == 0xFFFF_FFFF {
        tracing::warn!("no active console session; worker not started");
        return None;
    }
    match spawn_worker(session, cmdline) {
        Ok(worker) => Some(worker),
        Err(err) => {
            tracing::warn!(%err, session, "failed to spawn session worker");
            None
        }
    }
}

/// A running worker process.
struct Worker {
    process: HANDLE,
    thread: HANDLE,
}

impl Worker {
    fn terminate(self) {
        // SAFETY: owned handles; TerminateProcess is best-effort.
        unsafe {
            let _ = TerminateProcess(self.process, 1);
            let _ = CloseHandle(self.thread);
            let _ = CloseHandle(self.process);
        }
    }

    /// Spawns a thread that waits for the worker to exit and reports it.
    fn watch(&self, tx: Sender<ServiceMsg>) {
        // HANDLE is a raw pointer and not Send; move it across as an integer and rebuild it.
        let process = self.process.0 as isize;
        std::thread::spawn(move || {
            let process = HANDLE(process as *mut c_void);
            // SAFETY: the process handle outlives this wait because terminate() runs on the
            // service thread only after this reports, or the process ends on its own first.
            unsafe {
                WaitForSingleObject(process, u32::MAX);
            }
            let _ = tx.send(ServiceMsg::WorkerExited);
        });
    }
}

/// Launches the worker as SYSTEM in the given session on the default interactive desktop.
fn spawn_worker(session: u32, cmdline: &[u16]) -> Result<Worker> {
    // SAFETY: a sequence of Win32 calls; every handle opened is closed on all paths.
    unsafe {
        // Prefer the console user's token so the worker inherits the right profile/session; fall
        // back to duplicating our own SYSTEM token retargeted to the session.
        let mut user_token = HANDLE::default();
        let token = if WTSQueryUserToken(session, &mut user_token).is_ok() {
            user_token
        } else {
            duplicate_system_token(session)?
        };

        let mut env: *mut c_void = std::ptr::null_mut();
        let have_env = CreateEnvironmentBlock(&mut env, Some(token), false).is_ok();

        let desktop = to_wide("winsta0\\default");
        let startup = STARTUPINFOW {
            cb: u32::try_from(std::mem::size_of::<STARTUPINFOW>()).unwrap(),
            lpDesktop: PWSTR(desktop.as_ptr().cast_mut()),
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        let mut cmd = cmdline.to_vec();

        let result = CreateProcessAsUserW(
            Some(token),
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            if have_env { Some(env) } else { None },
            PCWSTR::null(),
            &startup,
            &mut info,
        );

        if have_env {
            let _ = DestroyEnvironmentBlock(env);
        }
        let _ = CloseHandle(token);
        result.context("CreateProcessAsUser")?;
        Ok(Worker {
            process: info.hProcess,
            thread: info.hThread,
        })
    }
}

/// Duplicates this process's (SYSTEM) token into a primary token retargeted to `session`.
unsafe fn duplicate_system_token(session: u32) -> Result<HANDLE> {
    // SAFETY: standard token duplication; the returned handle is owned by the caller.
    unsafe {
        let mut process_token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ACCESS_MASK(TOKEN_ALL_ACCESS.0),
            &mut process_token,
        )
        .context("OpenProcessToken")?;
        let mut dup = HANDLE::default();
        let result = DuplicateTokenEx(
            process_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityIdentification,
            TokenPrimary,
            &mut dup,
        );
        let _ = CloseHandle(process_token);
        result.context("DuplicateTokenEx")?;
        if let Err(err) = SetTokenInformation(
            dup,
            TokenSessionId,
            std::ptr::from_ref(&session).cast(),
            u32::try_from(std::mem::size_of::<u32>()).unwrap(),
        ) {
            let _ = CloseHandle(dup);
            return Err(err).context("SetTokenInformation(session)");
        }
        Ok(dup)
    }
}

/// Control handler. `context` is the `Sender<ServiceMsg>` we registered.
unsafe extern "system" fn handler(
    control: u32,
    event_type: u32,
    _event_data: *mut c_void,
    context: *mut c_void,
) -> u32 {
    // SAFETY: context is the pointer we passed to RegisterServiceCtrlHandlerEx; we borrow it.
    let tx = unsafe { &*(context.cast::<Sender<ServiceMsg>>()) };
    match control {
        SERVICE_CONTROL_STOP => {
            set_status(SERVICE_STOP_PENDING, 0);
            let _ = tx.send(ServiceMsg::Stop);
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            // Console connect/disconnect, logon, logoff, lock and unlock all land here; any of
            // them can change which session owns the screen, so respawn for all of them.
            let _ = event_type;
            let _ = tx.send(ServiceMsg::SessionChange);
        }
        _ => {}
    }
    0
}

fn set_status(state: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE, accept: u32) {
    let raw = STATUS_HANDLE.load(Ordering::SeqCst);
    if raw == 0 {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: ENUM_SERVICE_TYPE(SERVICE_WIN32_OWN_PROCESS.0),
        dwCurrentState: state,
        dwControlsAccepted: accept,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    // SAFETY: raw is a valid SERVICE_STATUS_HANDLE stored by service_main.
    unsafe {
        let _ = SetServiceStatus(SERVICE_STATUS_HANDLE(raw as *mut c_void), &status);
    }
}

// `CloseServiceHandle` lives in a different module in the metadata; re-declare the import here to
// keep the SCM handle cleanup local to this file.
use windows::Win32::System::Services::CloseServiceHandle;
