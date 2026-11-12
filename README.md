<img src="assets/icon.svg" width="64" height="64" alt="">

# PeetConnect

Fast, lightweight remote desktop built for Tailscale. Rust end to end, MIT licensed.

Status: early development. See [docs/PLAN.md](docs/PLAN.md) for architecture and milestones.

## Layout

| Path | What |
|---|---|
| `crates/peet-proto` | Wire protocol types and framing. No platform code. |
| `crates/peet-tailscale` | Client for the local tailscaled API: peer discovery and `whois` identity. |
| `bins/peet` | Command line. |

## Try it

Requires a Rust toolchain and a running Tailscale client.

```
cargo run -p peet -- peers          # online tailnet devices
cargo run -p peet -- peers --all    # include offline
cargo run -p peet -- whoami
cargo run -p peet -- whois 100.x.y.z
cargo run -p peet -- --json peers   # machine-readable
```

## Development

```
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

CI runs the same on Windows and Ubuntu with warnings denied.
