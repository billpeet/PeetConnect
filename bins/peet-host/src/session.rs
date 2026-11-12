//! One viewer connection: handshake, then the control loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use peet_capture::windows::{enumerate_monitors, Backend};
use peet_capture::MonitorDesc;
use peet_proto::{
    Capabilities, ClientHello, Codec, ControlMessage, Feature, InputEvent, MonitorId, RejectReason,
    ScreenshotResult, ServerHello, SessionId, StreamKind, PROTOCOL_VERSION,
};
use peet_transport::{quinn, FramedRecv, FramedSend, TransportError};
use tokio::sync::mpsc;

use crate::auth::Authoriser;
use crate::config::HostConfig;
use crate::stream::{MonitorStream, StreamParams};

/// Shared across sessions.
#[derive(Debug)]
pub struct HostState {
    pub config: HostConfig,
    pub auth: Authoriser,
    pub next_session: AtomicU64,
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn run(state: Arc<HostState>, connection: quinn::Connection) {
    let remote = connection.remote_address();
    match handshake(&state, &connection).await {
        Ok(Some((session_id, send, recv, identity, monitors))) => {
            tracing::info!(%remote, session = session_id.0, who = %identity, "session started");
            let started = Instant::now();
            if let Err(err) =
                control_loop(&state, &connection, session_id, send, recv, monitors).await
            {
                tracing::info!(%remote, session = session_id.0, %err, "session ended");
            } else {
                tracing::info!(%remote, session = session_id.0, elapsed = ?started.elapsed(), "session ended");
            }
        }
        Ok(None) => {}
        Err(err) => tracing::warn!(%remote, %err, "handshake failed"),
    }
    connection.close(0u32.into(), b"session ended");
}

type Handshake = (
    SessionId,
    FramedSend,
    FramedRecv,
    crate::auth::Identity,
    Vec<MonitorDesc>,
);

/// Authorises the peer and exchanges hellos. `Ok(None)` means we rejected them politely.
async fn handshake(state: &HostState, connection: &quinn::Connection) -> Result<Option<Handshake>> {
    let remote = connection.remote_address();
    let (kind, mut send, mut recv) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, peet_transport::accept_bi(connection))
            .await
            .context("no control stream within the handshake timeout")??;
    anyhow::ensure!(
        kind == StreamKind::Control,
        "first stream was {kind:?}, not control"
    );
    let hello: ClientHello = tokio::time::timeout(HANDSHAKE_TIMEOUT, recv.recv())
        .await
        .context("no hello within the handshake timeout")??
        .context("peer closed the control stream before hello")?;

    let identity = match state.auth.authorise(remote).await {
        Ok(identity) => identity,
        Err(denied) => {
            tracing::warn!(%remote, client = %hello.client_name, %denied, "rejected");
            return reject(&mut send, connection, RejectReason::NotAuthorised).await;
        }
    };
    if hello.protocol_version != PROTOCOL_VERSION {
        tracing::warn!(%remote, who = %identity, version = hello.protocol_version, "protocol mismatch");
        return reject(
            &mut send,
            connection,
            RejectReason::UnsupportedProtocolVersion,
        )
        .await;
    }
    if let Some(pin) = &state.config.pin {
        match &hello.pin {
            None => return reject(&mut send, connection, RejectReason::PinRequired).await,
            Some(offered) if !constant_time_eq(offered.as_bytes(), pin.as_bytes()) => {
                tracing::warn!(%remote, who = %identity, "wrong PIN");
                return reject(&mut send, connection, RejectReason::PinIncorrect).await;
            }
            Some(_) => {}
        }
    }
    if !hello.capabilities.codecs.contains(&Codec::H264) {
        return reject(&mut send, connection, RejectReason::NoCommonCodec).await;
    }

    let monitors = enumerate_monitors().context("enumerating monitors")?;
    let session_id = SessionId(state.next_session.fetch_add(1, Ordering::Relaxed) + 1);
    send.send(&ServerHello::Accepted {
        session_id,
        host_name: state.config.name.clone(),
        codec: Codec::H264,
        capabilities: Capabilities {
            codecs: vec![Codec::H264],
            features: vec![Feature::Screenshot],
        },
        monitors: monitors.iter().map(MonitorDesc::to_proto).collect(),
    })
    .await?;
    Ok(Some((session_id, send, recv, identity, monitors)))
}

async fn reject(
    send: &mut FramedSend,
    connection: &quinn::Connection,
    reason: RejectReason,
) -> Result<Option<Handshake>> {
    let _ = send.send(&ServerHello::Rejected { reason }).await;
    let _ = send.finish();
    // Give the frame a moment to leave before closing the connection.
    tokio::time::sleep(Duration::from_millis(200)).await;
    connection.close(1u32.into(), format!("{reason:?}").as_bytes());
    Ok(None)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn control_loop(
    state: &HostState,
    connection: &quinn::Connection,
    session_id: SessionId,
    mut send: FramedSend,
    mut recv: FramedRecv,
    monitors: Vec<MonitorDesc>,
) -> Result<()> {
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMessage>(256);
    let mut streams: HashMap<MonitorId, MonitorStream> = HashMap::new();
    let session_start = Instant::now();
    tokio::spawn(accept_input_streams(connection.clone(), session_id));

    loop {
        tokio::select! {
            incoming = recv.recv::<ControlMessage>() => {
                let Some(msg) = incoming? else {
                    anyhow::bail!("viewer closed the control stream");
                };
                match msg {
                    ControlMessage::Ping { nonce } => {
                        send.send(&ControlMessage::Pong { nonce }).await?;
                    }
                    ControlMessage::Pong { .. } => {}
                    ControlMessage::SetStream { monitor, enabled, max_fps, max_bitrate_kbps } => {
                        if !enabled {
                            if streams.remove(&monitor).is_some() {
                                tracing::info!(session = session_id.0, monitor = monitor.0, "stream disabled");
                            }
                            continue;
                        }
                        let Some(desc) = monitors.iter().find(|m| m.id == monitor) else {
                            tracing::warn!(session = session_id.0, monitor = monitor.0, "unknown monitor requested");
                            continue;
                        };
                        let params = StreamParams {
                            fps: max_fps.map_or(state.config.fps, |f| u32::from(f).min(state.config.fps)).max(1),
                            bitrate_kbps: max_bitrate_kbps.map_or(state.config.max_bitrate_kbps, |b| b.min(state.config.max_bitrate_kbps)).max(200),
                            backend: parse_backend(&state.config.capture),
                        };
                        if streams.get(&monitor).is_some_and(|s| s.params.fps == params.fps && s.params.bitrate_kbps == params.bitrate_kbps) {
                            streams[&monitor].request_keyframe();
                            continue;
                        }
                        // Replace any existing stream (drop stops it first).
                        streams.remove(&monitor);
                        streams.insert(monitor, MonitorStream::start(
                            connection.clone(), desc.clone(), params, ctl_tx.clone(), session_start,
                        ));
                    }
                    ControlMessage::RequestKeyframe { monitor } => {
                        if let Some(stream) = streams.get(&monitor) {
                            stream.request_keyframe();
                        }
                    }
                    ControlMessage::Screenshot(request) => {
                        let desc = monitors.iter().find(|m| m.id == request.monitor).cloned();
                        let tx = ctl_tx.clone();
                        tokio::task::spawn_blocking(move || {
                            let result = match desc {
                                Some(desc) => crate::screenshot::capture(&desc, request.codec, request.max_width),
                                None => Err(format!("monitor {} not found", request.monitor.0)),
                            };
                            let _ = tx.blocking_send(ControlMessage::ScreenshotResult(ScreenshotResult {
                                request_id: request.request_id,
                                result,
                            }));
                        });
                    }
                    ControlMessage::Disconnect { reason } => {
                        tracing::info!(session = session_id.0, %reason, "viewer disconnected");
                        break;
                    }
                    ControlMessage::Cursor(_)
                    | ControlMessage::MonitorsChanged { .. }
                    | ControlMessage::ScreenshotResult(_)
                    | ControlMessage::StreamStats { .. } => {
                        tracing::debug!(session = session_id.0, ?msg, "ignoring host-only message from viewer");
                    }
                }
            }
            outgoing = ctl_rx.recv() => {
                let Some(msg) = outgoing else { break };
                send.send(&msg).await?;
            }
            () = connection.closed_signal() => {
                anyhow::bail!("connection closed");
            }
        }
    }
    drop(streams);
    Ok(())
}

/// Input arrives on its own stream. Injection lands in milestone 4; for now we drain and
/// count so a viewer that sends input early does not stall on flow control.
async fn accept_input_streams(connection: quinn::Connection, session_id: SessionId) {
    loop {
        match peet_transport::accept_uni(&connection).await {
            Ok((StreamKind::Input, mut recv)) => {
                tokio::spawn(async move {
                    let mut count: u64 = 0;
                    while let Ok(Some(_event)) = recv.recv::<InputEvent>().await {
                        count += 1;
                    }
                    tracing::debug!(session = session_id.0, count, "input stream ended");
                });
            }
            Ok((kind, _)) => tracing::debug!(session = session_id.0, ?kind, "unexpected stream"),
            Err(TransportError::Connection(_)) => break,
            Err(err) => {
                tracing::debug!(session = session_id.0, %err, "accept failed");
                break;
            }
        }
    }
}

pub fn parse_backend(name: &str) -> Backend {
    match name {
        "dxgi" => Backend::Dxgi,
        "gdi" => Backend::Gdi,
        _ => Backend::Auto,
    }
}

/// `quinn::Connection::closed` as a unit future for `select!`.
trait ClosedSignal {
    fn closed_signal(&self) -> impl std::future::Future<Output = ()>;
}

impl ClosedSignal for quinn::Connection {
    async fn closed_signal(&self) {
        let _ = self.closed().await;
    }
}
