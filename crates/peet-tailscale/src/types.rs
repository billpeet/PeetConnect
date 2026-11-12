//! Subset of the tailscaled LocalAPI JSON responses that PeetConnect uses.
//!
//! Field names follow the Go structs in `ipnstate` and `tailcfg`. Everything is `default` so a
//! newer or older tailscaled that omits a field does not break parsing.

use std::collections::HashMap;

use serde::Deserialize;

/// Response from `GET /localapi/v0/status`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Status {
    pub version: String,
    /// `Running`, `Stopped`, `NeedsLogin`, `NeedsMachineAuth`, `Starting`, `NoState`.
    pub backend_state: String,
    #[serde(rename = "TailscaleIPs")]
    pub tailscale_ips: Vec<String>,
    #[serde(rename = "Self")]
    pub self_node: PeerStatus,
    /// Keyed by node public key.
    pub peer: HashMap<String, PeerStatus>,
    /// Keyed by user ID (as a string).
    pub user: HashMap<String, UserProfile>,
    #[serde(rename = "MagicDNSSuffix")]
    pub magic_dns_suffix: String,
    pub current_tailnet: Option<TailnetStatus>,
}

impl Status {
    /// True when tailscaled is up and this node is logged in.
    pub fn is_running(&self) -> bool {
        self.backend_state == "Running"
    }

    /// Peers sorted by hostname for stable display.
    pub fn peers_sorted(&self) -> Vec<&PeerStatus> {
        let mut peers: Vec<&PeerStatus> = self.peer.values().collect();
        peers.sort_by_key(|p| p.host_name.to_lowercase());
        peers
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct TailnetStatus {
    pub name: String,
    #[serde(rename = "MagicDNSSuffix")]
    pub magic_dns_suffix: String,
    #[serde(rename = "MagicDNSEnabled")]
    pub magic_dns_enabled: bool,
}

/// One node as reported by `status`. Used for both `Self` and each `Peer`.
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // mirrors the upstream Go struct
#[serde(default, rename_all = "PascalCase")]
pub struct PeerStatus {
    /// Stable node ID, e.g. `n4E3gQDoJJ11CNTRL`.
    #[serde(rename = "ID")]
    pub id: String,
    pub public_key: String,
    pub host_name: String,
    /// Fully qualified MagicDNS name with trailing dot.
    #[serde(rename = "DNSName")]
    pub dns_name: String,
    #[serde(rename = "OS")]
    pub os: String,
    #[serde(rename = "UserID")]
    pub user_id: u64,
    #[serde(rename = "TailscaleIPs")]
    pub tailscale_ips: Vec<String>,
    pub tags: Option<Vec<String>>,
    pub online: bool,
    pub active: bool,
    pub exit_node: bool,
    pub exit_node_option: bool,
    /// RFC 3339 timestamp of last activity.
    pub last_seen: Option<String>,
    /// Current direct endpoint, empty when relayed or idle.
    pub cur_addr: String,
    /// DERP relay region code when relayed.
    pub relay: String,
}

impl PeerStatus {
    /// First IPv4 Tailscale address, falling back to the first address of any family.
    pub fn primary_ip(&self) -> Option<&str> {
        self.tailscale_ips
            .iter()
            .find(|ip| ip.contains('.'))
            .or_else(|| self.tailscale_ips.first())
            .map(String::as_str)
    }

    /// MagicDNS name without the trailing dot.
    pub fn dns_name_trimmed(&self) -> &str {
        self.dns_name.trim_end_matches('.')
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct UserProfile {
    #[serde(rename = "ID")]
    pub id: u64,
    pub login_name: String,
    pub display_name: String,
    #[serde(rename = "ProfilePicURL")]
    pub profile_pic_url: String,
}

/// Response from `GET /localapi/v0/whois?addr=<ip[:port]>`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct WhoIs {
    pub node: WhoIsNode,
    pub user_profile: UserProfile,
    /// Peer capabilities granted by the tailnet ACL (grants). Keys are capability names.
    pub cap_map: Option<HashMap<String, serde_json::Value>>,
}

impl WhoIs {
    /// True for tagged nodes (servers), which have no interactive user.
    pub fn is_tagged(&self) -> bool {
        self.node.tags.as_ref().is_some_and(|t| !t.is_empty())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct WhoIsNode {
    /// Numeric node ID.
    #[serde(rename = "ID")]
    pub id: u64,
    /// Stable node ID matching [`PeerStatus::id`].
    #[serde(rename = "StableID")]
    pub stable_id: String,
    /// MagicDNS name with trailing dot.
    pub name: String,
    pub user: u64,
    pub tags: Option<Vec<String>>,
    pub online: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_subset() {
        let json = r#"{
            "Version": "1.104.6",
            "BackendState": "Running",
            "TailscaleIPs": ["100.1.2.3", "fd7a::1"],
            "Self": {"ID": "nSELF", "HostName": "me", "DNSName": "me.tail.ts.net.", "OS": "windows",
                     "UserID": 5, "TailscaleIPs": ["100.1.2.3"], "Online": true},
            "Peer": {
                "nodekey:aa": {"ID": "nPEER", "HostName": "laptop", "DNSName": "laptop.tail.ts.net.",
                               "OS": "linux", "UserID": 5, "TailscaleIPs": ["fd7a::2", "100.1.2.4"],
                               "Online": false, "Tags": ["tag:server"], "LastSeen": "2026-11-12T00:00:00Z"}
            },
            "User": {"5": {"ID": 5, "LoginName": "a@b.c", "DisplayName": "A"}},
            "MagicDNSSuffix": "tail.ts.net"
        }"#;
        let status: Status = serde_json::from_str(json).unwrap();
        assert!(status.is_running());
        assert_eq!(status.self_node.id, "nSELF");
        let peers = status.peers_sorted();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].primary_ip(), Some("100.1.2.4"));
        assert_eq!(peers[0].dns_name_trimmed(), "laptop.tail.ts.net");
        assert_eq!(status.user["5"].login_name, "a@b.c");
    }

    #[test]
    fn parses_whois() {
        let json = r#"{
            "Node": {"ID": 123, "StableID": "nPEER", "Name": "laptop.tail.ts.net.", "User": 5},
            "UserProfile": {"ID": 5, "LoginName": "a@b.c", "DisplayName": "A"},
            "CapMap": {"example.com/cap": [{"x": 1}]}
        }"#;
        let who: WhoIs = serde_json::from_str(json).unwrap();
        assert_eq!(who.node.stable_id, "nPEER");
        assert!(!who.is_tagged());
        assert!(who.cap_map.unwrap().contains_key("example.com/cap"));
    }
}
