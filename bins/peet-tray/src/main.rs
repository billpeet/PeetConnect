//! `peet-tray`: the PeetConnect notification-area app.
//!
//! Shows whether this machine's host is running, who is connected, and lets the person at the
//! keyboard disconnect viewers, pause new connections, start or stop a console host, and start
//! everything at sign-in. It talks to the host over the local control pipe (`peet-control`),
//! so it works the same whether the host is a console process or the service's worker.
//!
//! Launching the tray starts a console host when none is running after a short grace period
//! (the service's worker may still be starting); `--no-host` only watches.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod link;
#[cfg(windows)]
mod update;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    let start_host = !std::env::args().skip(1).any(|a| a == "--no-host");
    app::run(start_host)
}

#[cfg(not(windows))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("peet-tray currently supports Windows only")
}
