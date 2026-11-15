//! Home screen data: the viewer's saved settings (favourites, recents, defaults) and a
//! background discovery thread that keeps the tailnet device list fresh.
//!
//! The settings file lives at `%APPDATA%\PeetConnect\viewer.toml`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX_RECENTS: usize = 8;
const REFRESH: Duration = Duration::from_secs(3);

/// A host the user connects to, as shown on the home screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// What we connect to: a Tailscale IP or MagicDNS name, optionally with `:port`.
    pub target: String,
    /// Friendly name for display.
    pub name: String,
    /// Unix seconds of the last connection (recents only).
    #[serde(default)]
    pub when: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewerConfig {
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// While one host window is shown, keep the viewer and that window the same size: the
    /// viewer follows the window's size 1:1, and resizing the viewer resizes the window.
    pub match_window_size: bool,
    pub favourites: Vec<Saved>,
    pub recents: Vec<Saved>,
    /// Names the user gave hosts, by target. They win over discovered and saved names.
    pub names: BTreeMap<String, String>,
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            fps: 60,
            bitrate_kbps: 12000,
            match_window_size: true,
            favourites: Vec::new(),
            recents: Vec::new(),
            names: BTreeMap::new(),
        }
    }
}

impl ViewerConfig {
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("PeetConnect").join("viewer.toml"))
    }

    /// Loads the file, or defaults when it is missing or unreadable (logged, never fatal).
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
                tracing::warn!(path = %path.display(), %err, "bad viewer config; using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        let Ok(text) = toml::to_string_pretty(self) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(err) = std::fs::write(&path, text) {
            tracing::warn!(path = %path.display(), %err, "could not save viewer config");
        }
    }

    pub fn is_favourite(&self, target: &str) -> bool {
        self.favourites.iter().any(|f| f.target == target)
    }

    pub fn toggle_favourite(&mut self, target: &str, name: &str) {
        if let Some(i) = self.favourites.iter().position(|f| f.target == target) {
            self.favourites.remove(i);
        } else {
            self.favourites.push(Saved {
                target: target.to_string(),
                name: name.to_string(),
                when: 0,
            });
        }
        self.save();
    }

    /// What to call `target`: the user's name for it, else `fallback`.
    pub fn display_name(&self, target: &str, fallback: &str) -> String {
        self.names
            .get(target)
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    }

    /// Gives `target` a name of the user's choosing; an empty (or unchanged) name clears it.
    pub fn rename(&mut self, target: &str, name: &str, original: &str) {
        let name = name.trim();
        if name.is_empty() || name == original {
            self.names.remove(target);
        } else {
            self.names.insert(target.to_string(), name.to_string());
        }
        self.save();
    }

    /// Drops `target` from favourites and recents and forgets its name.
    pub fn forget(&mut self, target: &str) {
        self.favourites.retain(|f| f.target != target);
        self.recents.retain(|r| r.target != target);
        self.names.remove(target);
        self.save();
    }

    /// When `target` was last connected to (Unix seconds), or 0.
    pub fn last_connected(&self, target: &str) -> u64 {
        self.recents
            .iter()
            .find(|r| r.target == target)
            .map_or(0, |r| r.when)
    }

    /// Hosts for the home screen's cards: favourites in order, then recents not already
    /// among them.
    pub fn computers(&self) -> Vec<Saved> {
        let mut out = self.favourites.clone();
        for r in &self.recents {
            if !out.iter().any(|s| s.target == r.target) {
                out.push(r.clone());
            }
        }
        out
    }

    /// Moves (or adds) `target` to the top of the recents list.
    pub fn record_recent(&mut self, target: &str, name: &str) {
        self.recents.retain(|r| r.target != target);
        let when = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.recents.insert(
            0,
            Saved {
                target: target.to_string(),
                name: name.to_string(),
                when,
            },
        );
        self.recents.truncate(MAX_RECENTS);
        self.save();
    }
}

/// One tailnet device, as discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub dns_name: String,
    /// Primary Tailscale IP; what we connect to.
    pub target: String,
    pub os: String,
    pub online: bool,
}

impl Peer {
    /// Whether `target` (an IP, MagicDNS name or host name, optionally with `:port`) is this
    /// device.
    pub fn matches(&self, target: &str) -> bool {
        let t = target.trim();
        let host = match t.rsplit_once(':') {
            // An IPv6 address has more colons; only strip a port after a name or IPv4.
            Some((h, port)) if !h.contains(':') && port.parse::<u16>().is_ok() => h,
            _ => t,
        };
        self.target == host
            || self.dns_name.eq_ignore_ascii_case(host)
            || self.name.eq_ignore_ascii_case(host)
    }
}

/// What the discovery thread publishes.
#[derive(Debug, Default)]
pub struct Discovery {
    pub peers: Vec<Peer>,
    /// This device, so a saved host that is this machine still shows as online.
    pub this_device: Option<Peer>,
    pub self_name: String,
    pub error: Option<String>,
    pub ever_loaded: bool,
}

/// Starts a thread that refreshes the device list every few seconds. It stops on its own when
/// the returned handle is the last one (the UI dropped it).
pub fn start_discovery() -> Arc<Mutex<Discovery>> {
    let shared = Arc::new(Mutex::new(Discovery::default()));
    let weak = Arc::downgrade(&shared);
    let spawned = std::thread::Builder::new()
        .name("discovery".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    if let Some(shared) = weak.upgrade() {
                        shared.lock().expect("discovery poisoned").error = Some(err.to_string());
                    }
                    return;
                }
            };
            let api = peet_tailscale::LocalApi::new();
            while let Some(shared) = weak.upgrade() {
                let result = runtime.block_on(api.status());
                {
                    let mut d = shared.lock().expect("discovery poisoned");
                    d.ever_loaded = true;
                    match result {
                        Ok(status) if status.is_running() => {
                            d.self_name.clone_from(&status.self_node.host_name);
                            let me = &status.self_node;
                            d.this_device = me.primary_ip().map(|ip| Peer {
                                name: me.host_name.clone(),
                                dns_name: me.dns_name_trimmed().to_string(),
                                target: ip.to_string(),
                                os: me.os.clone(),
                                online: true,
                            });
                            d.peers = status
                                .peers_sorted()
                                .into_iter()
                                .filter_map(|p| {
                                    Some(Peer {
                                        name: p.host_name.clone(),
                                        dns_name: p.dns_name_trimmed().to_string(),
                                        target: p.primary_ip()?.to_string(),
                                        os: p.os.clone(),
                                        online: p.online,
                                    })
                                })
                                .collect();
                            d.error = None;
                        }
                        Ok(status) => {
                            d.error = Some(format!(
                                "Tailscale is not connected ({})",
                                status.backend_state
                            ));
                        }
                        Err(err) => {
                            d.error = Some(format!("Tailscale unavailable: {err}"));
                        }
                    }
                }
                drop(shared);
                std::thread::sleep(REFRESH);
            }
        });
    if let Err(err) = spawned {
        shared.lock().expect("discovery poisoned").error = Some(err.to_string());
    }
    shared
}

#[cfg(test)]
mod tests {
    use super::{Peer, Saved, ViewerConfig};

    fn saved(target: &str) -> Saved {
        Saved {
            target: target.into(),
            name: target.into(),
            when: 0,
        }
    }

    #[test]
    fn computers_lists_favourites_then_new_recents() {
        let cfg = ViewerConfig {
            favourites: vec![saved("a"), saved("b")],
            recents: vec![saved("c"), saved("a"), saved("d")],
            ..ViewerConfig::default()
        };
        let order: Vec<_> = cfg.computers().into_iter().map(|s| s.target).collect();
        assert_eq!(order, ["a", "b", "c", "d"]);
    }

    #[test]
    fn names_override_and_clear() {
        let mut cfg = ViewerConfig::default();
        cfg.names.insert("1.2.3.4".into(), "Study PC".into());
        assert_eq!(cfg.display_name("1.2.3.4", "DESKTOP"), "Study PC");
        assert_eq!(cfg.display_name("5.6.7.8", "OTHER"), "OTHER");
        // Clearing goes through `names` directly here; `rename` would also save to disk.
        cfg.names.remove("1.2.3.4");
        assert_eq!(cfg.display_name("1.2.3.4", "DESKTOP"), "DESKTOP");
    }

    #[test]
    fn names_round_trip_through_toml() {
        let mut cfg = ViewerConfig::default();
        cfg.favourites.push(saved("100.101.102.103"));
        cfg.names
            .insert("100.101.102.103".into(), "Study PC".into());
        let text = toml::to_string_pretty(&cfg).expect("serialise");
        let back: ViewerConfig = toml::from_str(&text).expect("parse");
        assert_eq!(
            back.names.get("100.101.102.103").map(String::as_str),
            Some("Study PC")
        );
    }

    #[test]
    fn peer_matching() {
        let p = Peer {
            name: "STUDY-PC".into(),
            dns_name: "study-pc.example.ts.net".into(),
            target: "100.101.102.103".into(),
            os: "windows".into(),
            online: true,
        };
        assert!(p.matches("100.101.102.103"));
        assert!(p.matches("100.101.102.103:7360"));
        assert!(p.matches("Study-PC.example.ts.net"));
        assert!(p.matches("study-pc"));
        assert!(!p.matches("100.101.102.10"));
    }
}
