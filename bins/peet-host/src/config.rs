//! Host configuration: a TOML file with command-line overrides.
//!
//! The file lives at `%APPDATA%\PeetConnect\host.toml` (or `~/.config/PeetConnect/host.toml`)
//! unless `--config` says otherwise. Every key is optional. Example:
//!
//! ```toml
//! port = 7735
//! name = "studio-pc"
//! pin = "4821"
//!
//! [allow]
//! users = ["alice@example.com"]
//! tags = ["tag:admin"]
//! nodes = ["laptop"]
//!
//! [video]
//! fps = 60
//! max_bitrate_kbps = 12000
//! capture = "auto"   # auto | dxgi | gdi
//! ```

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub bind: Option<IpAddr>,
    pub port: Option<u16>,
    pub name: Option<String>,
    pub pin: Option<String>,
    pub allow: AllowConfig,
    pub video: VideoConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AllowConfig {
    /// Tailscale login names, e.g. `alice@example.com`.
    pub users: Vec<String>,
    /// ACL tags, e.g. `tag:admin`.
    pub tags: Vec<String>,
    /// Node stable IDs or MagicDNS host names.
    pub nodes: Vec<String>,
    /// Accept every node on the tailnet.
    pub any_tailnet: bool,
    /// Accept connections from this machine (loopback). For development only.
    pub local: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VideoConfig {
    pub fps: Option<u32>,
    pub max_bitrate_kbps: Option<u32>,
    /// `auto`, `dxgi` or `gdi`.
    pub capture: Option<String>,
}

pub fn default_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("PeetConnect").join("host.toml"))
}

/// Loads the file at `path`, or the default location. A missing default file is not an error.
pub fn load(path: Option<&Path>) -> Result<(FileConfig, Option<PathBuf>)> {
    let (path, required) = match path {
        Some(p) => (Some(p.to_path_buf()), true),
        None => (default_path(), false),
    };
    let Some(path) = path else {
        return Ok((FileConfig::default(), None));
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let config: FileConfig =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            Ok((config, Some(path)))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !required => {
            Ok((FileConfig::default(), None))
        }
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Fully resolved settings the host runs with.
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub bind: SocketAddr,
    pub name: String,
    pub pin: Option<String>,
    pub allow: AllowConfig,
    pub fps: u32,
    pub max_bitrate_kbps: u32,
    pub capture: String,
    /// True when running as the service worker: capture and input threads follow the active
    /// input desktop so they keep working across the secure desktop.
    pub follow_desktop: bool,
}

impl HostConfig {
    pub fn resolve(file: FileConfig, cli: &crate::Cli) -> Self {
        let bind_ip = cli
            .bind
            .or(file.bind)
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let port = cli.port.or(file.port).unwrap_or(peet_proto::DEFAULT_PORT);
        let mut allow = file.allow;
        allow.users.extend(cli.allow_user.iter().cloned());
        allow.tags.extend(cli.allow_tag.iter().cloned());
        allow.nodes.extend(cli.allow_node.iter().cloned());
        allow.any_tailnet |= cli.allow_any_tailnet;
        allow.local |= cli.allow_local;
        Self {
            bind: SocketAddr::new(bind_ip, port),
            name: cli
                .name
                .clone()
                .or(file.name)
                .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned()),
            pin: cli.pin.clone().or(file.pin).filter(|p| !p.is_empty()),
            allow,
            fps: cli.fps.or(file.video.fps).unwrap_or(60).clamp(1, 240),
            max_bitrate_kbps: cli
                .bitrate_kbps
                .or(file.video.max_bitrate_kbps)
                .unwrap_or(12_000)
                .max(200),
            capture: cli
                .capture
                .clone()
                .or(file.video.capture)
                .unwrap_or_else(|| "auto".into()),
            follow_desktop: cli.session_worker,
        }
    }

    /// True when at least one rule can admit somebody.
    pub fn admits_anyone(&self) -> bool {
        let a = &self.allow;
        a.any_tailnet || a.local || !a.users.is_empty() || !a.tags.is_empty() || !a.nodes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example() {
        let text = r#"
            port = 8000
            [allow]
            users = ["a@b.c"]
            [video]
            fps = 30
        "#;
        let config: FileConfig = toml::from_str(text).unwrap();
        assert_eq!(config.port, Some(8000));
        assert_eq!(config.allow.users, vec!["a@b.c".to_string()]);
        assert_eq!(config.video.fps, Some(30));
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<FileConfig>("prot = 1").is_err());
    }
}
