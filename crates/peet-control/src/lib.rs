//! Local control API of a running `peet-host`.
//!
//! The host serves it on the named pipe [`PIPE_NAME`]; local tools (the tray today, the CLI
//! later) connect to watch status and issue commands. Framing is one JSON object per line in
//! each direction, so a session can be inspected by hand.
//!
//! On connect the host sends a [`HostMessage::Status`] snapshot, then another whenever
//! anything changes, interleaved with [`HostMessage::Notice`]s for events worth telling a
//! person about (someone connected, left, or was turned away). The client sends [`Request`]s
//! at any time; the host answers with the updated status, not with a reply per request.
//!
//! The pipe accepts no remote clients, and its security descriptor admits SYSTEM,
//! administrators, the host process's owner and interactive users. The last one matters when
//! the host runs as the service worker under SYSTEM and the tray runs as the signed-in user.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Name of the host's control pipe. One host serves a machine, so the name is fixed.
pub const PIPE_NAME: &str = r"\\.\pipe\peetconnect-host";

/// Client to host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// End this viewer session. The viewer is told and does not reconnect.
    Disconnect { session: u64 },
    /// While paused the host turns away new viewers; sessions already running continue.
    SetPaused { paused: bool },
    /// Stop the host. Honoured in console mode only: the service would restart its worker.
    Shutdown,
}

/// Host to client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    Status(HostStatus),
    Notice(Notice),
}

/// Everything a status display needs, sent whole on every change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    /// Name shown to viewers.
    pub name: String,
    /// `peet-host` version.
    pub version: String,
    pub pid: u32,
    pub mode: HostMode,
    /// Listening address, e.g. `0.0.0.0:7735`.
    pub listen: String,
    pub paused: bool,
    /// False when no allow rule is configured, so every viewer will be rejected.
    pub admits_anyone: bool,
    pub pin_required: bool,
    /// The configuration file the host reads (it may not exist yet).
    pub config_path: Option<String>,
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostMode {
    /// A foreground `peet-host` process (started by hand or by the tray).
    Console,
    /// The per-session worker launched by the Windows service.
    ServiceWorker,
}

/// One connected viewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: u64,
    /// Tailscale node (device) name of the viewer.
    pub node: String,
    /// Tailscale login of the viewer's owner, absent for tagged nodes.
    pub user: Option<String>,
    /// Remote socket address.
    pub remote: String,
    /// Wall-clock start, milliseconds since the Unix epoch.
    pub started_unix_ms: u64,
}

impl SessionInfo {
    /// Short description for menus and notifications: `alice@example.com on laptop`.
    pub fn describe(&self) -> String {
        match &self.user {
            Some(user) => format!("{user} on {}", self.node),
            None => self.node.clone(),
        }
    }
}

/// Events worth surfacing to a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notice {
    Connected {
        session: SessionInfo,
    },
    Disconnected {
        session: SessionInfo,
        reason: String,
    },
    /// A connection attempt was refused (not allowed, wrong PIN, paused).
    Rejected {
        /// Who tried, as well as the host could tell.
        who: String,
        reason: String,
    },
}

/// Writes one message as a JSON line and flushes it.
pub async fn write_message<W, T>(writer: &mut W, message: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

/// Reads the next JSON line. `Ok(None)` at end of stream; blank lines are skipped.
pub async fn read_message<R, T>(reader: &mut R) -> std::io::Result<Option<T>>
where
    R: AsyncBufRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            return serde_json::from_str(&line)
                .map(Some)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err));
        }
    }
}

#[cfg(windows)]
pub use pipe::{connect, ControlListener};

#[cfg(windows)]
mod pipe {
    use std::io;

    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use windows::core::w;
    use windows::Win32::Foundation::{LocalFree, ERROR_PIPE_BUSY, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    use super::PIPE_NAME;

    /// Protected DACL: full control for SYSTEM, administrators and the creating owner; read and
    /// write (but not creating pipe instances) for interactive users.
    const SDDL: windows::core::PCWSTR = w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;GRGW;;;IU)");

    /// Server side: hands out one connected pipe per client.
    #[derive(Debug)]
    pub struct ControlListener {
        next: NamedPipeServer,
        descriptor: Descriptor,
    }

    impl ControlListener {
        /// Creates the first pipe instance. Fails if another process already owns the name,
        /// which also stops a second host (or an impostor) sharing it.
        pub fn bind() -> io::Result<Self> {
            let descriptor = Descriptor::new()?;
            let next = create(&descriptor, true)?;
            Ok(Self { next, descriptor })
        }

        /// Waits for the next client and returns its pipe.
        pub async fn accept(&mut self) -> io::Result<NamedPipeServer> {
            self.next.connect().await?;
            let fresh = create(&self.descriptor, false)?;
            Ok(std::mem::replace(&mut self.next, fresh))
        }
    }

    fn create(descriptor: &Descriptor, first: bool) -> io::Result<NamedPipeServer> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: descriptor.0 .0,
            bInheritHandle: false.into(),
        };
        // SAFETY: `attributes` points at a valid security descriptor owned by `descriptor`,
        // which outlives this call; CreateNamedPipeW copies what it needs.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(PIPE_NAME, (&raw mut attributes).cast())
        }
    }

    /// A self-relative security descriptor allocated by
    /// `ConvertStringSecurityDescriptorToSecurityDescriptorW`, freed on drop.
    #[derive(Debug)]
    struct Descriptor(PSECURITY_DESCRIPTOR);

    // The descriptor is immutable once built and only read by CreateNamedPipeW.
    unsafe impl Send for Descriptor {}
    unsafe impl Sync for Descriptor {}

    impl Descriptor {
        fn new() -> io::Result<Self> {
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            // SAFETY: SDDL is a valid NUL-terminated wide string; the out-pointer is ours.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    SDDL,
                    SDDL_REVISION_1,
                    &mut descriptor,
                    None,
                )
            }
            .map_err(io::Error::other)?;
            Ok(Self(descriptor))
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: allocated with LocalAlloc by the conversion above, freed exactly once.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0 .0)));
            }
        }
    }

    /// Connects to a running host. `NotFound` means no host is running.
    pub async fn connect() -> io::Result<NamedPipeClient> {
        loop {
            match ClientOptions::new().open(PIPE_NAME) {
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                    // Every instance is mid-handshake; the host creates another right away.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                other => return other,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn messages_round_trip_as_json_lines() {
        let status = HostMessage::Status(HostStatus {
            name: "studio".into(),
            version: "0.1.0".into(),
            pid: 42,
            mode: HostMode::Console,
            listen: "0.0.0.0:7735".into(),
            paused: false,
            admits_anyone: true,
            pin_required: false,
            config_path: None,
            sessions: vec![SessionInfo {
                id: 1,
                node: "laptop".into(),
                user: Some("a@b.c".into()),
                remote: "100.64.0.2:5000".into(),
                started_unix_ms: 0,
            }],
        });
        let mut buf = Vec::new();
        write_message(&mut buf, &status).await.unwrap();
        write_message(&mut buf, &Request::SetPaused { paused: true })
            .await
            .unwrap();
        let text = String::from_utf8(buf.clone()).unwrap();
        assert!(text.starts_with(r#"{"type":"status","name":"studio""#));
        assert!(text.contains(r#"{"type":"set_paused","paused":true}"#));

        let mut reader = tokio::io::BufReader::new(buf.as_slice());
        let first: HostMessage = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(first, status);
        let second: Request = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(second, Request::SetPaused { paused: true });
        assert!(read_message::<_, Request>(&mut reader)
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn describes_sessions() {
        let mut s = SessionInfo {
            id: 1,
            node: "laptop".into(),
            user: Some("a@b.c".into()),
            remote: String::new(),
            started_unix_ms: 0,
        };
        assert_eq!(s.describe(), "a@b.c on laptop");
        s.user = None;
        assert_eq!(s.describe(), "laptop");
    }
}
