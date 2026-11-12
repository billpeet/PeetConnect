//! Who may connect.
//!
//! Tailscale has already proven which node sent a packet; tailscaled's `whois` tells us the
//! node and the user behind it. We compare that identity with the allow-list. There is no
//! password: the only secret is the optional PIN, which exists for the case where several
//! people share one tailnet login.

use std::net::SocketAddr;

use peet_tailscale::{LocalApi, TailscaleError};

use crate::config::AllowConfig;

/// Who is on the other end, for logs and the connection list.
#[derive(Debug, Clone)]
pub struct Identity {
    pub node: String,
    pub user: Option<String>,
    pub tags: Vec<String>,
}

impl std::fmt::Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.user, self.tags.is_empty()) {
            (Some(user), _) => write!(f, "{} ({})", self.node, user),
            (None, false) => write!(f, "{} [{}]", self.node, self.tags.join(",")),
            (None, true) => write!(f, "{}", self.node),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Denied {
    #[error("{0} is not a Tailscale address")]
    NotTailnet(SocketAddr),
    #[error("tailscaled could not identify {addr}: {reason}")]
    Unknown { addr: SocketAddr, reason: String },
    #[error("{0} is not on the allow-list")]
    NotAllowed(Identity),
}

#[derive(Debug)]
pub struct Authoriser {
    api: LocalApi,
    allow: AllowConfig,
}

impl Authoriser {
    pub fn new(allow: AllowConfig) -> Self {
        Self {
            api: LocalApi::new(),
            allow,
        }
    }

    pub async fn authorise(&self, remote: SocketAddr) -> Result<Identity, Denied> {
        if remote.ip().is_loopback() {
            if self.allow.local {
                return Ok(Identity {
                    node: "localhost".into(),
                    user: None,
                    tags: Vec::new(),
                });
            }
            return Err(Denied::NotTailnet(remote));
        }
        if !peet_transport::is_tailscale_ip(remote.ip()) {
            return Err(Denied::NotTailnet(remote));
        }
        let who = self.api.whois(remote.ip()).await.map_err(|err| {
            let reason = match &err {
                TailscaleError::Http { status, .. } if status.as_u16() == 404 => {
                    "not a tailnet peer".to_string()
                }
                other => other.to_string(),
            };
            Denied::Unknown {
                addr: remote,
                reason,
            }
        })?;
        let identity = Identity {
            node: who.node.name.trim_end_matches('.').to_string(),
            user: (!who.is_tagged() && !who.user_profile.login_name.is_empty())
                .then(|| who.user_profile.login_name.clone()),
            tags: who.node.tags.clone().unwrap_or_default(),
        };
        let a = &self.allow;
        let node_matches = a.nodes.iter().any(|n| {
            n.eq_ignore_ascii_case(&who.node.stable_id)
                || n.eq_ignore_ascii_case(&identity.node)
                || identity
                    .node
                    .split('.')
                    .next()
                    .is_some_and(|short| n.eq_ignore_ascii_case(short))
        });
        let user_matches = identity
            .user
            .as_ref()
            .is_some_and(|u| a.users.iter().any(|x| x.eq_ignore_ascii_case(u)));
        let tag_matches = identity.tags.iter().any(|t| a.tags.contains(t));
        if a.any_tailnet || node_matches || user_matches || tag_matches {
            Ok(identity)
        } else {
            Err(Denied::NotAllowed(identity))
        }
    }
}
