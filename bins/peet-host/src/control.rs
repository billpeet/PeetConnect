//! The host's half of the local control API (`peet-control`): a registry of live sessions,
//! the paused flag, change notifications, and the named-pipe server the tray talks to.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use peet_control::{HostMessage, HostMode, HostStatus, Notice, Request, SessionInfo};
use tokio::io::BufReader;
use tokio::sync::{broadcast, Notify};

use crate::session::HostState;

/// Something a control client should hear about.
#[derive(Debug, Clone)]
enum Change {
    /// Status changed; send a fresh snapshot.
    Status,
    /// Tell the person, then send a fresh snapshot.
    Notice(Notice),
}

/// Live state shared by sessions and control clients.
#[derive(Debug)]
pub struct ControlHub {
    config_path: Option<PathBuf>,
    paused: AtomicBool,
    sessions: Mutex<BTreeMap<u64, Live>>,
    changes: broadcast::Sender<Change>,
    /// Signalled when a control client asks the (console) host to stop.
    pub shutdown: Notify,
}

#[derive(Debug)]
struct Live {
    info: SessionInfo,
    kick: Arc<Notify>,
}

impl ControlHub {
    pub fn new(config_path: Option<PathBuf>) -> Self {
        Self {
            config_path,
            paused: AtomicBool::new(false),
            sessions: Mutex::default(),
            changes: broadcast::channel(64).0,
            shutdown: Notify::new(),
        }
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Records a new session and announces it. The returned `Notify` fires when someone at
    /// the host asks for the session to end.
    pub fn register(&self, info: SessionInfo) -> Arc<Notify> {
        let kick = Arc::new(Notify::new());
        self.sessions.lock().expect("sessions poisoned").insert(
            info.id,
            Live {
                info: info.clone(),
                kick: kick.clone(),
            },
        );
        let _ = self
            .changes
            .send(Change::Notice(Notice::Connected { session: info }));
        kick
    }

    pub fn unregister(&self, id: u64, reason: String) {
        let removed = self.sessions.lock().expect("sessions poisoned").remove(&id);
        if let Some(live) = removed {
            let _ = self.changes.send(Change::Notice(Notice::Disconnected {
                session: live.info,
                reason,
            }));
        }
    }

    pub fn rejected(&self, who: String, reason: &str) {
        let _ = self.changes.send(Change::Notice(Notice::Rejected {
            who,
            reason: reason.to_string(),
        }));
    }

    fn set_paused(&self, paused: bool) {
        if self.paused.swap(paused, Ordering::Relaxed) != paused {
            tracing::info!(
                paused,
                "new connections {}",
                if paused { "paused" } else { "resumed" }
            );
            let _ = self.changes.send(Change::Status);
        }
    }

    fn kick(&self, id: u64) {
        let sessions = self.sessions.lock().expect("sessions poisoned");
        if let Some(live) = sessions.get(&id) {
            tracing::info!(session = id, who = %live.info.describe(), "disconnect requested at the host");
            live.kick.notify_one();
        }
    }
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn status(state: &HostState, listen: &str) -> HostStatus {
    let hub = &state.hub;
    HostStatus {
        name: state.config.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        mode: if state.config.follow_desktop {
            HostMode::ServiceWorker
        } else {
            HostMode::Console
        },
        listen: listen.to_string(),
        paused: hub.paused(),
        admits_anyone: state.config.admits_anyone(),
        pin_required: state.config.pin.is_some(),
        config_path: hub.config_path.as_ref().map(|p| p.display().to_string()),
        log_path: state.config.follow_desktop.then(|| {
            crate::machine::log_dir()
                .join("host.log")
                .display()
                .to_string()
        }),
        sessions: hub
            .sessions
            .lock()
            .expect("sessions poisoned")
            .values()
            .map(|l| l.info.clone())
            .collect(),
    }
}

/// Serves the control pipe until the process exits. A failure to bind (another host already
/// owns the pipe) is logged and leaves the host running without a control API.
pub async fn serve(state: Arc<HostState>, listen: String) {
    let mut listener = match peet_control::ControlListener::bind() {
        Ok(listener) => listener,
        Err(err) => {
            tracing::warn!(%err, pipe = peet_control::PIPE_NAME, "control pipe unavailable; the tray cannot see this host");
            return;
        }
    };
    tracing::info!(pipe = peet_control::PIPE_NAME, "control pipe ready");
    loop {
        match listener.accept().await {
            Ok(pipe) => {
                tokio::spawn(client(state.clone(), listen.clone(), pipe));
            }
            Err(err) => {
                tracing::warn!(%err, "control pipe accept failed");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

async fn client(
    state: Arc<HostState>,
    listen: String,
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
) {
    tracing::debug!("control client connected");
    let (reader, mut writer) = tokio::io::split(pipe);
    let mut reader = BufReader::new(reader);
    let mut changes = state.hub.changes.subscribe();
    let send_status = |state: &HostState| HostMessage::Status(status(state, &listen));

    if peet_control::write_message(&mut writer, &send_status(&state))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            request = peet_control::read_message::<_, Request>(&mut reader) => {
                match request {
                    Ok(Some(request)) => handle(&state, &request),
                    Ok(None) => break,
                    Err(err) => {
                        tracing::debug!(%err, "bad control request");
                        break;
                    }
                }
            }
            change = changes.recv() => {
                let message = match change {
                    Ok(Change::Notice(notice)) => {
                        if peet_control::write_message(&mut writer, &HostMessage::Notice(notice)).await.is_err() {
                            break;
                        }
                        send_status(&state)
                    }
                    Ok(Change::Status) | Err(broadcast::error::RecvError::Lagged(_)) => send_status(&state),
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if peet_control::write_message(&mut writer, &message).await.is_err() {
                    break;
                }
            }
        }
    }
    tracing::debug!("control client left");
}

fn handle(state: &HostState, request: &Request) {
    match request {
        Request::Disconnect { session } => state.hub.kick(*session),
        Request::SetPaused { paused } => state.hub.set_paused(*paused),
        Request::Shutdown => {
            if state.config.follow_desktop {
                tracing::warn!("ignoring shutdown request: the service owns this worker");
            } else {
                tracing::info!("shutdown requested over the control pipe");
                state.hub.shutdown.notify_one();
            }
        }
    }
}
