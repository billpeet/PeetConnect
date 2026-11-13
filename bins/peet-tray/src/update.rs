//! The tray's updater thread.
//!
//! In a per-user install the tray owns updates: it checks two minutes after starting and then
//! every six hours or so, downloads and verifies a newer release, and installs it silently
//! once nothing is in use (no viewer connected to the host, no viewer window or `peet.exe` of
//! ours running). With "Install updates automatically" off it waits for "Install now" instead.
//! The installer closes the tray and starts it again.
//!
//! In a per-machine install the service owns updates and writes its progress to
//! `%ProgramData%\PeetConnect\update.json`; the tray shows that, quits when the service starts
//! the installer (the service starts the tray again afterwards), and offers "Install now", which
//! runs the installer with an administrator prompt.
//!
//! Development builds and portable copies are not installed, so they never update.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use peet_update::{Installation, Manifest, Scope, Silence, Status, Verified, VERSION};
use windows::core::w;
use windows::Win32::System::Registry::{
    RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, REG_SZ, RRF_RT_REG_DWORD,
    RRF_RT_REG_SZ,
};

use crate::link::Shared;

const FIRST_CHECK: Duration = Duration::from_secs(2 * 60);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Spread checks over an extra hour so machines do not all ask at once.
const CHECK_JITTER_SECS: u64 = 60 * 60;
/// How often a downloaded update looks for a moment when nothing is in use.
const IDLE_POLL: Duration = Duration::from_secs(10 * 60);
/// After this long waiting, say what is in the way.
const NAG_AFTER_SECS: u64 = 3 * 24 * 60 * 60;
/// Failures are quiet until they have lasted this long.
const FAILURE_NOTICE_AFTER_SECS: u64 = 24 * 60 * 60;
/// An installer that has not closed us by then has failed.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How often the per-machine tray rereads the service's state file.
const MACHINE_POLL: Duration = Duration::from_secs(20);

const SETTINGS_KEY: windows::core::PCWSTR = w!(r"Software\PeetConnect");

/// Requests from the menu.
#[derive(Debug, Clone, Copy)]
pub enum Command {
    CheckNow,
    InstallNow,
    /// A setting changed: re-evaluate without checking.
    Recheck,
}

/// Starts the updater thread for the installation this tray belongs to; `None` for a build that
/// is not installed. `wake` is called after every change to [`Shared`].
pub fn spawn(
    shared: Arc<Mutex<Shared>>,
    wake: impl Fn() + Send + 'static,
) -> Option<(Scope, Sender<Command>)> {
    let installation = peet_update::installation()?;
    let scope = installation.scope;
    let (tx, rx) = std::sync::mpsc::channel();
    let updater = Updater {
        installation,
        shared,
        wake: Box::new(wake),
        commands: rx,
    };
    std::thread::Builder::new()
        .name("updater".into())
        .spawn(move || match updater.installation.scope {
            Scope::User => updater.run_user(),
            Scope::Machine => updater.run_machine(),
        })
        .ok()?;
    Some((scope, tx))
}

struct Updater {
    installation: Installation,
    shared: Arc<Mutex<Shared>>,
    wake: Box<dyn Fn() + Send>,
    commands: Receiver<Command>,
}

impl Updater {
    fn set_status(&self, status: Status) {
        let changed = {
            let mut shared = self.shared.lock().expect("shared poisoned");
            let changed = shared.update != status;
            shared.update = status;
            changed
        };
        if changed {
            (self.wake)();
        }
    }

    fn notify(&self, title: &str, text: &str) {
        self.shared
            .lock()
            .expect("shared poisoned")
            .update_notices
            .push((title.to_string(), text.to_string()));
        (self.wake)();
    }

    /// Waits up to `timeout` for a menu command. `Err` when the tray is gone.
    fn wait(&self, timeout: Duration) -> Result<Option<Command>, ()> {
        match self.commands.recv_timeout(timeout) {
            Ok(command) => Ok(Some(command)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(()),
        }
    }

    /// Nothing that an install would interrupt: no viewer on the host, no viewer window or
    /// `peet.exe` of ours. Returns what is in the way otherwise.
    fn in_the_way(&self) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        let sessions = self
            .shared
            .lock()
            .expect("shared poisoned")
            .status
            .as_ref()
            .map_or(0, |s| s.sessions.len());
        if sessions > 0 {
            found.push(if sessions == 1 {
                "a connected viewer".to_string()
            } else {
                format!("{sessions} connected viewers")
            });
        }
        for blocker in peet_update::blockers(&self.installation.dir) {
            let text = blocker.describe();
            if !found.contains(&text) {
                found.push(text);
            }
        }
        found
    }

    // ---- per-user install ----

    fn run_user(self) {
        let mut next_check = Instant::now() + FIRST_CHECK;
        let mut ready: Option<(Verified, u64)> = None;
        let mut failing_since: Option<u64> = None;
        let mut failure_noticed = false;
        let mut nagged = false;
        let mut manual = false;
        loop {
            let now = Instant::now();
            let timeout = if ready.is_some() {
                next_check.min(now + IDLE_POLL)
            } else {
                next_check
            }
            .saturating_duration_since(now);
            match self.wait(timeout) {
                Err(()) => return,
                Ok(Some(Command::CheckNow)) => {
                    manual = true;
                    next_check = Instant::now();
                }
                Ok(Some(Command::InstallNow)) => {
                    if let Some((installer, _)) = ready.take() {
                        self.install(&installer, Silence::Progress);
                    } else {
                        manual = true;
                        next_check = Instant::now();
                    }
                }
                Ok(Some(Command::Recheck) | None) => {}
            }

            if Instant::now() >= next_check {
                next_check = Instant::now() + CHECK_INTERVAL + jitter();
                match self.check_and_download(ready.as_ref().map(|(v, _)| v)) {
                    Ok(None) => {
                        failing_since = None;
                        failure_noticed = false;
                        if ready.is_none() {
                            self.set_status(Status::UpToDate {
                                checked: peet_update::now_unix(),
                            });
                            if manual {
                                self.notify(
                                    "PeetConnect is up to date",
                                    &format!("Version {VERSION} is the newest release."),
                                );
                            }
                        }
                    }
                    Ok(Some(installer)) => {
                        failing_since = None;
                        failure_noticed = false;
                        nagged = false;
                        log(&format!("downloaded and verified {}", installer.version));
                        ready = Some((installer, peet_update::now_unix()));
                    }
                    Err(err) => {
                        log(&format!("update check failed: {err:#}"));
                        let since = *failing_since.get_or_insert_with(peet_update::now_unix);
                        if ready.is_none() {
                            self.set_status(Status::Failed {
                                message: format!("{err:#}"),
                                since,
                            });
                        }
                        let long = peet_update::now_unix().saturating_sub(since)
                            > FAILURE_NOTICE_AFTER_SECS;
                        if manual || (long && !failure_noticed) {
                            failure_noticed = true;
                            self.notify(
                                "Could not check for PeetConnect updates",
                                &format!("{err:#}"),
                            );
                        }
                    }
                }
                manual = false;
            }

            let Some((installer, since)) = &ready else {
                continue;
            };
            let automatic = auto_install();
            let waiting_for = self.in_the_way();
            if automatic && waiting_for.is_empty() {
                let (installer, _) = ready.take().expect("checked above");
                self.install(&installer, Silence::Full);
                continue;
            }
            if automatic
                && !nagged
                && peet_update::now_unix().saturating_sub(*since) > NAG_AFTER_SECS
            {
                nagged = true;
                self.notify(
                    &format!("PeetConnect {} is waiting to install", installer.version),
                    &format!(
                        "It installs once nothing is in use, but {} has been running for days. \
                         Choose \"Install now\" in the tray menu to close it and update.",
                        waiting_for.join(" and ")
                    ),
                );
            }
            self.set_status(Status::Ready {
                version: installer.version.clone(),
                automatic,
                since: *since,
                waiting_for,
            });
        }
    }

    /// Checks for a newer release and downloads it. `None` when there is nothing newer than
    /// this build and what is already downloaded.
    fn check_and_download(&self, have: Option<&Verified>) -> Result<Option<Verified>> {
        let Some(manifest) = peet_update::check(VERSION)? else {
            return Ok(None);
        };
        if have.is_some_and(|v| !peet_update::is_newer(&manifest.version, &v.version)) {
            return Ok(None);
        }
        self.download(&manifest).map(Some)
    }

    fn download(&self, manifest: &Manifest) -> Result<Verified> {
        self.set_status(Status::Downloading {
            version: manifest.version.clone(),
        });
        peet_update::download(manifest, &staging())
    }

    /// Starts the installer and waits for it to close the tray. Returns only if it did not.
    fn install(&self, installer: &Verified, silence: Silence) {
        self.set_status(Status::Installing {
            version: installer.version.clone(),
        });
        let log_file = staging().join("install.log");
        log(&format!("installing {}", installer.version));
        if let Err(err) =
            peet_update::launch(installer, self.installation.scope, silence, Some(&log_file))
        {
            log(&format!("could not start the installer: {err:#}"));
            self.fail_install(&format!("{err:#}"));
            return;
        }
        // The installer closes us; if it is still running after this long, it failed (or the
        // person declined the administrator prompt).
        let deadline = Instant::now() + INSTALL_TIMEOUT;
        while Instant::now() < deadline {
            if self.wait(Duration::from_secs(5)).is_err() {
                return;
            }
        }
        log("the installer did not finish");
        self.fail_install(&format!(
            "the installer did not finish; see {}",
            log_file.display()
        ));
    }

    fn fail_install(&self, message: &str) {
        self.set_status(Status::Failed {
            message: message.to_string(),
            since: peet_update::now_unix(),
        });
        self.notify("PeetConnect could not update", message);
    }

    // ---- per-machine install ----

    fn run_machine(self) {
        let state_file = machine_state_file();
        loop {
            let status = read_machine_state(&state_file);
            if let Some(Status::Installing { .. }) = &status {
                // The service is about to replace our files; get out of the way. It starts the
                // tray again once the update is in.
                log("the service is installing an update; quitting");
                self.shared.lock().expect("shared poisoned").quit = true;
                (self.wake)();
                return;
            }
            self.set_status(status.unwrap_or_default());
            match self.wait(MACHINE_POLL) {
                Err(()) => return,
                Ok(Some(Command::InstallNow)) => self.install_machine_now(),
                Ok(Some(Command::CheckNow | Command::Recheck) | None) => {}
            }
        }
    }

    /// "Install now" in a per-machine install: download as this user and run the installer,
    /// which asks for administrator rights.
    fn install_machine_now(&self) {
        let result = peet_update::check(VERSION).and_then(|manifest| match manifest {
            Some(manifest) => self.download(&manifest).map(Some),
            None => Ok(None),
        });
        match result {
            Ok(Some(installer)) => self.install(&installer, Silence::Progress),
            Ok(None) => self.notify(
                "PeetConnect is up to date",
                &format!("Version {VERSION} is the newest release."),
            ),
            Err(err) => {
                log(&format!("update failed: {err:#}"));
                self.notify("PeetConnect could not update", &format!("{err:#}"));
            }
        }
    }
}

/// `%ProgramData%\PeetConnect\update.json`, written by the service.
fn machine_state_file() -> PathBuf {
    std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
        .join("PeetConnect")
        .join("update.json")
}

/// The service's state, ignoring an `Installing` left behind long ago by an install that never
/// restarted the service.
fn read_machine_state(path: &Path) -> Option<Status> {
    let text = std::fs::read(path).ok()?;
    let status: Status = serde_json::from_slice(&text).ok()?;
    if matches!(status, Status::Installing { .. }) {
        let age = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .unwrap_or(Duration::MAX);
        if age > Duration::from_secs(10 * 60) {
            return None;
        }
    }
    Some(status)
}

fn jitter() -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    Duration::from_secs(nanos % CHECK_JITTER_SECS)
}

fn staging() -> PathBuf {
    local_dir().join("updates")
}

fn local_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("PeetConnect")
}

/// Appends a line to `%LOCALAPPDATA%\PeetConnect\tray.log`, starting it afresh past 1 MB.
pub fn log(message: &str) {
    let path = local_dir().join("tray.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1024 * 1024) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{} {message}", peet_update::now_unix());
    }
}

/// "Install updates automatically" (per-user installs). On unless switched off.
pub fn auto_install() -> bool {
    let mut value = 1u32;
    let mut size = 4u32;
    // SAFETY: a DWORD read into a local of the right size.
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            SETTINGS_KEY,
            w!("AutoInstallUpdates"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    result.is_err() || value != 0
}

pub fn set_auto_install(on: bool) -> Result<()> {
    let value = u32::from(on);
    // SAFETY: writes a DWORD from a local.
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            SETTINGS_KEY,
            w!("AutoInstallUpdates"),
            REG_DWORD.0,
            Some((&raw const value).cast()),
            4,
        )
    }
    .ok()?;
    Ok(())
}

/// Records this version as the one last run and returns the previous one when this is newer,
/// so the tray can say "Updated to …" once, however the update arrived.
pub fn take_upgrade() -> Option<String> {
    let mut buffer = [0u16; 64];
    let mut size = u32::try_from(std::mem::size_of_val(&buffer)).unwrap_or(0);
    // SAFETY: the buffer and its size in bytes describe memory we own.
    let previous = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            SETTINGS_KEY,
            w!("LastVersion"),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    }
    .ok()
    .ok()
    .map(|()| {
        let len = (size as usize / 2).saturating_sub(1).min(buffer.len());
        String::from_utf16_lossy(&buffer[..len])
    });
    if previous.as_deref() != Some(VERSION) {
        let wide: Vec<u16> = VERSION.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: a NUL-terminated UTF-16 string of the given size in bytes.
        let _ = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                SETTINGS_KEY,
                w!("LastVersion"),
                REG_SZ.0,
                Some(wide.as_ptr().cast()),
                u32::try_from(wide.len() * 2).unwrap_or(0),
            )
        };
    }
    previous.filter(|p| peet_update::is_newer(VERSION, p))
}
