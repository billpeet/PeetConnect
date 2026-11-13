//! The service's updater, for per-machine installs (`%ProgramFiles%\PeetConnect`).
//!
//! Same schedule as the tray's in a per-user install: a check two minutes after the service
//! starts and every six hours or so after that. A newer release is downloaded into
//! `%ProgramData%\PeetConnect\updates` (writable only by SYSTEM and Administrators, so nobody
//! can swap the installer the service will run as SYSTEM) and installed once nothing is in use.
//! Progress goes to `%ProgramData%\PeetConnect\update.json` for the tray.
//!
//! Before installing, the service notes which sessions have a tray running and marks the state
//! `installing`, which makes those trays quit. The installer stops the service, replaces the
//! files and starts it again; the new service then starts the trays again ([`after_start`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use peet_update::{Installation, Scope, Silence, Status, Verified, VERSION};

const FIRST_CHECK: Duration = Duration::from_secs(2 * 60);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const CHECK_JITTER_SECS: u64 = 60 * 60;
const IDLE_POLL: Duration = Duration::from_secs(10 * 60);
/// How long trays get to quit (they reread the state file every 20 s) before the installer
/// closes whatever is left.
const TRAY_GRACE: Duration = Duration::from_secs(30);

fn state_file() -> PathBuf {
    crate::machine::dir().join("update.json")
}

fn staging() -> PathBuf {
    crate::machine::dir().join("updates")
}

/// Sessions whose tray the service closed for an update.
fn relaunch_file() -> PathBuf {
    staging().join("relaunch-trays.json")
}

/// Starts the update thread when this service belongs to a per-machine install.
pub fn spawn(automatic: bool) {
    let Some(installation) = peet_update::installation().filter(|i| i.scope == Scope::Machine)
    else {
        tracing::info!("not a per-machine install; automatic updates are off");
        return;
    };
    let _ = std::thread::Builder::new()
        .name("updater".into())
        .spawn(move || run(&installation, automatic));
}

fn write_state(status: &Status) {
    let path = state_file();
    let result = serde_json::to_vec_pretty(status)
        .map_err(anyhow::Error::from)
        .and_then(|text| {
            // Write and rename, so the tray never reads half a file.
            let temp = path.with_extension("json.tmp");
            std::fs::write(&temp, text)?;
            std::fs::rename(&temp, &path)?;
            Ok(())
        });
    if let Err(err) = result {
        tracing::warn!("writing {}: {err:#}", path.display());
    }
}

fn run(installation: &Installation, automatic: bool) {
    tracing::info!(automatic, "update checks on");
    let mut next_check = Instant::now() + FIRST_CHECK;
    let mut ready: Option<(Verified, u64)> = None;
    let mut failing_since: Option<u64> = None;
    loop {
        let now = Instant::now();
        let wake = if ready.is_some() {
            next_check.min(now + IDLE_POLL)
        } else {
            next_check
        };
        std::thread::sleep(wake.saturating_duration_since(now));

        if Instant::now() >= next_check {
            next_check = Instant::now() + CHECK_INTERVAL + jitter();
            match check_and_download(ready.as_ref().map(|(v, _)| v)) {
                Ok(None) => {
                    failing_since = None;
                    if ready.is_none() {
                        write_state(&Status::UpToDate {
                            checked: peet_update::now_unix(),
                        });
                    }
                }
                Ok(Some(installer)) => {
                    failing_since = None;
                    tracing::info!(version = %installer.version, "update downloaded and verified");
                    ready = Some((installer, peet_update::now_unix()));
                }
                Err(err) => {
                    tracing::warn!("update check failed: {err:#}");
                    let since = *failing_since.get_or_insert_with(peet_update::now_unix);
                    if ready.is_none() {
                        write_state(&Status::Failed {
                            message: format!("{err:#}"),
                            since,
                        });
                    }
                }
            }
        }

        let Some((installer, since)) = &ready else {
            continue;
        };
        let waiting_for = in_the_way(&installation.dir);
        if automatic && waiting_for.is_empty() {
            let (installer, _) = ready.take().expect("checked above");
            if let Err(err) = install(installation, &installer) {
                tracing::warn!("could not start the installer: {err:#}");
                write_state(&Status::Failed {
                    message: format!("{err:#}"),
                    since: peet_update::now_unix(),
                });
            }
            continue;
        }
        write_state(&Status::Ready {
            version: installer.version.clone(),
            automatic,
            since: *since,
            waiting_for,
        });
    }
}

fn check_and_download(have: Option<&Verified>) -> Result<Option<Verified>> {
    let Some(manifest) = peet_update::check(VERSION)? else {
        return Ok(None);
    };
    if have.is_some_and(|v| !peet_update::is_newer(&manifest.version, &v.version)) {
        return Ok(None);
    }
    write_state(&Status::Downloading {
        version: manifest.version.clone(),
    });
    peet_update::download(&manifest, &staging()).map(Some)
}

/// What an install would interrupt: viewers connected to the worker, and viewer windows or
/// `peet.exe` processes of ours in any session.
fn in_the_way(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    match host_sessions() {
        0 => {}
        1 => found.push("a connected viewer".to_string()),
        n => found.push(format!("{n} connected viewers")),
    }
    for blocker in peet_update::blockers(dir) {
        let text = blocker.describe();
        if !found.contains(&text) {
            found.push(text);
        }
    }
    found
}

/// Viewers connected to the session worker, read from its control pipe. Zero when no worker
/// is running.
fn host_sessions() -> usize {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return 0;
    };
    runtime.block_on(async {
        let Ok(pipe) = peet_control::connect().await else {
            return 0;
        };
        let mut reader = tokio::io::BufReader::new(pipe);
        let first = tokio::time::timeout(
            Duration::from_secs(5),
            peet_control::read_message::<_, peet_control::HostMessage>(&mut reader),
        )
        .await;
        match first {
            Ok(Ok(Some(peet_control::HostMessage::Status(status)))) => status.sessions.len(),
            _ => 0,
        }
    })
}

fn install(installation: &Installation, installer: &Verified) -> Result<()> {
    let trays: Vec<u32> = peet_update::processes(&installation.dir)
        .into_iter()
        .filter(|p| p.name.eq_ignore_ascii_case("peet-tray.exe"))
        .map(|p| p.session)
        .collect();
    std::fs::write(relaunch_file(), serde_json::to_vec(&trays)?)
        .context("recording the trays to start again")?;
    write_state(&Status::Installing {
        version: installer.version.clone(),
    });
    // The trays poll the state file; give them a moment to leave on their own.
    let deadline = Instant::now() + TRAY_GRACE;
    while Instant::now() < deadline
        && peet_update::processes(&installation.dir)
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case("peet-tray.exe"))
    {
        std::thread::sleep(Duration::from_millis(500));
    }
    tracing::info!(version = %installer.version, "starting the installer");
    let log = crate::machine::log_dir().join("install.log");
    peet_update::launch(installer, Scope::Machine, Silence::Full, Some(&log))
}

/// Run once the service is up: starts the trays an update closed, in the sessions they ran in.
pub fn after_start(launch: impl Fn(u32, &Path) -> Result<()>) {
    let path = relaunch_file();
    let Ok(text) = std::fs::read(&path) else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    let Ok(sessions) = serde_json::from_slice::<Vec<u32>>(&text) else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let tray = exe.with_file_name("peet-tray.exe");
    for session in sessions {
        match launch(session, &tray) {
            Ok(()) => tracing::info!(session, "started the tray again after the update"),
            Err(err) => tracing::warn!(session, "could not start the tray again: {err:#}"),
        }
    }
}

fn jitter() -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    Duration::from_secs(nanos % CHECK_JITTER_SECS)
}
