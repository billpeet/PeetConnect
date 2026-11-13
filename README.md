<img src="assets/icon.svg" width="64" height="64" alt="">

# PeetConnect

[![CI](https://github.com/billpeet/peetconnect/actions/workflows/ci.yml/badge.svg)](https://github.com/billpeet/peetconnect/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Fast, lightweight remote desktop built for Tailscale. Rust end to end, MIT licensed.

- **Tailscale-native.** Hosts are reached by MagicDNS name or 100.x address and authorise viewers
  by their tailnet identity (`whois`), so there is no account, relay or port forwarding.
- **Low latency.** DXGI capture, hardware H.264 (Media Foundation), QUIC transport, DXVA decode
  straight into a DirectComposition surface. Several monitors at once, laid out as on the host.
- **Agent-friendly.** A headless CLI and an MCP server let an LLM agent see and drive a machine
  without a window.
- **Unattended.** Runs as a console app or as a Windows service that follows the active session.

Status: early development, Windows host and viewer. Clean-room, no AnyDesk or RustDesk code.
See [docs/PLAN.md](docs/PLAN.md) for architecture, decisions and milestones.

## Layout

| Path | What |
|---|---|
| `crates/peet-proto` | Wire protocol types and framing. No platform code. |
| `crates/peet-tailscale` | Client for the local tailscaled API: peer discovery and `whois` identity. |
| `crates/peet-capture` | Screen capture: DXGI Desktop Duplication with GDI fallback (Windows). |
| `crates/peet-transport` | QUIC (quinn) endpoints, self-signed TLS, tagged framed streams. |
| `crates/peet-core` | Client core: connect, handshake, control, video events, screenshots. Shared by viewer, CLI and (later) MCP. |
| `crates/peet-codec` | H.264 encode (Media Foundation hardware/software, openh264 fallback) and decode (Media Foundation + DXVA), GPU colour conversion. |
| `crates/peet-control` | Local control API of a running host: JSON lines over the `\\.\pipe\peetconnect-host` named pipe (status, sessions, notices; disconnect, pause, shutdown). |
| `crates/peet-input` | Synthesises mouse and keyboard input with `SendInput`; layout-independent key table. |
| `bins/peet` | Command line, and the `mcp` subcommand: an MCP server exposing hosts as agent tools. |
| `bins/peet-bench` | Capture and encode benchmarks. |
| `bins/peet-host` | Host: serves monitors to authorised tailnet viewers; injects input; runs as a console process or a LocalSystem service. |
| `bins/peet-tray` | Notification-area app for the host: status icon, connected viewers, connect/disconnect notifications, disconnect, pause, start/stop host, start with Windows. |
| `bins/peet-viewer` | GPUI viewer: home screen with tailnet devices, favourites and recents; `--connect <host>` for a direct session; `--loopback` test rig. |
| `crates/peet-winres` | Build-script helper: embeds the app icon and version info in the Windows executables. |
| `xtask` | Repository chores; `cargo xtask icons` regenerates `assets/peetconnect.ico` and `assets/icon-256.png` from the SVGs. |

## Try it

Requires a Rust toolchain and a running Tailscale client.

```
cargo run -p peet -- peers          # online tailnet devices
cargo run -p peet -- peers --all    # include offline
cargo run -p peet -- whoami
cargo run -p peet -- whois 100.x.y.z
cargo run -p peet -- --json peers   # machine-readable

cargo run --release -p peet-bench -- monitors
cargo run --release -p peet-bench -- encoders
cargo run --release -p peet-bench -- capture --seconds 5   # writes bench-out/monitor-<id>.h264
```

The benchmark opens animated windows on each monitor so the capturer always has motion.

```
cargo run --release -p peet-viewer -- --loopback --present d3d    # loopback mirror of the primary monitor
cargo run --release -p peet-viewer -- --loopback --present d3d --upscale 3840x2160 --bitrate-kbps 30000
cargo run --release -p peet-viewer -- --loopback --present gpui   # the rejected image-element path, for comparison
```

Stage timings print to stderr every two seconds.

### Remote session over Tailscale

On the machine to be viewed:

```
cargo run --release -p peet-host -- --allow-user you@example.com
```

Authorisation is the tailnet identity reported by tailscaled's `whois` for the connecting
address, matched against `--allow-user`, `--allow-tag`, `--allow-node` or `--allow-any-tailnet`
(plus an optional `--pin`). Non-tailnet addresses are rejected before the handshake. A config
file (`%APPDATA%\PeetConnect\host.toml`, see `bins/peet-host/src/config.rs`) holds the same
settings. Allow `peet-host.exe` through Windows Firewall when prompted.

On the viewing machine:

```
cargo run --release -p peet-viewer                                  # home screen
cargo run --release -p peet-viewer -- --connect <magicdns-name-or-100.x.y.z>
cargo run --release -p peet -- monitors <host>                      # headless: list monitors
cargo run --release -p peet -- screenshot <host> -o shot.png        # headless: PNG still
```

The home screen lists the devices on your tailnet (live, with online state), your favourites
(star a row) and recent connections; click a row or type an address and press Enter. A PIN box
sits next to the address (Tab switches between them); a host that wants a PIN sends you back to
it with a plain message. Frame-rate and bitrate presets below the address apply to the next
session. Those and the lists persist in `%APPDATA%\PeetConnect\viewer.toml`.

In a session the viewer forwards mouse and keyboard to the host. The bar at the top shows the
host, the stream statistics and buttons for each monitor, all monitors, fullscreen and
disconnect; the same from the keyboard is `Ctrl+Alt+1`..`9`, `Ctrl+Alt+0`, `Ctrl+Alt+F` and
`Ctrl+Alt+D`. In fullscreen the bar hides and comes back when the pointer touches the top edge.
Every other key reaches the remote machine. The host injects input with `SendInput` and so drives
whatever has focus in the interactive session. The clipboard syncs both ways (text only): copy on
either side and paste on the other. While connecting, and after a drop, a card over the video says
what is happening and offers to cancel; the viewer reconnects on its own with backoff and returns
to the monitor you were watching. A host that never answered or rejected you is reported back on
the home screen in plain words.

The CLI drives input headlessly, which is how an agent clicks and types without a window:

```
cargo run --release -p peet -- move  <host> 400 300          # pointer to monitor-relative x y
cargo run --release -p peet -- click <host> 400 300          # move then left-click
cargo run --release -p peet -- type  <host> "hello world"    # Unicode text
cargo run --release -p peet -- key   <host> ctrl+alt+delete  # a key chord
cargo run --release -p peet -- scroll <host> --dy 3          # wheel notches
cargo run --release -p peet -- clipboard <host>              # read the host clipboard
cargo run --release -p peet -- clipboard <host> --set "hi"   # write the host clipboard
```

Coordinates are relative to the chosen monitor (`--monitor`, default primary) unless
`--absolute` is given.

### Agent surface (MCP)

`peet mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io) server on stdio, so
an LLM agent can drive hosts as tools with no GUI. It keeps one QUIC session per host open across
tool calls and reconnects lazily, so a run of clicks and screenshots reuses a single connection.

```
cargo run --release -p peet -- mcp
```

Register it with an MCP client by pointing the client at the `peet` binary with the `mcp`
argument. The tools are `peet_peers`, `peet_monitors`, `peet_screenshot` (returns a PNG image),
`peet_move`, `peet_click`, `peet_scroll`, `peet_type`, `peet_key`, `peet_clipboard_get` and
`peet_clipboard_set`. Each takes the target `host` (and an optional `pin`); coordinates are
monitor-relative unless `absolute` is set. For a newline, send `peet_key` with `enter` rather
than a `\n` in `peet_type`.

### Tray app

`peet-tray` sits in the notification area of the host machine. Launched, it starts a console host
(windowless, logging to `%LOCALAPPDATA%\PeetConnect\host.log`) unless one is already running;
`--no-host` only watches. The icon shows the state: red when accepting, a green dot while someone
is viewing, an amber pause badge while paused, grey when no host is running. Its menu lists
connected viewers (click one to disconnect it; the viewer is told and does not reconnect), and has
Pause new connections, Start/Stop/Restart host, Open configuration (creates a commented
`host.toml` if there is none), Open host log, Start with Windows (a per-user Run key) and Quit,
which also stops a host the tray started. Windows notifications announce viewers connecting,
leaving, and connections refused.

The tray reads allow rules from `%APPDATA%\PeetConnect\host.toml`, not from flags, so set at
least one `[allow]` rule there. It also works with the service: the service's worker serves the
same control pipe, and the tray shows "(service)" and offers no Stop.

### Unattended access (Windows service)

For a machine you want to reach when nobody is signed in, install the host as a service. It runs
as LocalSystem and launches a worker into the active console session, which is what lets it
capture and inject across sign-in, the lock screen and UAC prompts.

```
scripts\install-service.ps1 -AllowUser you@example.com      # installs and starts (prompts for admin)
scripts\install-service.ps1 -Uninstall                      # stops and removes
```

The allow rules are baked into the service command line; reinstall to change them. Under the
hood the script runs `peet-host --install <flags>` elevated and `sc start PeetConnect`.

The service path is implemented but unverified: the development machine has no administrator
rights to install a service, and the secure-desktop and session-switch behaviour needs a locked
machine to exercise. The interactive host (`--console`, the default) is the tested mode.

## Development

```
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

CI runs the same on Windows and Ubuntu with warnings denied. `Cargo.lock` pins `libc` to 0.2.189:
GPUI's Linux build pulls `xattr 0.2`, which does not compile against newer `libc`; keep the pin
when running `cargo update` until GPUI drops that dependency.

The app icon lives in `assets/icon.svg` (with a flat `assets/icon-small.svg` for the 32 px and smaller frames). After editing either, run `cargo xtask icons` and commit the regenerated `.ico` and `.png`; the build embeds the committed `.ico`.

## Licence

MIT. See [LICENSE](LICENSE).
