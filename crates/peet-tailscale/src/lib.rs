//! Client for the local `tailscaled` LocalAPI.
//!
//! PeetConnect uses Tailscale for two things:
//!
//! * **Discovery**: [`LocalApi::status`] lists every device on the tailnet, which is the
//!   viewer's connection list.
//! * **Authentication**: [`LocalApi::whois`] maps the source address of an incoming QUIC
//!   connection to a tailnet node and user. The host authorises against that identity, so
//!   PeetConnect needs no account system of its own.
//!
//! The LocalAPI is plain HTTP/1.1 over a local IPC endpoint: a named pipe on Windows and a
//! Unix socket elsewhere. On Windows the pipe handle must allow impersonation or tailscaled
//! answers 401, because it identifies the caller by impersonating the pipe client.

#![forbid(unsafe_code)]

mod localapi;
pub mod types;

pub use localapi::{Endpoint, LocalApi, TailscaleError};
pub use types::{PeerStatus, Status, UserProfile, WhoIs, WhoIsNode};
