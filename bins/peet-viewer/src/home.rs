//! Home screen data: the viewer's saved settings (favourites, recents, defaults) and a
//! background discovery thread that keeps the tailnet device list fresh.
//!
//! The settings file lives at `%APPDATA%\PeetConnect\viewer.toml`.

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
    pub favourites: Vec<Saved>,
    pub recents: Vec<Saved>,
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            fps: 60,
            bitrate_kbps: 12000,
            favourites: Vec::new(),
            recents: Vec::new(),
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

/// What the discovery thread publishes.
#[derive(Debug, Default)]
pub struct Discovery {
    pub peers: Vec<Peer>,
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
