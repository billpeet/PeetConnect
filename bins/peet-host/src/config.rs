//! Host configuration: a TOML file with command-line overrides.
//!
//! The file lives at `%APPDATA%\PeetConnect\host.toml` (or `~/.config/PeetConnect/host.toml`)
//! for a console host and at `%ProgramData%\PeetConnect\host.toml` for the service and its
//! worker, unless `--config` says otherwise. Every key is optional. Example:
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
//!
//! [update]
//! automatic = true    # the service installs updates by itself (per-machine installs)
//! ```

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<IpAddr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    #[serde(skip_serializing_if = "is_default")]
    pub allow: AllowConfig,
    #[serde(skip_serializing_if = "is_default")]
    pub video: VideoConfig,
    #[serde(skip_serializing_if = "is_default")]
    pub update: UpdateConfig,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AllowConfig {
    /// Tailscale login names, e.g. `alice@example.com`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<String>,
    /// ACL tags, e.g. `tag:admin`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Node stable IDs or MagicDNS host names.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<String>,
    /// Accept every node on the tailnet.
    #[serde(skip_serializing_if = "is_default")]
    pub any_tailnet: bool,
    /// Accept connections from this machine (loopback). For development only.
    #[serde(skip_serializing_if = "is_default")]
    pub local: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VideoConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bitrate_kbps: Option<u32>,
    /// `auto`, `dxgi` or `gdi`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture: Option<String>,
}

/// Updates of a per-machine install, which the service applies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    /// Install a downloaded update by itself once nothing is in use (the default). When false
    /// it waits for `peet update` or the tray's "Install now".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automatic: Option<bool>,
}

impl FileConfig {
    /// The settings given as command-line flags, for writing into a new file.
    pub fn from_cli(cli: &crate::Cli) -> Self {
        Self {
            bind: cli.bind,
            port: cli.port,
            name: cli.name.clone(),
            pin: cli.pin.clone(),
            allow: AllowConfig {
                users: cli.allow_user.clone(),
                tags: cli.allow_tag.clone(),
                nodes: cli.allow_node.clone(),
                any_tailnet: cli.allow_any_tailnet,
                local: cli.allow_local,
            },
            video: VideoConfig {
                fps: cli.fps,
                max_bitrate_kbps: cli.bitrate_kbps,
                capture: cli.capture.clone(),
            },
            update: UpdateConfig::default(),
        }
    }

    /// A new configuration file: a header explaining the keys, then these settings.
    pub fn to_file_text(&self) -> Result<String> {
        let body = toml::to_string(self).context("formatting the configuration")?;
        Ok(format!("{TEMPLATE_HEADER}\n{body}"))
    }
}

const TEMPLATE_HEADER: &str = "\
# PeetConnect host configuration. Restart the host after editing.
#
# port = 7735
# name = \"studio-pc\"
# pin = \"4821\"            # required in addition to tailnet identity
#
# [allow]                 # who may connect; nobody is admitted without a rule
# users = [\"alice@example.com\"]
# tags = [\"tag:admin\"]
# nodes = [\"laptop\"]
# any_tailnet = false
#
# [video]
# fps = 60
# max_bitrate_kbps = 12000
# capture = \"auto\"        # auto | dxgi | gdi
#
# [update]                # per-machine installs, updated by the service
# automatic = true        # false: download, but wait for `peet update` to install
";

/// The default file: per-user for a console host, machine-wide for the service and its worker.
pub fn default_path(machine: bool) -> Option<PathBuf> {
    #[cfg(windows)]
    if machine {
        return Some(crate::machine::config_path());
    }
    let _ = machine;
    dirs::config_dir().map(|d| d.join("PeetConnect").join("host.toml"))
}

/// Loads the file at `path`, or the default location. A missing default file is not an error.
pub fn load(path: Option<&Path>, machine: bool) -> Result<(FileConfig, Option<PathBuf>)> {
    let (path, required) = match path {
        Some(p) => (Some(p.to_path_buf()), true),
        None => (default_path(machine), false),
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
    /// The service installs downloaded updates by itself.
    pub auto_update: bool,
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
            auto_update: file.update.automatic.unwrap_or(true),
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
    fn written_file_reads_back() {
        let config = FileConfig {
            pin: Some("4821".into()),
            allow: AllowConfig {
                users: vec!["a@b.c".into()],
                ..AllowConfig::default()
            },
            ..FileConfig::default()
        };
        let text = config.to_file_text().unwrap();
        assert!(
            !text.contains("\n[video]"),
            "empty tables are left out:\n{text}"
        );
        assert_eq!(toml::from_str::<FileConfig>(&text).unwrap(), config);
        assert_eq!(
            toml::from_str::<FileConfig>(&FileConfig::default().to_file_text().unwrap()).unwrap(),
            FileConfig::default()
        );
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<FileConfig>("prot = 1").is_err());
    }
}
