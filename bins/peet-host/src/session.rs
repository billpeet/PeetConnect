//! One viewer connection: handshake, then the control loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use peet_capture::windows::{
    describe_window, enumerate_monitors, enumerate_windows, wgc_supported, Backend,
};
use peet_capture::{MonitorDesc, WindowDesc};
use peet_proto::{
    Capabilities, ClientHello, Codec, ControlMessage, Feature, InputEvent, RejectReason,
    ScreenshotResult, ServerHello, SessionId, SourceId, StreamKind, WindowChange, PROTOCOL_VERSION,
};
use peet_transport::{quinn, FramedRecv, FramedSend, TransportError};
use tokio::sync::mpsc;

use crate::auth::Authoriser;
use crate::config::HostConfig;
use crate::control::ControlHub;
use crate::stream::{Source, StreamParams, VideoStream};

/// Shared across sessions.
#[derive(Debug)]
pub struct HostState {
    pub config: HostConfig,
    pub auth: Authoriser,
    pub next_session: AtomicU64,
    pub hub: ControlHub,
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn run(state: Arc<HostState>, connection: quinn::Connection) {
    let remote = connection.remote_address();
    match handshake(&state, &connection).await {
        Ok(Some((session_id, send, recv, identity, monitors))) => {
            tracing::info!(%remote, session = session_id.0, who = %identity, "session started");
            let started = Instant::now();
            let kick = state.hub.register(peet_control::SessionInfo {
                id: session_id.0,
                node: identity.node.clone(),
                user: identity.user.clone(),
                remote: remote.to_string(),
                started_unix_ms: crate::control::now_unix_ms(),
            });
            let reason =
                match control_loop(&state, &connection, session_id, send, recv, monitors, &kick)
                    .await
                {
                    Ok(reason) => reason.to_string(),
                    Err(err) => format!("{err:#}"),
                };
            tracing::info!(%remote, session = session_id.0, %reason, elapsed = ?started.elapsed(), "session ended");
            state.hub.unregister(session_id.0, reason);
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
            state
                .hub
                .rejected(remote.ip().to_string(), &format!("not allowed: {denied}"));
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
    if state.hub.paused() {
        tracing::info!(%remote, who = %identity, "rejected: connections are paused");
        state
            .hub
            .rejected(identity.to_string(), "new connections are paused");
        return reject(&mut send, connection, RejectReason::Paused).await;
    }
    if let Some(pin) = &state.config.pin {
        match &hello.pin {
            None => return reject(&mut send, connection, RejectReason::PinRequired).await,
            Some(offered) if !constant_time_eq(offered.as_bytes(), pin.as_bytes()) => {
                tracing::warn!(%remote, who = %identity, "wrong PIN");
                state.hub.rejected(identity.to_string(), "wrong PIN");
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
            features: {
                let mut features = vec![Feature::Screenshot, Feature::Clipboard];
                if wgc_supported() {
                    features.push(Feature::WindowCapture);
                }
                features
            },
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
    kick: &tokio::sync::Notify,
) -> Result<&'static str> {
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMessage>(256);
    let mut streams: HashMap<SourceId, VideoStream> = HashMap::new();
    let session_start = Instant::now();
    // Sleeping displays are composed only a few times a second; wake them and keep them on.
    let _awake = if state.config.wake_display {
        let follow_desktop = state.config.follow_desktop;
        tokio::task::spawn_blocking(move || crate::display::wake(follow_desktop));
        crate::display::KeepAwake::new()
    } else {
        None
    };
    tokio::spawn(accept_input_streams(
        connection.clone(),
        session_id,
        state.config.follow_desktop,
    ));
    // Clipboard sync: a watcher pushes local changes to the viewer; `last_set` remembers what
    // the viewer last wrote so its own text is not echoed straight back.
    let last_set: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
    let watcher_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let tx = ctl_tx.clone();
        let last_set = last_set.clone();
        let stop = watcher_stop.clone();
        std::thread::Builder::new()
            .name(format!("clipboard-{}", session_id.0))
            .spawn(move || clipboard_watcher(&tx, &last_set, &stop))
            .context("spawning clipboard watcher")?;
    }
    let _stop_on_exit = StopOnDrop(watcher_stop);

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
                    ControlMessage::SetStream { source, enabled, max_fps, max_bitrate_kbps } => {
                        // Window streams end on their own when the window closes.
                        streams.retain(|_, s| !s.finished());
                        if !enabled {
                            if streams.remove(&source).is_some() {
                                tracing::info!(session = session_id.0, %source, "stream disabled");
                            }
                            continue;
                        }
                        let stream_source = match source {
                            SourceId::Monitor(monitor) => {
                                let Some(desc) = monitors.iter().find(|m| m.id == monitor) else {
                                    tracing::warn!(session = session_id.0, %source, "unknown monitor requested");
                                    continue;
                                };
                                Source::Monitor(desc.clone())
                            }
                            SourceId::Window(window) => {
                                if describe_window(window.0).is_none() {
                                    tracing::warn!(session = session_id.0, %source, "unknown window requested");
                                    send.send(&ControlMessage::WindowChanged { window, change: WindowChange::Closed }).await?;
                                    continue;
                                }
                                Source::Window(window)
                            }
                        };
                        let params = StreamParams {
                            fps: max_fps.map_or(state.config.fps, |f| u32::from(f).min(state.config.fps)).max(1),
                            bitrate_kbps: max_bitrate_kbps.map_or(state.config.max_bitrate_kbps, |b| b.min(state.config.max_bitrate_kbps)).max(200),
                            backend: parse_backend(&state.config.capture),
                            follow_desktop: state.config.follow_desktop,
                            window_popups: state.config.window_popups,
                        };
                        if streams.get(&source).is_some_and(|s| s.params.fps == params.fps && s.params.bitrate_kbps == params.bitrate_kbps) {
                            streams[&source].request_keyframe();
                            continue;
                        }
                        // Replace any existing stream (drop stops it first).
                        streams.remove(&source);
                        streams.insert(source, VideoStream::start(
                            connection.clone(), stream_source, params, ctl_tx.clone(), session_start,
                        ));
                    }
                    ControlMessage::RequestKeyframe { source } => {
                        if let Some(stream) = streams.get(&source) {
                            stream.request_keyframe();
                        }
                    }
                    ControlMessage::ListWindows { request_id } => {
                        let tx = ctl_tx.clone();
                        tokio::task::spawn_blocking(move || {
                            let windows = enumerate_windows()
                                .unwrap_or_else(|err| {
                                    tracing::warn!(%err, "listing windows failed");
                                    Vec::new()
                                })
                                .iter()
                                .map(WindowDesc::to_proto)
                                .collect();
                            let _ = tx.blocking_send(ControlMessage::Windows { request_id, windows });
                        });
                    }
                    ControlMessage::FocusWindow { window } => {
                        let follow_desktop = state.config.follow_desktop;
                        tokio::task::spawn_blocking(move || {
                            let _desktop = follow_desktop.then(crate::desktop::attach_input_desktop).flatten();
                            if let Err(err) = crate::window::focus(window.0) {
                                tracing::info!(window = window.0, %err, "focus failed");
                            }
                        });
                    }
                    ControlMessage::ResizeWindow { window, width, height } => {
                        tokio::task::spawn_blocking(move || {
                            if let Err(err) = crate::window::resize(window.0, width, height) {
                                tracing::info!(window = window.0, %err, "resize failed");
                            }
                        });
                    }
                    ControlMessage::Screenshot(request) => {
                        let monitor = match request.source {
                            SourceId::Monitor(id) => monitors.iter().find(|m| m.id == id).cloned(),
                            SourceId::Window(_) => None,
                        };
                        let adapter = monitors.first().map_or(0, |m| m.adapter_index);
                        let tx = ctl_tx.clone();
                        let popups = state.config.window_popups;
                        tokio::task::spawn_blocking(move || {
                            let result = match (request.source, monitor) {
                                (SourceId::Window(w), _) => crate::screenshot::capture_window(w.0, adapter, request.codec, request.max_width, popups),
                                (_, Some(desc)) => crate::screenshot::capture(&desc, request.codec, request.max_width),
                                (source, None) => Err(format!("{source} not found")),
                            };
                            let _ = tx.blocking_send(ControlMessage::ScreenshotResult(ScreenshotResult {
                                request_id: request.request_id,
                                result,
                            }));
                        });
                    }
                    ControlMessage::SetClipboard { text } => {
                        let last_set = last_set.clone();
                        tokio::task::spawn_blocking(move || {
                            *last_set.lock().expect("last_set poisoned") = Some(text.clone());
                            if let Err(err) = crate::clipboard::set_text(&text) {
                                tracing::debug!(%err, "set clipboard failed");
                            }
                        });
                    }
                    ControlMessage::GetClipboard { request_id } => {
                        let tx = ctl_tx.clone();
                        tokio::task::spawn_blocking(move || {
                            let text = crate::clipboard::get_text().unwrap_or_else(|err| {
                                tracing::debug!(%err, "get clipboard failed");
                                None
                            });
                            let _ = tx.blocking_send(ControlMessage::ClipboardText { request_id, text });
                        });
                    }
                    ControlMessage::Disconnect { reason } => {
                        tracing::info!(session = session_id.0, %reason, "viewer disconnected");
                        return Ok("viewer disconnected");
                    }
                    ControlMessage::Cursor(_)
                    | ControlMessage::MonitorsChanged { .. }
                    | ControlMessage::ScreenshotResult(_)
                    | ControlMessage::ClipboardText { .. }
                    | ControlMessage::Windows { .. }
                    | ControlMessage::WindowChanged { .. }
                    | ControlMessage::StreamStats { .. } => {
                        tracing::debug!(session = session_id.0, ?msg, "ignoring host-only message from viewer");
                    }
                }
            }
            outgoing = ctl_rx.recv() => {
                let Some(msg) = outgoing else { return Ok("control channel closed") };
                send.send(&msg).await?;
            }
            () = kick.notified() => {
                drop(streams);
                send.send(&ControlMessage::Disconnect {
                    reason: "disconnected by the person at the host".into(),
                })
                .await?;
                let _ = send.finish();
                // Let the message reach the viewer before the connection is closed.
                tokio::time::sleep(Duration::from_millis(200)).await;
                return Ok("disconnected at the host");
            }
            () = connection.closed_signal() => {
                anyhow::bail!("connection closed");
            }
        }
    }
}

/// Sets the flag when dropped, so the watcher thread exits with the control loop.
struct StopOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Polls the clipboard sequence number and pushes new text to the viewer as an unsolicited
/// `ClipboardText` (request id 0). Text the viewer itself just set is skipped.
fn clipboard_watcher(
    tx: &mpsc::Sender<ControlMessage>,
    last_set: &std::sync::Mutex<Option<String>>,
    stop: &std::sync::atomic::AtomicBool,
) {
    let mut seen = crate::clipboard::sequence_number();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(300));
        let seq = crate::clipboard::sequence_number();
        if seq == seen {
            continue;
        }
        seen = seq;
        let Ok(Some(text)) = crate::clipboard::get_text() else {
            continue;
        };
        if last_set.lock().expect("last_set poisoned").as_deref() == Some(text.as_str()) {
            continue;
        }
        tracing::debug!(
            chars = text.chars().count(),
            "clipboard changed; pushing to viewer"
        );
        let msg = ControlMessage::ClipboardText {
            request_id: 0,
            text: Some(text),
        };
        if tx.blocking_send(msg).is_err() {
            break;
        }
    }
}

/// Input arrives on its own stream. Each stream gets a task that injects every event into the
/// interactive desktop via `SendInput`. Injection runs on a dedicated blocking thread so a slow
/// FFI call never stalls the async runtime, and so every event for a stream keeps the same
/// desktop attachment.
async fn accept_input_streams(
    connection: quinn::Connection,
    session_id: SessionId,
    follow_desktop: bool,
) {
    loop {
        match peet_transport::accept_uni(&connection).await {
            Ok((StreamKind::Input, recv)) => {
                tokio::spawn(inject_input_stream(recv, session_id, follow_desktop));
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

/// Reads input events off one stream and feeds them to a blocking injector thread.
async fn inject_input_stream(mut recv: FramedRecv, session_id: SessionId, follow_desktop: bool) {
    // A bounded channel bridges async reads to the synchronous SendInput thread. Input is tiny,
    // so a shallow queue keeps latency low while absorbing short stalls.
    let (tx, rx) = std::sync::mpsc::sync_channel::<InputEvent>(256);
    let worker = std::thread::spawn(move || {
        // SendInput targets this thread's desktop, so follow the input desktop when running as
        // the service worker. The guard is held for the thread's lifetime.
        let _desktop = follow_desktop
            .then(crate::desktop::attach_input_desktop)
            .flatten();
        let mut injector = peet_input::Injector::new();
        let mut count: u64 = 0;
        while let Ok(event) = rx.recv() {
            if let Err(err) = injector.inject(&event) {
                tracing::debug!(session = session_id.0, %err, "input injection failed");
            }
            count += 1;
        }
        count
    });
    while let Ok(Some(event)) = recv.recv::<InputEvent>().await {
        if tx.send(event).is_err() {
            break;
        }
    }
    drop(tx);
    let count = tokio::task::spawn_blocking(move || worker.join().unwrap_or(0))
        .await
        .unwrap_or(0);
    tracing::debug!(session = session_id.0, count, "input stream ended");
}

pub fn parse_backend(name: &str) -> Backend {
    match name {
        "dxgi" => Backend::Dxgi,
        "gdi" => Backend::Gdi,
        "wgc" => Backend::Wgc,
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
