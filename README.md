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
| `bins/peet` | Command line. |
| `bins/peet-bench` | Capture and encode benchmarks. |
| `bins/peet-host` | Host: serves monitors to authorised tailnet viewers. Plain user process for now. |
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
```

Coordinates are relative to the chosen monitor (`--monitor`, default primary) unless
`--absolute` is given. The service that injects into the lock screen and other secure desktops
is still to come; today the host runs as a normal process in the interactive session.

## Development

```
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

CI runs the same on Windows and Ubuntu with warnings denied.
