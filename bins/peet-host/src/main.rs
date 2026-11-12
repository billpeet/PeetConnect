//! `peet-host`: serves this machine's screens to authorised tailnet viewers.
//!
//! Milestone 3 build: runs as a plain user process in the interactive session. The Windows
//! service, secure-desktop capture and input injection arrive in milestone 4.

use std::net::IpAddr;
use std::path::PathBuf;

use clap::Parser;

mod config;

#[derive(Debug, Parser)]
#[command(name = "peet-host", version, about = "PeetConnect host")]
#[allow(clippy::struct_excessive_bools)] // a CLI naturally has many on/off flags
pub struct Cli {
    /// Configuration file (default: %APPDATA%\PeetConnect\host.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Address to listen on. Default: all interfaces; non-tailnet peers are rejected anyway.
    #[arg(long)]
    bind: Option<IpAddr>,
    #[arg(long)]
    port: Option<u16>,
    /// Name shown to viewers. Default: this machine's hostname.
    #[arg(long)]
    name: Option<String>,
    /// Require this PIN in addition to tailnet identity.
    #[arg(long)]
    pin: Option<String>,
    /// Allow a tailnet user, e.g. alice@example.com. Repeatable.
    #[arg(long = "allow-user")]
    allow_user: Vec<String>,
    /// Allow nodes carrying this ACL tag, e.g. tag:admin. Repeatable.
    #[arg(long = "allow-tag")]
    allow_tag: Vec<String>,
    /// Allow a node by MagicDNS name or stable ID. Repeatable.
    #[arg(long = "allow-node")]
    allow_node: Vec<String>,
    /// Allow every node on the tailnet.
    #[arg(long)]
    allow_any_tailnet: bool,
    /// Allow loopback connections from this machine (development only).
    #[arg(long)]
    allow_local: bool,
    /// Capture rate cap.
    #[arg(long)]
    fps: Option<u32>,
    /// Bitrate cap in kbit/s; viewers may ask for less.
    #[arg(long)]
    bitrate_kbps: Option<u32>,
    /// Capture backend: auto, dxgi or gdi.
    #[arg(long)]
    capture: Option<String>,
    /// Run in the foreground attached to this console.
    #[arg(long)]
    console: bool,
    /// Install the Windows service (requires an elevated prompt). Other flags are baked in.
    #[arg(long)]
    install: bool,
    /// Remove the Windows service (requires an elevated prompt).
    #[arg(long)]
    uninstall: bool,
    /// Run as the Windows service. Only valid when launched by the Service Control Manager.
    #[arg(long)]
    service: bool,
    /// Internal: the per-session worker the service launches into the interactive desktop.
    #[arg(long, hide = true)]
    session_worker: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let (file, path) = config::load(cli.config.as_deref())?;
    if let Some(path) = &path {
        tracing::info!(path = %path.display(), "loaded configuration");
    }
    let config = config::HostConfig::resolve(file, &cli);
    #[cfg(windows)]
    {
        if cli.install {
            return service::install(&cli);
        }
        if cli.uninstall {
            return service::uninstall();
        }
        if cli.service {
            return service::run_service();
        }
        host::run(config)
    }
    #[cfg(not(windows))]
    {
        let _ = config;
        anyhow::bail!("peet-host currently supports Windows only")
    }
}

#[cfg(windows)]
mod auth;
#[cfg(windows)]
mod desktop;
#[cfg(windows)]
mod screenshot;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod session;
#[cfg(windows)]
mod stream;

#[cfg(windows)]
mod host {
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use peet_tailscale::LocalApi;
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };

    use crate::auth::Authoriser;
    use crate::config::HostConfig;
    use crate::session::{self, HostState};

    pub fn run(config: HostConfig) -> Result<()> {
        // Monitor bounds must be physical pixels, not scaled by our own DPI virtualisation.
        // SAFETY: plain Win32 call with a constant argument.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(serve(config))
    }

    async fn serve(config: HostConfig) -> Result<()> {
        if !config.admits_anyone() {
            tracing::warn!(
                "no allow rules configured: every connection will be rejected. \
                 Use --allow-user you@example.com, --allow-tag, --allow-node or a config file."
            );
        }
        match LocalApi::new().status().await {
            Ok(status) if status.is_running() => {
                tracing::info!(
                    node = %status.self_node.dns_name_trimmed(),
                    ips = ?status.tailscale_ips,
                    "tailscale is up"
                );
            }
            Ok(status) => {
                tracing::warn!(state = %status.backend_state, "tailscale is not connected; only --allow-local connections can succeed");
            }
            Err(err) => {
                tracing::warn!(%err, "tailscaled not reachable; only --allow-local connections can succeed");
            }
        }

        let endpoint = peet_transport::server_endpoint(config.bind)
            .with_context(|| format!("listening on {}", config.bind))?;
        tracing::info!(
            addr = %endpoint.local_addr()?, name = %config.name, fps = config.fps,
            max_kbps = config.max_bitrate_kbps, capture = %config.capture,
            pin = config.pin.is_some(), allow = ?config.allow, "peet-host listening"
        );

        let state = Arc::new(HostState {
            auth: Authoriser::new(config.allow.clone()),
            config,
            next_session: AtomicU64::new(0),
        });

        loop {
            tokio::select! {
                incoming = endpoint.accept() => {
                    let Some(incoming) = incoming else { break };
                    let remote = incoming.remote_address();
                    let state = state.clone();
                    tokio::spawn(async move {
                        match incoming.await {
                            Ok(connection) => session::run(state, connection).await,
                            Err(err) => tracing::debug!(%remote, %err, "connection failed before handshake"),
                        }
                    });
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down");
                    break;
                }
            }
        }
        endpoint.close(0u32.into(), b"host shutting down");
        endpoint.wait_idle().await;
        Ok(())
    }
}
