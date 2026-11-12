//! A Model Context Protocol server on stdio.
//!
//! This is the agent surface for milestone 5: an MCP client (an LLM agent) spawns `peet mcp`
//! and drives PeetConnect hosts as tools, with no GUI. The server speaks newline-delimited
//! JSON-RPC 2.0 on stdin/stdout and keeps one live session per host open across tool calls,
//! so a sequence of clicks and screenshots reuses a single QUIC connection (the "local
//! control API" the plan calls for). Idle sessions reconnect lazily on the next call.
//!
//! The protocol framing is implemented directly rather than pulled from a crate: it is a small,
//! stable surface and keeps the dependency tree clean.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use peet_core::proto::{Codec, InputEvent, MonitorId, MouseButton};
use peet_core::{Control, Event, SessionInfo};
use peet_tailscale::LocalApi;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::virtual_point;

/// Protocol version we advertise when the client does not pin one.
const PROTOCOL_VERSION: &str = "2025-06-18";
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(15);
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(10);
/// Grace after flushing input so the host receives and injects it before the next call.
const INPUT_GRACE: Duration = Duration::from_millis(250);

/// Runs the server until stdin closes.
pub async fn serve() -> Result<()> {
    let sessions = Arc::new(Sessions::default());
    let api = LocalApi::new();

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => {
                // Parse error: reply with a null id per JSON-RPC.
                write_message(
                    &mut stdout,
                    &rpc_error(&Value::Null, -32700, &err.to_string()),
                )
                .await?;
                continue;
            }
        };

        // Notifications (no `id`) get no response.
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        let reply = match dispatch(&method, params, &sessions, &api).await {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(err) => rpc_error(&id, err.0, &err.1),
        };
        write_message(&mut stdout, &reply).await?;
    }
    Ok(())
}

/// A JSON-RPC error as (code, message).
type RpcError = (i64, String);

fn err(code: i64, message: impl Into<String>) -> RpcError {
    (code, message.into())
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

async fn write_message(stdout: &mut tokio::io::Stdout, value: &Value) -> Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    stdout.write_all(line.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

async fn dispatch(
    method: &str,
    params: Value,
    sessions: &Arc<Sessions>,
    api: &LocalApi,
) -> Result<Value, RpcError> {
    match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION)
                .to_string();
            Ok(json!({
                "protocolVersion": requested,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "peetconnect", "version": env!("CARGO_PKG_VERSION") },
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_list() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| err(-32602, "tools/call needs a tool name"))?;
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            // Tool execution errors are reported inside the result, not as protocol errors, so
            // the agent can read them and react.
            let result = match call_tool(name, &args, sessions, api).await {
                Ok(content) => json!({ "content": content }),
                Err(e) => json!({
                    "content": [text_content(&format!("Error: {e:#}"))],
                    "isError": true,
                }),
            };
            Ok(result)
        }
        other => Err(err(-32601, format!("method not found: {other}"))),
    }
}

// ---- tools -----------------------------------------------------------------------------------

fn text_content(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// Common `host` and `pin` fields shared by most tools.
fn host_prop() -> Value {
    json!({ "type": "string", "description": "Host name, MagicDNS name or IP, optional :port" })
}

fn tool_list() -> Vec<Value> {
    vec![
        json!({
            "name": "peet_peers",
            "description": "List devices on the tailnet.",
            "inputSchema": { "type": "object", "properties": {
                "all": { "type": "boolean", "description": "Include offline devices" }
            } },
        }),
        json!({
            "name": "peet_monitors",
            "description": "List a host's monitors (id, name, geometry, DPI scale).",
            "inputSchema": { "type": "object", "required": ["host"], "properties": {
                "host": host_prop(), "pin": { "type": "string" }
            } },
        }),
        json!({
            "name": "peet_screenshot",
            "description": "Capture a host monitor as a PNG image. Returns the image.",
            "inputSchema": { "type": "object", "required": ["host"], "properties": {
                "host": host_prop(),
                "monitor": { "type": "integer", "description": "Monitor id; default primary" },
                "max_width": { "type": "integer", "description": "Downscale to at most this wide" },
                "pin": { "type": "string" }
            } },
        }),
        json!({
            "name": "peet_move",
            "description": "Move the pointer to monitor-relative X,Y (or absolute with absolute=true).",
            "inputSchema": { "type": "object", "required": ["host", "x", "y"], "properties": {
                "host": host_prop(),
                "x": { "type": "integer" }, "y": { "type": "integer" },
                "monitor": { "type": "integer" }, "absolute": { "type": "boolean" },
                "pin": { "type": "string" }
            } },
        }),
        json!({
            "name": "peet_click",
            "description": "Click a mouse button, optionally moving to X,Y first.",
            "inputSchema": { "type": "object", "required": ["host"], "properties": {
                "host": host_prop(),
                "x": { "type": "integer" }, "y": { "type": "integer" },
                "button": { "type": "string", "enum": ["left", "right", "middle", "back", "forward"] },
                "double": { "type": "boolean" },
                "monitor": { "type": "integer" }, "absolute": { "type": "boolean" },
                "pin": { "type": "string" }
            } },
        }),
        json!({
            "name": "peet_scroll",
            "description": "Scroll the wheel in notches. Positive dy up, positive dx right.",
            "inputSchema": { "type": "object", "required": ["host"], "properties": {
                "host": host_prop(),
                "dx": { "type": "integer" }, "dy": { "type": "integer" },
                "pin": { "type": "string" }
            } },
        }),
        json!({
            "name": "peet_type",
            "description": "Type Unicode text on the host.",
            "inputSchema": { "type": "object", "required": ["host", "text"], "properties": {
                "host": host_prop(), "text": { "type": "string" }, "pin": { "type": "string" }
            } },
        }),
        json!({
            "name": "peet_key",
            "description": "Press a key chord such as enter, ctrl+c or ctrl+alt+delete.",
            "inputSchema": { "type": "object", "required": ["host", "chord"], "properties": {
                "host": host_prop(), "chord": { "type": "string" }, "pin": { "type": "string" }
            } },
        }),
        json!({
            "name": "peet_clipboard_get",
            "description": "Read the host's clipboard text.",
            "inputSchema": { "type": "object", "required": ["host"], "properties": {
                "host": host_prop(), "pin": { "type": "string" }
            } },
        }),
        json!({
            "name": "peet_clipboard_set",
            "description": "Write text to the host's clipboard.",
            "inputSchema": { "type": "object", "required": ["host", "text"], "properties": {
                "host": host_prop(), "text": { "type": "string" }, "pin": { "type": "string" }
            } },
        }),
    ]
}

async fn call_tool(
    name: &str,
    args: &Value,
    sessions: &Arc<Sessions>,
    api: &LocalApi,
) -> Result<Vec<Value>> {
    match name {
        "peet_peers" => tool_peers(args, api).await,
        "peet_monitors" => tool_monitors(args, sessions).await,
        "peet_screenshot" => tool_screenshot(args, sessions).await,
        "peet_move" => tool_move(args, sessions).await,
        "peet_click" => tool_click(args, sessions).await,
        "peet_scroll" => tool_scroll(args, sessions).await,
        "peet_type" => tool_type(args, sessions).await,
        "peet_key" => tool_key(args, sessions).await,
        "peet_clipboard_get" => tool_clipboard_get(args, sessions).await,
        "peet_clipboard_set" => tool_clipboard_set(args, sessions).await,
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

async fn tool_peers(args: &Value, api: &LocalApi) -> Result<Vec<Value>> {
    let all = arg_bool(args, "all").unwrap_or(false);
    let status = crate::fetch_status(api).await?;
    let peers: Vec<_> = status
        .peers_sorted()
        .into_iter()
        .filter(|p| all || p.online)
        .collect();
    let json = crate::peers_json(&status, &peers);
    Ok(vec![text_content(&serde_json::to_string_pretty(&json)?)])
}

async fn tool_monitors(args: &Value, sessions: &Arc<Sessions>) -> Result<Vec<Value>> {
    let slot = sessions.acquire(args)?;
    let info = run(&slot, |_control, info| async move { Ok(info) }).await?;
    let monitors: Vec<Value> = info
        .monitors
        .iter()
        .map(|m| {
            json!({
                "id": m.id.0, "name": m.name,
                "x": m.bounds.x, "y": m.bounds.y,
                "width": m.bounds.width, "height": m.bounds.height,
                "scale_percent": m.scale_percent, "primary": m.primary,
            })
        })
        .collect();
    let out = json!({ "host_name": info.host_name, "monitors": monitors });
    Ok(vec![text_content(&serde_json::to_string_pretty(&out)?)])
}

async fn tool_screenshot(args: &Value, sessions: &Arc<Sessions>) -> Result<Vec<Value>> {
    let monitor = arg_u32(args, "monitor");
    let max_width = arg_u32(args, "max_width");
    let slot = sessions.acquire(args)?;
    let image = run(&slot, |control, info| async move {
        let target = pick_monitor(&info, monitor)?;
        let img = control
            .screenshot(target, Codec::Png, max_width, SCREENSHOT_TIMEOUT)
            .await?;
        Ok(img)
    })
    .await?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&image.data);
    Ok(vec![
        text_content(&format!(
            "{}x{} PNG, {} bytes",
            image.width,
            image.height,
            image.data.len()
        )),
        json!({ "type": "image", "data": b64, "mimeType": "image/png" }),
    ])
}

async fn tool_move(args: &Value, sessions: &Arc<Sessions>) -> Result<Vec<Value>> {
    let x = arg_i32(args, "x").context("x is required")?;
    let y = arg_i32(args, "y").context("y is required")?;
    let monitor = arg_u32(args, "monitor");
    let absolute = arg_bool(args, "absolute").unwrap_or(false);
    let slot = sessions.acquire(args)?;
    let (vx, vy) = run(&slot, |control, info| async move {
        let (vx, vy) = virtual_point(&info, monitor, x, y, absolute)
            .map_err(|e| peet_core::ConnectError::Handshake(e.to_string()))?;
        control.send_input(InputEvent::MouseMove { x: vx, y: vy });
        settle(&control).await;
        Ok((vx, vy))
    })
    .await?;
    Ok(vec![text_content(&format!("moved to {vx},{vy}"))])
}

async fn tool_click(args: &Value, sessions: &Arc<Sessions>) -> Result<Vec<Value>> {
    let x = arg_i32(args, "x");
    let y = arg_i32(args, "y");
    if x.is_some() != y.is_some() {
        return Err(anyhow!("click needs both x and y, or neither"));
    }
    let button = parse_button(arg_str(args, "button").unwrap_or("left"))?;
    let double = arg_bool(args, "double").unwrap_or(false);
    let monitor = arg_u32(args, "monitor");
    let absolute = arg_bool(args, "absolute").unwrap_or(false);
    let slot = sessions.acquire(args)?;
    run(&slot, |control, info| async move {
        if let (Some(x), Some(y)) = (x, y) {
            let (vx, vy) = virtual_point(&info, monitor, x, y, absolute)
                .map_err(|e| peet_core::ConnectError::Handshake(e.to_string()))?;
            control.send_input(InputEvent::MouseMove { x: vx, y: vy });
        }
        for _ in 0..if double { 2 } else { 1 } {
            control.send_input(InputEvent::MouseButton {
                button,
                pressed: true,
            });
            control.send_input(InputEvent::MouseButton {
                button,
                pressed: false,
            });
        }
        settle(&control).await;
        Ok(())
    })
    .await?;
    Ok(vec![text_content(&format!("clicked {button:?}"))])
}

async fn tool_scroll(args: &Value, sessions: &Arc<Sessions>) -> Result<Vec<Value>> {
    let dx = arg_i32(args, "dx").unwrap_or(0);
    let dy = arg_i32(args, "dy").unwrap_or(0);
    let slot = sessions.acquire(args)?;
    run(&slot, |control, _info| async move {
        control.send_input(InputEvent::MouseWheel {
            delta_x: dx * 120,
            delta_y: dy * 120,
        });
        settle(&control).await;
        Ok(())
    })
    .await?;
    Ok(vec![text_content(&format!("scrolled dx={dx} dy={dy}"))])
}

async fn tool_type(args: &Value, sessions: &Arc<Sessions>) -> Result<Vec<Value>> {
    let text = arg_str(args, "text")
        .context("text is required")?
        .to_string();
    let slot = sessions.acquire(args)?;
    let count = text.chars().count();
    run(&slot, |control, _info| {
        let text = text.clone();
        async move {
            control.send_input(InputEvent::Text { text });
            settle(&control).await;
            Ok(())
        }
    })
    .await?;
    Ok(vec![text_content(&format!("typed {count} characters"))])
}

async fn tool_key(args: &Value, sessions: &Arc<Sessions>) -> Result<Vec<Value>> {
    let chord = arg_str(args, "chord")
        .context("chord is required")?
        .to_string();
    let names = peet_input::keys::parse_chord(&chord);
    if names.is_empty() {
        return Err(anyhow!("empty key chord"));
    }
    let mut keys = Vec::with_capacity(names.len());
    for name in &names {
        keys.push(peet_input::keys::lookup(name).ok_or_else(|| anyhow!("unknown key `{name}`"))?);
    }
    let slot = sessions.acquire(args)?;
    run(&slot, |control, _info| {
        let keys = keys.clone();
        async move {
            for key in &keys {
                control.send_input(InputEvent::Key {
                    scancode: key.scancode,
                    extended: key.extended,
                    pressed: true,
                });
            }
            for key in keys.iter().rev() {
                control.send_input(InputEvent::Key {
                    scancode: key.scancode,
                    extended: key.extended,
                    pressed: false,
                });
            }
            settle(&control).await;
            Ok(())
        }
    })
    .await?;
    Ok(vec![text_content(&format!("sent {chord}"))])
}

async fn tool_clipboard_get(args: &Value, sessions: &Arc<Sessions>) -> Result<Vec<Value>> {
    let slot = sessions.acquire(args)?;
    let text = run(&slot, |control, _info| async move {
        control.get_clipboard(CLIPBOARD_TIMEOUT).await
    })
    .await?;
    Ok(vec![text_content(text.as_deref().unwrap_or(""))])
}

async fn tool_clipboard_set(args: &Value, sessions: &Arc<Sessions>) -> Result<Vec<Value>> {
    let text = arg_str(args, "text")
        .context("text is required")?
        .to_string();
    let slot = sessions.acquire(args)?;
    let count = text.chars().count();
    run(&slot, |control, _info| {
        let text = text.clone();
        async move {
            control.set_clipboard(text).await?;
            tokio::time::sleep(Duration::from_millis(150)).await;
            Ok(())
        }
    })
    .await?;
    Ok(vec![text_content(&format!(
        "set host clipboard ({count} characters)"
    ))])
}

// ---- session cache ---------------------------------------------------------------------------

/// One live, headless session plus a task that drains and discards host events so the control
/// reader never stalls on a full event queue. The drain task owns the session guard, so the
/// connection stays open until the task ends or is aborted (on `Live` drop).
struct Live {
    info: SessionInfo,
    control: Control,
    alive: Arc<AtomicBool>,
    drain: tokio::task::JoinHandle<()>,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.drain.abort();
    }
}

/// A host we may hold a session to. Locked while (re)connecting.
struct HostSlot {
    host: String,
    pin: Option<String>,
    live: tokio::sync::Mutex<Option<Live>>,
}

impl HostSlot {
    async fn connect(&self) -> Result<Live, peet_core::ConnectError> {
        let addr = peet_core::resolve(&self.host).await?;
        let opts = peet_core::ConnectOptions {
            client_name: format!(
                "peet MCP on {}",
                gethostname::gethostname().to_string_lossy()
            ),
            pin: self.pin.clone(),
            ..peet_core::ConnectOptions::default()
        };
        let session = peet_core::connect(addr, opts).await?;
        let (info, control, mut events, guard) = session.split();
        let alive = Arc::new(AtomicBool::new(true));
        let flag = alive.clone();
        let drain = tokio::spawn(async move {
            // Hold the guard so the connection stays open for the slot's lifetime; it closes
            // when this task ends or is aborted.
            let _guard = guard;
            while let Some(event) = events.recv().await {
                if matches!(event, Event::Closed(_)) {
                    break;
                }
            }
            flag.store(false, Ordering::Relaxed);
        });
        Ok(Live {
            info,
            control,
            alive,
            drain,
        })
    }
}

#[derive(Default)]
struct Sessions {
    hosts: std::sync::Mutex<HashMap<String, Arc<HostSlot>>>,
}

impl Sessions {
    /// Returns the slot for this tool call's `host`, creating it on first use.
    fn acquire(&self, args: &Value) -> Result<Arc<HostSlot>> {
        let host = arg_str(args, "host")
            .context("host is required")?
            .to_string();
        let pin = arg_str(args, "pin").map(str::to_string);
        let key = format!("{host}|{}", pin.as_deref().unwrap_or(""));
        let mut hosts = self.hosts.lock().expect("sessions poisoned");
        let slot = hosts.entry(key).or_insert_with(|| {
            Arc::new(HostSlot {
                host,
                pin,
                live: tokio::sync::Mutex::new(None),
            })
        });
        Ok(slot.clone())
    }
}

/// Runs `op` against a connected session for `slot`, reconnecting once if the session is gone
/// or the operation fails on a dead connection.
async fn run<T, F, Fut>(slot: &Arc<HostSlot>, op: F) -> Result<T>
where
    F: Fn(Control, SessionInfo) -> Fut,
    Fut: std::future::Future<Output = Result<T, peet_core::ConnectError>>,
{
    let mut last_err: Option<peet_core::ConnectError> = None;
    for _ in 0..2 {
        let (control, info) = {
            let mut live = slot.live.lock().await;
            let need = live
                .as_ref()
                .is_none_or(|l| !l.alive.load(Ordering::Relaxed));
            if need {
                *live = Some(slot.connect().await?);
            }
            let l = live.as_ref().expect("just connected");
            (l.control.clone(), l.info.clone())
        };
        match op(control, info).await {
            Ok(value) => return Ok(value),
            Err(e) => {
                // Drop the session so the next attempt reconnects.
                *slot.live.lock().await = None;
                last_err = Some(e);
            }
        }
    }
    Err(last_err.map_or_else(|| anyhow!("operation failed"), anyhow::Error::new))
}

async fn settle(control: &Control) {
    control.flush_input().await;
    tokio::time::sleep(INPUT_GRACE).await;
}

// ---- argument + value helpers ----------------------------------------------------------------

fn pick_monitor(
    info: &SessionInfo,
    monitor: Option<u32>,
) -> Result<MonitorId, peet_core::ConnectError> {
    let target = match monitor {
        Some(id) => info.monitors.iter().find(|m| m.id.0 == id),
        None => info
            .monitors
            .iter()
            .find(|m| m.primary)
            .or(info.monitors.first()),
    };
    target
        .map(|m| m.id)
        .ok_or_else(|| peet_core::ConnectError::Handshake("host reports no such monitor".into()))
}

fn parse_button(name: &str) -> Result<MouseButton> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "left" => MouseButton::Left,
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        "back" => MouseButton::Back,
        "forward" => MouseButton::Forward,
        other => return Err(anyhow!("unknown mouse button: {other}")),
    })
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn arg_i32(args: &Value, key: &str) -> Option<i32> {
    args.get(key)
        .and_then(Value::as_i64)
        .and_then(|v| i32::try_from(v).ok())
}

fn arg_u32(args: &Value, key: &str) -> Option<u32> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
}
