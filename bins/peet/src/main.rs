//! `peet`: the PeetConnect command line.
//!
//! Tailscale discovery and identity, plus headless session commands built on `peet-core`:
//! list a host's monitors and take screenshots without a GUI. Input and the full agent
//! surface arrive in milestone 5.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use peet_core::{ConnectOptions, Session};
use peet_proto::{Codec, InputEvent, MouseButton};
use peet_tailscale::{LocalApi, PeerStatus, Status};

#[derive(Debug, Parser)]
#[command(name = "peet", version, about = "PeetConnect remote desktop CLI")]
struct Cli {
    /// Emit machine-readable JSON instead of a table.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List devices on the tailnet.
    Peers {
        /// Include devices that are currently offline.
        #[arg(long)]
        all: bool,
    },
    /// Show this device's Tailscale identity.
    Whoami,
    /// Resolve a tailnet address to its node and user.
    Whois {
        /// Tailscale IP of the peer, e.g. 100.101.102.103.
        addr: IpAddr,
    },
    /// List the monitors of a PeetConnect host.
    Monitors {
        /// Host name, MagicDNS name or IP, with optional :port.
        host: String,
        #[arg(long)]
        pin: Option<String>,
    },
    /// Capture a monitor of a PeetConnect host to a PNG file.
    Screenshot {
        /// Host name, MagicDNS name or IP, with optional :port.
        host: String,
        /// Monitor ID (see `monitors`); default is the primary.
        #[arg(long)]
        monitor: Option<u32>,
        /// Downscale so the image is at most this wide.
        #[arg(long)]
        max_width: Option<u32>,
        /// Output file; `-` writes PNG bytes to stdout.
        #[arg(short, long, default_value = "screenshot.png")]
        output: PathBuf,
        #[arg(long)]
        pin: Option<String>,
    },
    /// Move the pointer. X and Y are monitor-relative pixels unless --absolute is given.
    Move {
        /// Host name, MagicDNS name or IP, with optional :port.
        host: String,
        x: i32,
        y: i32,
        /// Monitor ID the coordinates are relative to; default is the primary.
        #[arg(long)]
        monitor: Option<u32>,
        /// Treat X and Y as host virtual-desktop coordinates instead.
        #[arg(long)]
        absolute: bool,
        #[arg(long)]
        pin: Option<String>,
    },
    /// Click a mouse button, optionally moving to X Y first.
    Click {
        /// Host name, MagicDNS name or IP, with optional :port.
        host: String,
        /// Monitor-relative X (requires Y). Omit to click at the current position.
        x: Option<i32>,
        /// Monitor-relative Y.
        y: Option<i32>,
        #[arg(long, value_enum, default_value_t = CliButton::Left)]
        button: CliButton,
        /// Send two clicks in quick succession.
        #[arg(long)]
        double: bool,
        #[arg(long)]
        monitor: Option<u32>,
        #[arg(long)]
        absolute: bool,
        #[arg(long)]
        pin: Option<String>,
    },
    /// Scroll the wheel. Positive dy scrolls up, positive dx scrolls right (notches).
    Scroll {
        /// Host name, MagicDNS name or IP, with optional :port.
        host: String,
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        dy: i32,
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        dx: i32,
        #[arg(long)]
        pin: Option<String>,
    },
    /// Type Unicode text on the host.
    Type {
        /// Host name, MagicDNS name or IP, with optional :port.
        host: String,
        /// Text to type.
        text: String,
        #[arg(long)]
        pin: Option<String>,
    },
    /// Press a key chord such as `enter`, `ctrl+c` or `ctrl+alt+delete`.
    Key {
        /// Host name, MagicDNS name or IP, with optional :port.
        host: String,
        /// Key chord: names joined with `+` (see docs for the key list).
        chord: String,
        #[arg(long)]
        pin: Option<String>,
    },
    /// Print protocol constants for this build.
    Version,
}

/// Mouse button names accepted on the command line.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl From<CliButton> for MouseButton {
    fn from(b: CliButton) -> Self {
        match b {
            CliButton::Left => MouseButton::Left,
            CliButton::Right => MouseButton::Right,
            CliButton::Middle => MouseButton::Middle,
            CliButton::Back => MouseButton::Back,
            CliButton::Forward => MouseButton::Forward,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let api = LocalApi::new();

    match cli.command {
        Command::Peers { all } => {
            let status = fetch_status(&api).await?;
            let peers: Vec<&PeerStatus> = status
                .peers_sorted()
                .into_iter()
                .filter(|p| all || p.online)
                .collect();
            if cli.json {
                print_json(&peers_json(&status, &peers))?;
            } else {
                print_peer_table(&status, &peers);
            }
        }
        Command::Whoami => {
            let status = fetch_status(&api).await?;
            let me = &status.self_node;
            if cli.json {
                print_json(&serde_json::json!({
                    "id": me.id,
                    "host_name": me.host_name,
                    "dns_name": me.dns_name_trimmed(),
                    "os": me.os,
                    "ips": me.tailscale_ips,
                    "user": status.user.get(&me.user_id.to_string()).map(|u| &u.login_name),
                    "tailnet": status.current_tailnet.as_ref().map(|t| &t.name),
                }))?;
            } else {
                println!("{} ({})", me.host_name, me.dns_name_trimmed());
                println!("  id    {}", me.id);
                println!("  os    {}", me.os);
                println!("  ips   {}", me.tailscale_ips.join(", "));
                if let Some(user) = status.user.get(&me.user_id.to_string()) {
                    println!("  user  {} ({})", user.login_name, user.display_name);
                }
                if let Some(tailnet) = &status.current_tailnet {
                    println!("  tailnet {}", tailnet.name);
                }
            }
        }
        Command::Whois { addr } => {
            let who = api
                .whois(addr)
                .await
                .with_context(|| format!("whois lookup for {addr} failed"))?;
            if cli.json {
                print_json(&serde_json::json!({
                    "node_id": who.node.stable_id,
                    "name": who.node.name.trim_end_matches('.'),
                    "tags": who.node.tags,
                    "user": who.user_profile.login_name,
                    "display_name": who.user_profile.display_name,
                }))?;
            } else {
                println!("{}", who.node.name.trim_end_matches('.'));
                println!("  node   {}", who.node.stable_id);
                if who.is_tagged() {
                    println!("  tags   {}", who.node.tags.unwrap_or_default().join(", "));
                } else {
                    println!(
                        "  user   {} ({})",
                        who.user_profile.login_name, who.user_profile.display_name
                    );
                }
            }
        }
        Command::Monitors { host, pin } => {
            let session = open_session(&host, pin).await?;
            let info = session.info();
            if cli.json {
                print_json(&serde_json::json!({
                    "host_name": info.host_name,
                    "session_id": info.session_id.0,
                    "codec": format!("{:?}", info.codec),
                    "monitors": info.monitors.iter().map(|m| serde_json::json!({
                        "id": m.id.0, "name": m.name, "x": m.bounds.x, "y": m.bounds.y,
                        "width": m.bounds.width, "height": m.bounds.height,
                        "scale_percent": m.scale_percent, "primary": m.primary,
                    })).collect::<Vec<_>>(),
                }))?;
            } else {
                println!("{} ({} monitors)", info.host_name, info.monitors.len());
                for m in &info.monitors {
                    println!(
                        "  {:>4}  {:<14} {}x{} at {},{}  {}%{}",
                        m.id.0,
                        m.name,
                        m.bounds.width,
                        m.bounds.height,
                        m.bounds.x,
                        m.bounds.y,
                        m.scale_percent,
                        if m.primary { "  primary" } else { "" }
                    );
                }
            }
            session.close("done");
        }
        Command::Screenshot {
            host,
            monitor,
            max_width,
            output,
            pin,
        } => {
            let session = open_session(&host, pin).await?;
            let info = session.info();
            let target = match monitor {
                Some(id) => info
                    .monitors
                    .iter()
                    .find(|m| m.id.0 == id)
                    .with_context(|| format!("host has no monitor {id}"))?,
                None => info
                    .monitors
                    .iter()
                    .find(|m| m.primary)
                    .or(info.monitors.first())
                    .context("host reports no monitors")?,
            };
            let image = session
                .control()
                .screenshot(target.id, Codec::Png, max_width, Duration::from_secs(15))
                .await?;
            session.close("done");
            if output.as_os_str() == "-" {
                use std::io::Write as _;
                std::io::stdout().write_all(&image.data)?;
            } else {
                std::fs::write(&output, &image.data)
                    .with_context(|| format!("writing {}", output.display()))?;
                if cli.json {
                    print_json(&serde_json::json!({
                        "path": output, "width": image.width, "height": image.height,
                        "bytes": image.data.len(), "monitor": target.id.0,
                    }))?;
                } else {
                    eprintln!(
                        "{}x{} PNG, {} bytes -> {}",
                        image.width,
                        image.height,
                        image.data.len(),
                        output.display()
                    );
                }
            }
        }
        Command::Move {
            host,
            x,
            y,
            monitor,
            absolute,
            pin,
        } => {
            let session = open_session(&host, pin).await?;
            let (vx, vy) = virtual_point(session.info(), monitor, x, y, absolute)?;
            session
                .control()
                .send_input(InputEvent::MouseMove { x: vx, y: vy });
            finish_input(&session).await;
            session.close("done");
            if cli.json {
                print_json(&serde_json::json!({ "x": vx, "y": vy }))?;
            } else {
                eprintln!("moved to {vx},{vy}");
            }
        }
        Command::Click {
            host,
            x,
            y,
            button,
            double,
            monitor,
            absolute,
            pin,
        } => {
            let session = open_session(&host, pin).await?;
            let control = session.control();
            match (x, y) {
                (Some(x), Some(y)) => {
                    let (vx, vy) = virtual_point(session.info(), monitor, x, y, absolute)?;
                    control.send_input(InputEvent::MouseMove { x: vx, y: vy });
                }
                (None, None) => {}
                _ => anyhow::bail!("click needs both X and Y, or neither"),
            }
            let button = MouseButton::from(button);
            let clicks = if double { 2 } else { 1 };
            for _ in 0..clicks {
                control.send_input(InputEvent::MouseButton {
                    button,
                    pressed: true,
                });
                control.send_input(InputEvent::MouseButton {
                    button,
                    pressed: false,
                });
            }
            finish_input(&session).await;
            session.close("done");
            if !cli.json {
                eprintln!("clicked {button:?}");
            }
        }
        Command::Scroll { host, dy, dx, pin } => {
            let session = open_session(&host, pin).await?;
            // Protocol deltas are 1/120 units; one notch is 120.
            session.control().send_input(InputEvent::MouseWheel {
                delta_x: dx * 120,
                delta_y: dy * 120,
            });
            finish_input(&session).await;
            session.close("done");
            if !cli.json {
                eprintln!("scrolled dx={dx} dy={dy}");
            }
        }
        Command::Type { host, text, pin } => {
            let session = open_session(&host, pin).await?;
            session
                .control()
                .send_input(InputEvent::Text { text: text.clone() });
            finish_input(&session).await;
            session.close("done");
            if cli.json {
                print_json(&serde_json::json!({ "typed": text }))?;
            } else {
                eprintln!("typed {} characters", text.chars().count());
            }
        }
        Command::Key { host, chord, pin } => {
            let names = peet_input::keys::parse_chord(&chord);
            if names.is_empty() {
                anyhow::bail!("empty key chord");
            }
            let mut keys = Vec::with_capacity(names.len());
            for name in &names {
                let key = peet_input::keys::lookup(name)
                    .with_context(|| format!("unknown key `{name}`"))?;
                keys.push(key);
            }
            let session = open_session(&host, pin).await?;
            let control = session.control();
            // Press in order, release in reverse, so modifiers wrap the final key.
            for key in &keys {
                control.send_input(InputEvent::Key {
                    scancode: key.scancode,
                    extended: key.extended,
                    pressed: true,
                });
            }
            for key in keys.iter().rev() {
                control.send_input(InputEvent::Key {
                    scancode: key.scancode,
                    extended: key.extended,
                    pressed: false,
                });
            }
            finish_input(&session).await;
            session.close("done");
            if !cli.json {
                eprintln!("sent {chord}");
            }
        }
        Command::Version => {
            if cli.json {
                print_json(&serde_json::json!({
                    "peet": env!("CARGO_PKG_VERSION"),
                    "protocol_version": peet_proto::PROTOCOL_VERSION,
                    "default_port": peet_proto::DEFAULT_PORT,
                    "alpn": String::from_utf8_lossy(peet_proto::ALPN),
                }))?;
            } else {
                println!("peet {}", env!("CARGO_PKG_VERSION"));
                println!("protocol v{}", peet_proto::PROTOCOL_VERSION);
                println!("default port {}", peet_proto::DEFAULT_PORT);
            }
        }
    }
    Ok(())
}

/// Flushes queued input and gives the host a moment to receive and inject it before the caller
/// closes the connection.
async fn finish_input(session: &Session) {
    session.control().flush_input().await;
    tokio::time::sleep(Duration::from_millis(250)).await;
}

/// Maps a monitor-relative (or absolute) point to host virtual-desktop coordinates.
fn virtual_point(
    info: &peet_core::SessionInfo,
    monitor: Option<u32>,
    x: i32,
    y: i32,
    absolute: bool,
) -> Result<(i32, i32)> {
    if absolute {
        return Ok((x, y));
    }
    let target = match monitor {
        Some(id) => info
            .monitors
            .iter()
            .find(|m| m.id.0 == id)
            .with_context(|| format!("host has no monitor {id}"))?,
        None => info
            .monitors
            .iter()
            .find(|m| m.primary)
            .or(info.monitors.first())
            .context("host reports no monitors")?,
    };
    Ok((target.bounds.x + x, target.bounds.y + y))
}

async fn open_session(host: &str, pin: Option<String>) -> Result<Session> {
    let addr = peet_core::resolve(host).await?;
    let opts = ConnectOptions {
        client_name: format!(
            "peet CLI on {}",
            gethostname::gethostname().to_string_lossy()
        ),
        pin,
        ..ConnectOptions::default()
    };
    peet_core::connect(addr, opts)
        .await
        .with_context(|| format!("connecting to {host} ({addr})"))
}

async fn fetch_status(api: &LocalApi) -> Result<Status> {
    let status = api
        .status()
        .await
        .context("is Tailscale installed and running?")?;
    if !status.is_running() {
        anyhow::bail!(
            "Tailscale is not connected (state: {}). Run `tailscale up` first.",
            status.backend_state
        );
    }
    Ok(status)
}

fn print_peer_table(status: &Status, peers: &[&PeerStatus]) {
    if peers.is_empty() {
        println!("no peers (use --all to include offline devices)");
        return;
    }
    let name_width = peers
        .iter()
        .map(|p| p.host_name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let ip_width = peers
        .iter()
        .filter_map(|p| p.primary_ip().map(str::len))
        .max()
        .unwrap_or(2)
        .max(2);
    println!(
        "{:<name_width$}  {:<ip_width$}  OS        STATE    OWNER",
        "NAME", "IP"
    );
    for peer in peers {
        let owner = peer.tags.as_ref().filter(|t| !t.is_empty()).map_or_else(
            || {
                status
                    .user
                    .get(&peer.user_id.to_string())
                    .map_or_else(String::new, |u| u.login_name.clone())
            },
            |tags| tags.join(","),
        );
        println!(
            "{:<name_width$}  {:<ip_width$}  {:<8}  {:<7}  {}",
            peer.host_name,
            peer.primary_ip().unwrap_or(""),
            peer.os,
            if peer.online { "online" } else { "offline" },
            owner
        );
    }
}

fn peers_json(status: &Status, peers: &[&PeerStatus]) -> serde_json::Value {
    serde_json::Value::Array(
        peers
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "host_name": p.host_name,
                    "dns_name": p.dns_name_trimmed(),
                    "os": p.os,
                    "ips": p.tailscale_ips,
                    "online": p.online,
                    "tags": p.tags,
                    "user": status.user.get(&p.user_id.to_string()).map(|u| &u.login_name),
                    "last_seen": p.last_seen,
                })
            })
            .collect(),
    )
}

fn print_json(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
