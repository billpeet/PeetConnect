//! HTTP/1.1 client for the tailscaled LocalAPI over local IPC.

use std::net::IpAddr;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::types::{Status, WhoIs};

/// Host header tailscaled expects; the value is conventional, not routable.
const HOST_HEADER: &str = "local-tailscaled.sock";

/// Upper bound on a LocalAPI call; tailscaled normally answers in milliseconds.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Where to find tailscaled's LocalAPI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// Windows named pipe, e.g. `\\.\pipe\ProtectedPrefix\Administrators\Tailscale\tailscaled`.
    NamedPipe(String),
    /// Unix domain socket, e.g. `/var/run/tailscale/tailscaled.sock`.
    UnixSocket(std::path::PathBuf),
}

impl Endpoint {
    /// The default endpoint for the current platform.
    pub fn platform_default() -> Self {
        #[cfg(windows)]
        {
            Endpoint::NamedPipe(
                r"\\.\pipe\ProtectedPrefix\Administrators\Tailscale\tailscaled".to_owned(),
            )
        }
        #[cfg(target_os = "macos")]
        {
            Endpoint::UnixSocket("/var/run/tailscaled.socket".into())
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            Endpoint::UnixSocket("/var/run/tailscale/tailscaled.sock".into())
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TailscaleError {
    /// tailscaled is not running or not reachable at the configured endpoint.
    #[error("cannot reach tailscaled at {endpoint:?}: {source}")]
    NotReachable {
        endpoint: Endpoint,
        #[source]
        source: std::io::Error,
    },
    #[error("LocalAPI returned {status}: {body}")]
    Http { status: StatusCode, body: String },
    #[error("LocalAPI transport error: {0}")]
    Transport(#[from] hyper::Error),
    #[error("LocalAPI request build error: {0}")]
    Request(#[from] hyper::http::Error),
    #[error("LocalAPI returned malformed JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("this endpoint type is not supported on this platform")]
    UnsupportedEndpoint,
    #[error("LocalAPI request timed out after {0:?}")]
    Timeout(Duration),
}

/// A client for the local tailscaled instance.
///
/// Each request opens a fresh connection. The LocalAPI is local and cheap, and this keeps
/// the client free of connection state and safe to share.
#[derive(Debug, Clone)]
pub struct LocalApi {
    endpoint: Endpoint,
}

impl Default for LocalApi {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalApi {
    /// Client for the platform default endpoint.
    pub fn new() -> Self {
        Self {
            endpoint: Endpoint::platform_default(),
        }
    }

    pub fn with_endpoint(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Full node status: this node, every peer, and the user table.
    pub async fn status(&self) -> Result<Status, TailscaleError> {
        self.get_json("/localapi/v0/status").await
    }

    /// Identity of the tailnet node that owns `addr`.
    ///
    /// Returns an HTTP 404 error if the address is not a tailnet peer, which callers should
    /// treat as "not authorised" rather than as a transport failure.
    pub async fn whois(&self, addr: IpAddr) -> Result<WhoIs, TailscaleError> {
        self.get_json(&format!("/localapi/v0/whois?addr={addr}"))
            .await
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, TailscaleError> {
        tokio::time::timeout(DEFAULT_TIMEOUT, self.get_json_inner(path))
            .await
            .map_err(|_| TailscaleError::Timeout(DEFAULT_TIMEOUT))?
    }

    async fn get_json_inner<T: DeserializeOwned>(&self, path: &str) -> Result<T, TailscaleError> {
        let response = match &self.endpoint {
            #[cfg(windows)]
            Endpoint::NamedPipe(name) => {
                let pipe =
                    pipe::connect(name)
                        .await
                        .map_err(|source| TailscaleError::NotReachable {
                            endpoint: self.endpoint.clone(),
                            source,
                        })?;
                send(pipe, path).await?
            }
            #[cfg(unix)]
            Endpoint::UnixSocket(sock_path) => {
                let stream =
                    tokio::net::UnixStream::connect(sock_path)
                        .await
                        .map_err(|source| TailscaleError::NotReachable {
                            endpoint: self.endpoint.clone(),
                            source,
                        })?;
                send(stream, path).await?
            }
            #[allow(unreachable_patterns)]
            _ => return Err(TailscaleError::UnsupportedEndpoint),
        };

        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();
        if !status.is_success() {
            return Err(TailscaleError::Http {
                status,
                body: String::from_utf8_lossy(&body).trim().to_owned(),
            });
        }
        Ok(serde_json::from_slice(&body)?)
    }
}

async fn send<S>(io: S, path: &str) -> Result<Response<Incoming>, TailscaleError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;
    tokio::spawn(async move {
        if let Err(err) = conn.await {
            tracing::debug!(%err, "LocalAPI connection closed with error");
        }
    });
    let request = Request::get(path)
        .header(hyper::header::HOST, HOST_HEADER)
        .body(Empty::<Bytes>::new())?;
    Ok(sender.send_request(request).await?)
}

#[cfg(windows)]
mod pipe {
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

    /// `SECURITY_SQOS_PRESENT`: the QoS flags below are meaningful.
    const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
    /// `SECURITY_IMPERSONATION`: let the server impersonate us. tailscaled needs this to
    /// learn which Windows user is calling; tokio's default of identification-only yields 401.
    const SECURITY_IMPERSONATION: u32 = 0x0002_0000;

    /// `ERROR_PIPE_BUSY`: all pipe instances are busy; retry shortly.
    const ERROR_PIPE_BUSY: i32 = 231;

    pub async fn connect(name: &str) -> std::io::Result<NamedPipeClient> {
        let mut attempts = 0;
        loop {
            match ClientOptions::new()
                .security_qos_flags(SECURITY_SQOS_PRESENT | SECURITY_IMPERSONATION)
                .open(name)
            {
                Ok(client) => return Ok(client),
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempts < 20 => {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }
}
