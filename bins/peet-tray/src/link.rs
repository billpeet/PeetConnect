//! The tray's connection to the host's control pipe, on its own thread.
//!
//! Keeps trying to connect while no host is running; while connected, mirrors the host's
//! status into [`Shared`], queues notices, forwards requests, and pokes the UI window after
//! every change.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use peet_control::{HostMessage, HostStatus, Notice, Request};
use tokio::io::BufReader;
use tokio::sync::mpsc;

/// State the link thread writes and the UI thread reads.
#[derive(Debug, Default)]
pub struct Shared {
    /// `None` while no host is reachable.
    pub status: Option<HostStatus>,
    /// Notices not yet shown.
    pub notices: Vec<Notice>,
}

/// Starts the link thread. `wake` is called (from that thread) after every change.
pub fn spawn(
    shared: Arc<Mutex<Shared>>,
    wake: impl Fn() + Send + 'static,
) -> std::io::Result<mpsc::UnboundedSender<Request>> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("control-link".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            runtime.block_on(run(&shared, &wake, rx));
        })?;
    Ok(tx)
}

async fn run(
    shared: &Mutex<Shared>,
    wake: &impl Fn(),
    mut requests: mpsc::UnboundedReceiver<Request>,
) {
    loop {
        if let Ok(pipe) = peet_control::connect().await {
            session(pipe, shared, wake, &mut requests).await;
        }
        if shared
            .lock()
            .expect("shared poisoned")
            .status
            .take()
            .is_some()
        {
            wake();
        }
        // Requests made while no host was listening are meaningless to the next one.
        while requests.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
}

async fn session(
    pipe: tokio::net::windows::named_pipe::NamedPipeClient,
    shared: &Mutex<Shared>,
    wake: &impl Fn(),
    requests: &mut mpsc::UnboundedReceiver<Request>,
) {
    let (reader, mut writer) = tokio::io::split(pipe);
    let mut reader = BufReader::new(reader);
    loop {
        tokio::select! {
            message = peet_control::read_message::<_, HostMessage>(&mut reader) => {
                let Ok(Some(message)) = message else { return };
                {
                    let mut shared = shared.lock().expect("shared poisoned");
                    match message {
                        HostMessage::Status(status) => shared.status = Some(status),
                        HostMessage::Notice(notice) => shared.notices.push(notice),
                    }
                }
                wake();
            }
            request = requests.recv() => {
                let Some(request) = request else { return };
                if peet_control::write_message(&mut writer, &request).await.is_err() {
                    return;
                }
            }
        }
    }
}
