<img src="assets/icon.svg" width="64" height="64" alt="">

# PeetConnect

Fast, lightweight remote desktop built for Tailscale. Rust end to end, MIT licensed.

Status: early development. See [docs/PLAN.md](docs/PLAN.md) for architecture and milestones.

## Layout

| Path | What |
|---|---|
| `crates/peet-proto` | Wire protocol types and framing. No platform code. |
| `crates/peet-tailscale` | Client for the local tailscaled API: peer discovery and `whois` identity. |
| `crates/peet-capture` | Screen capture: DXGI Desktop Duplication with GDI fallback (Windows). |
| `crates/peet-transport` | QUIC (quinn) endpoints, self-signed TLS, tagged framed streams. |
| `crates/peet-core` | Client core: connect, handshake, control, video events, screenshots. Shared by viewer, CLI and (later) MCP. |
| `crates/peet-codec` | H.264 encode (Media Foundation hardware/software, openh264 fallback) and decode (Media Foundation + DXVA), GPU colour conversion. |
| `crates/peet-input` | Synthesises mouse and keyboard input with `SendInput`; layout-independent key table. |
| `bins/peet` | Command line, and the `mcp` subcommand: an MCP server exposing hosts as agent tools. |
| `bins/peet-bench` | Capture and encode benchmarks. |
| `bins/peet-host` | Host: serves monitors to authorised tailnet viewers; injects input; runs as a console process or a LocalSystem service. |
| `bins/peet-viewer` | GPUI viewer: `--connect <host>` for a remote session, or a loopback test rig. |

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
cargo run --release -p peet-viewer -- --present d3d            # loopback mirror of the primary monitor
cargo run --release -p peet-viewer -- --present d3d --upscale 3840x2160 --bitrate-kbps 30000
cargo run --release -p peet-viewer -- --present gpui           # the rejected image-element path, for comparison
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
cargo run --release -p peet-viewer -- --connect <magicdns-name-or-100.x.y.z>
cargo run --release -p peet -- monitors <host>                      # headless: list monitors
cargo run --release -p peet -- screenshot <host> -o shot.png        # headless: PNG still
```

The viewer forwards mouse and keyboard to the host. `Ctrl+Alt+1`..`9` switch monitors locally;
every other key reaches the remote machine. The host injects input with `SendInput` and so
drives whatever has focus in the interactive session.

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

CI runs the same on Windows and Ubuntu with warnings denied.
