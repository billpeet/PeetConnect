//! `peet`: the PeetConnect command line.
//!
//! Milestone 0 exposes Tailscale discovery and identity. Session commands (connect,
//! screenshot, input) arrive with the client core in later milestones.

use std::net::IpAddr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
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
    /// Print protocol constants for this build.
    Version,
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
