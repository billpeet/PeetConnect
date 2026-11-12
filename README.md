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
| `crates/peet-codec` | H.264 encode (Media Foundation hardware/software, openh264 fallback) and decode (Media Foundation + DXVA), GPU colour conversion. |
| `bins/peet` | Command line. |
| `bins/peet-bench` | Capture and encode benchmarks. |
| `bins/peet-viewer` | GPUI viewer. Currently a loopback test rig: capture, encode, decode and present locally. |

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

## Development

```
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

CI runs the same on Windows and Ubuntu with warnings denied.
