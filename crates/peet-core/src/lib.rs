//! Client core: everything a viewer, CLI or agent needs to talk to a PeetConnect host.
//!
//! [`connect`] performs the handshake and returns a [`Session`]: a [`Control`] handle for
//! sending requests (stream selection, keyframes, screenshots, input) and an event receiver
//! for everything the host pushes back (video frames, cursor, statistics). The GUI viewer,
//! the `peet` CLI and the MCP server all sit on this crate so they behave identically.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use peet_proto::{
    Capabilities, ClientHello, Codec, ControlMessage, CursorUpdate, EncodedImage, Feature,
    InputEvent, MonitorId, MonitorInfo, RejectReason, ScreenshotRequest, ScreenshotResult,
    ServerHello, SessionId, StreamKind, VideoFrame, DEFAULT_PORT, PROTOCOL_VERSION,
};
use peet_transport::{quinn, FramedRecv, FramedSend, TransportError};
use tokio::sync::{mpsc, oneshot};

pub use peet_proto as proto;

/// How often the client pings the host to measure round-trip time.
pub const PING_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("could not resolve {0}")]
    Resolve(String),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("host rejected the connection: {0:?}")]
    Rejected(RejectReason),
    #[error("handshake failed: {0}")]
    Handshake(String),
}

/// Options for [`connect`].
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// Shown in the host's connection list. Defaults to this machine's hostname.
    pub client_name: String,
    pub pin: Option<String>,
    /// Codecs in order of preference.
    pub codecs: Vec<Codec>,
    pub features: Vec<Feature>,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            client_name: gethostname::gethostname().to_string_lossy().into_owned(),
            pin: None,
            codecs: vec![Codec::H264],
            features: vec![Feature::Screenshot],
        }
    }
}

/// What the host told us at handshake time.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub host_name: String,
    pub codec: Codec,
    pub capabilities: Capabilities,
    pub monitors: Vec<MonitorInfo>,
    pub remote: SocketAddr,
}

/// Something the host pushed to us.
#[derive(Debug)]
pub enum Event {
    Video(VideoFrame),
    Cursor(CursorUpdate),
    MonitorsChanged(Vec<MonitorInfo>),
    StreamStats {
        monitor: MonitorId,
        fps: u16,
        bitrate_kbps: u32,
        skipped: u32,
    },
    /// The connection ended. No more events follow.
    Closed(String),
}

/// Resolves `host`, `host:port`, `ip`, or `[v6]:port` to a socket address, defaulting the
/// port to [`DEFAULT_PORT`]. MagicDNS names resolve through the system resolver.
pub async fn resolve(target: &str) -> Result<SocketAddr, ConnectError> {
    if let Ok(addr) = target.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = target.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, DEFAULT_PORT));
    }
    let with_port = if target.matches(':').count() >= 2 && !target.starts_with('[') {
        // Bare IPv6 without brackets: wrap it.
        format!("[{target}]:{DEFAULT_PORT}")
    } else if target
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        target.to_string()
    } else {
        format!("{target}:{DEFAULT_PORT}")
    };
    let mut addrs = tokio::net::lookup_host(&with_port)
        .await
        .map_err(|_| ConnectError::Resolve(target.to_string()))?;
    // Prefer a Tailscale address when the name resolves to several.
    let all: Vec<SocketAddr> = addrs.by_ref().collect();
    all.iter()
        .copied()
        .find(|a| peet_transport::is_tailscale_ip(a.ip()))
        .or_else(|| all.first().copied())
        .ok_or_else(|| ConnectError::Resolve(target.to_string()))
}

/// Connects and completes the handshake.
pub async fn connect(addr: SocketAddr, opts: ConnectOptions) -> Result<Session, ConnectError> {
    let (endpoint, connection) = peet_transport::connect(addr).await?;
    let (mut send, mut recv) = peet_transport::open_bi(&connection, StreamKind::Control).await?;
    send.send(&ClientHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: opts.client_name,
        capabilities: Capabilities {
            codecs: opts.codecs,
            features: opts.features,
        },
        pin: opts.pin,
    })
    .await?;
    let hello: ServerHello = recv
        .recv()
        .await?
        .ok_or_else(|| ConnectError::Handshake("host closed the control stream".into()))?;
    let info = match hello {
        ServerHello::Accepted {
            session_id,
            host_name,
            codec,
            capabilities,
            monitors,
        } => SessionInfo {
            session_id,
            host_name,
            codec,
            capabilities,
            monitors,
            remote: addr,
        },
        ServerHello::Rejected { reason } => return Err(ConnectError::Rejected(reason)),
    };
    tracing::info!(host = %info.host_name, %addr, ?info.codec, monitors = info.monitors.len(), "connected");
    Ok(Session::start(endpoint, connection, send, recv, info))
}

/// An open session. Drop it (or call [`Session::close`]) to disconnect.
#[derive(Debug)]
pub struct Session {
    info: SessionInfo,
    control: Control,
    events: mpsc::Receiver<Event>,
    connection: quinn::Connection,
    endpoint: quinn::Endpoint,
}

impl Session {
    fn start(
        endpoint: quinn::Endpoint,
        connection: quinn::Connection,
        send: FramedSend,
        recv: FramedRecv,
        info: SessionInfo,
    ) -> Self {
        let (event_tx, events) = mpsc::channel(128);
        let (ctl_tx, ctl_rx) = mpsc::channel(64);
        let (input_tx, input_rx) = mpsc::channel(512);
        let shared = Arc::new(Shared::default());
        let control = Control {
            tx: ctl_tx,
            input_tx,
            shared: shared.clone(),
        };

        tokio::spawn(control_writer(send, ctl_rx));
        tokio::spawn(control_reader(
            recv,
            shared.clone(),
            event_tx.clone(),
            connection.clone(),
        ));
        tokio::spawn(pinger(control.clone(), shared, connection.clone()));
        tokio::spawn(uni_acceptor(connection.clone(), event_tx));
        tokio::spawn(input_writer(connection.clone(), input_rx));

        Self {
            info,
            control,
            events,
            connection,
            endpoint,
        }
    }

    pub fn info(&self) -> &SessionInfo {
        &self.info
    }

    pub fn control(&self) -> Control {
        self.control.clone()
    }

    /// Next event from the host; `None` once the session is over and the final
    /// [`Event::Closed`] has been delivered.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    /// Splits into independent halves so the event loop can live on another task or thread.
    pub fn split(self) -> (SessionInfo, Control, mpsc::Receiver<Event>, SessionGuard) {
        (
            self.info,
            self.control,
            self.events,
            SessionGuard {
                connection: self.connection,
                endpoint: self.endpoint,
            },
        )
    }

    pub fn close(&self, reason: &str) {
        self.connection.close(0u32.into(), reason.as_bytes());
    }
}

/// Keeps the connection alive; closes it on drop.
#[derive(Debug)]
pub struct SessionGuard {
    connection: quinn::Connection,
    /// Kept alive so the UDP socket stays open for the connection.
    #[allow(dead_code)]
    endpoint: quinn::Endpoint,
}

impl SessionGuard {
    pub fn close(&self, reason: &str) {
        self.connection.close(0u32.into(), reason.as_bytes());
    }

    pub fn stats(&self) -> quinn::ConnectionStats {
        self.connection.stats()
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.connection.close(0u32.into(), b"viewer closed");
    }
}

#[derive(Debug, Default)]
struct Shared {
    next_request: AtomicU64,
    screenshots: Mutex<HashMap<u64, oneshot::Sender<ScreenshotResult>>>,
    pings: Mutex<HashMap<u64, Instant>>,
    rtt: Mutex<Option<Duration>>,
}

/// Cloneable handle for sending requests to the host.
#[derive(Debug, Clone)]
pub struct Control {
    tx: mpsc::Sender<ControlMessage>,
    input_tx: mpsc::Sender<InputEvent>,
    shared: Arc<Shared>,
}

impl Control {
    async fn send(&self, msg: ControlMessage) -> Result<(), ConnectError> {
        self.tx
            .send(msg)
            .await
            .map_err(|_| ConnectError::Handshake("session closed".into()))
    }

    /// Non-blocking variant for callers on non-async threads. Drops the message if the
    /// outgoing queue is full.
    pub fn try_send(&self, msg: ControlMessage) -> bool {
        self.tx.try_send(msg).is_ok()
    }

    pub async fn set_stream(
        &self,
        monitor: MonitorId,
        enabled: bool,
        max_fps: Option<u16>,
        max_bitrate_kbps: Option<u32>,
    ) -> Result<(), ConnectError> {
        self.send(ControlMessage::SetStream {
            monitor,
            enabled,
            max_fps,
            max_bitrate_kbps,
        })
        .await
    }

    pub async fn request_keyframe(&self, monitor: MonitorId) -> Result<(), ConnectError> {
        self.send(ControlMessage::RequestKeyframe { monitor }).await
    }

    /// Queues an input event. Input has its own stream so it is never stuck behind control
    /// traffic. Returns false if the queue is full.
    pub fn send_input(&self, event: InputEvent) -> bool {
        self.input_tx.try_send(event).is_ok()
    }

    /// Requests a still image and waits for it.
    pub async fn screenshot(
        &self,
        monitor: MonitorId,
        codec: Codec,
        max_width: Option<u32>,
        timeout: Duration,
    ) -> Result<EncodedImage, ConnectError> {
        let request_id = self.shared.next_request.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.shared
            .screenshots
            .lock()
            .expect("screenshot map poisoned")
            .insert(request_id, tx);
        self.send(ControlMessage::Screenshot(ScreenshotRequest {
            request_id,
            monitor,
            codec,
            max_width,
        }))
        .await?;
        let result = tokio::time::timeout(timeout, rx).await;
        self.shared
            .screenshots
            .lock()
            .expect("screenshot map poisoned")
            .remove(&request_id);
        match result {
            Ok(Ok(ScreenshotResult { result, .. })) => result.map_err(ConnectError::Handshake),
            Ok(Err(_)) => Err(ConnectError::Handshake("session closed".into())),
            Err(_) => Err(ConnectError::Handshake("screenshot timed out".into())),
        }
    }

    /// Latest measured round-trip time, if a pong has arrived.
    pub fn rtt(&self) -> Option<Duration> {
        *self.shared.rtt.lock().expect("rtt poisoned")
    }
}

async fn control_writer(mut send: FramedSend, mut rx: mpsc::Receiver<ControlMessage>) {
    while let Some(msg) = rx.recv().await {
        if let Err(err) = send.send(&msg).await {
            tracing::debug!(%err, "control stream write failed");
            break;
        }
    }
}

async fn control_reader(
    mut recv: FramedRecv,
    shared: Arc<Shared>,
    events: mpsc::Sender<Event>,
    connection: quinn::Connection,
) {
    let reason = loop {
        match recv.recv::<ControlMessage>().await {
            Ok(Some(msg)) => match msg {
                ControlMessage::Ping { nonce } => {
                    // Hosts may ping too; answer on the writer via events is overkill, so we
                    // simply ignore: our own pings measure the RTT.
                    tracing::trace!(nonce, "host ping");
                }
                ControlMessage::Pong { nonce } => {
                    let sent = shared.pings.lock().expect("pings poisoned").remove(&nonce);
                    if let Some(sent) = sent {
                        *shared.rtt.lock().expect("rtt poisoned") = Some(sent.elapsed());
                    }
                }
                ControlMessage::ScreenshotResult(result) => {
                    let waiter = shared
                        .screenshots
                        .lock()
                        .expect("screenshot map poisoned")
                        .remove(&result.request_id);
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(result);
                    }
                }
                ControlMessage::Cursor(update) => {
                    if events.send(Event::Cursor(update)).await.is_err() {
                        break "consumer gone".to_string();
                    }
                }
                ControlMessage::MonitorsChanged { monitors } => {
                    if events.send(Event::MonitorsChanged(monitors)).await.is_err() {
                        break "consumer gone".to_string();
                    }
                }
                ControlMessage::StreamStats {
                    monitor,
                    fps,
                    bitrate_kbps,
                    skipped,
                } => {
                    let event = Event::StreamStats {
                        monitor,
                        fps,
                        bitrate_kbps,
                        skipped,
                    };
                    if events.send(event).await.is_err() {
                        break "consumer gone".to_string();
                    }
                }
                ControlMessage::Disconnect { reason } => break reason,
                ControlMessage::SetStream { .. }
                | ControlMessage::RequestKeyframe { .. }
                | ControlMessage::Screenshot(_) => {
                    tracing::debug!(?msg, "ignoring client-only message from host");
                }
            },
            Ok(None) => break "host closed the control stream".to_string(),
            Err(err) => break describe_close(&connection, &err),
        }
    };
    let _ = events.send(Event::Closed(reason)).await;
}

fn describe_close(connection: &quinn::Connection, err: &TransportError) -> String {
    match connection.close_reason() {
        Some(quinn::ConnectionError::ApplicationClosed(close)) => {
            let text = String::from_utf8_lossy(&close.reason);
            if text.is_empty() {
                "host closed the connection".to_string()
            } else {
                text.into_owned()
            }
        }
        Some(quinn::ConnectionError::TimedOut) => "connection timed out".to_string(),
        Some(other) => other.to_string(),
        None => err.to_string(),
    }
}

async fn pinger(control: Control, shared: Arc<Shared>, connection: quinn::Connection) {
    let mut interval = tokio::time::interval(PING_INTERVAL);
    let mut nonce: u64 = 0;
    loop {
        interval.tick().await;
        if connection.close_reason().is_some() {
            break;
        }
        nonce += 1;
        shared
            .pings
            .lock()
            .expect("pings poisoned")
            .insert(nonce, Instant::now());
        if control.send(ControlMessage::Ping { nonce }).await.is_err() {
            break;
        }
    }
}

async fn uni_acceptor(connection: quinn::Connection, events: mpsc::Sender<Event>) {
    loop {
        match peet_transport::accept_uni(&connection).await {
            Ok((StreamKind::Video, recv)) => {
                tokio::spawn(video_reader(recv, events.clone()));
            }
            Ok((kind, _)) => tracing::debug!(?kind, "ignoring unexpected stream from host"),
            Err(TransportError::Connection(_)) => break,
            Err(err) => {
                tracing::debug!(%err, "accepting stream failed");
                break;
            }
        }
    }
}

async fn video_reader(mut recv: FramedRecv, events: mpsc::Sender<Event>) {
    loop {
        match recv.recv::<VideoFrame>().await {
            Ok(Some(frame)) => {
                if events.send(Event::Video(frame)).await.is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(%err, "video stream ended");
                break;
            }
        }
    }
}

async fn input_writer(connection: quinn::Connection, mut rx: mpsc::Receiver<InputEvent>) {
    let mut stream: Option<FramedSend> = None;
    while let Some(event) = rx.recv().await {
        if stream.is_none() {
            match peet_transport::open_uni(&connection, StreamKind::Input).await {
                Ok(s) => stream = Some(s),
                Err(err) => {
                    tracing::debug!(%err, "could not open input stream");
                    break;
                }
            }
        }
        if let Some(s) = stream.as_mut() {
            if let Err(err) = s.send(&event).await {
                tracing::debug!(%err, "input stream write failed");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_forms() {
        assert_eq!(
            resolve("100.64.0.1").await.unwrap(),
            "100.64.0.1:7735".parse().unwrap()
        );
        assert_eq!(
            resolve("100.64.0.1:9000").await.unwrap(),
            "100.64.0.1:9000".parse().unwrap()
        );
        assert_eq!(
            resolve("[::1]:9000").await.unwrap(),
            "[::1]:9000".parse().unwrap()
        );
        assert_eq!(resolve("localhost:7735").await.unwrap().port(), 7735);
    }
}
