# PeetConnect — Architecture and Plan

Remote desktop (AnyDesk/RustDesk class), optimised for latency, UX and automation.
Windows host + viewer first; Linux and Android later. MIT licensed, clean room.

## Decisions (2026-11-12)

| Topic | Decision |
|---|---|
| Language | Rust end to end |
| Viewer UI | GPUI (Zed's UI framework). Video surface risk mitigated by a dedicated D3D11 child HWND if GPUI's image path can't hold 60 fps at 4K (spike in M2) |
| Transport | QUIC (quinn) over Tailscale only in v1. No relay, no NAT traversal. Relay is a later, optional component |
| Auth | Tailscale identity via LocalAPI `whois` on the peer address, plus an allow-list (tailnet users / tags / node IDs) and optional per-host PIN. No account system of our own |
| Unattended access | In v1. Host runs as a Windows service (SYSTEM) with a per-session helper for capture, input, clipboard and secure desktop |
| Codec | H.264 hardware encode/decode via Media Foundation (NVENC/AMF/QSV through MF transforms), openh264 software fallback (BSD). Lossless PNG/zstd path for screenshots. HEVC/AV1 negotiated later |
| Agent-friendliness | Local control API (named pipe / Unix socket, token-authed), `peet` CLI, MCP server, headless viewer that can screenshot and send input without a window |
| Audio | Deferred |
| Multi-monitor | Priority. Each monitor is its own stream; viewer can show one or all |
| Multi-viewer per host | Low priority, but protocol must not preclude it |
| Audience | Personal first, distributable later: installer, code signing and auto-update designed for but not built in v1 |
| Licensing | MIT. No AGPL/GPL deps (no scrap, hbb_common, x264, ffmpeg-GPL builds) |

## Topology

```
 Viewer machine                               Host machine
 ┌──────────────────────┐                     ┌────────────────────────────────┐
 │ peet-viewer (GPUI)   │                     │ peet-host service (SYSTEM)     │
 │ peet CLI / MCP       │── QUIC/Tailscale ──▶│   ├─ listener + auth (whois)   │
 │ peet-core (client)   │   udp :7735         │   ├─ session manager           │
 └──────────────────────┘                     │   └─ spawns per-session helper │
                                              │ peet-host-helper (user/winlogon│
                                              │   desktop): capture, encode,   │
                                              │   input, clipboard, cursor     │
                                              │ peet-tray (user): status UI    │
                                              └────────────────────────────────┘
```

Service ↔ helper communicate over a local named pipe with shared-memory frame handoff
(or the helper owns the QUIC connection's media streams directly; decide in M4).

## Crate layout (Cargo workspace)

```
crates/
  peet-proto      Wire protocol: message types, versioning, codec negotiation (serde + postcard
                  or prost). Shared by every binary. No platform code.
  peet-tailscale  Tailscale LocalAPI client (status, whois) over named pipe / Unix socket.
  peet-transport  QUIC session over quinn. Stream roles: control, video[n], input, clipboard,
                  file, audio(reserved).
  peet-capture    trait Capturer { monitors(), next_frame() -> Frame{texture|cpu, dirty, cursor} }
                  windows: DXGI Desktop Duplication; GDI fallback. linux/android later.
  peet-codec      trait Encoder / Decoder. windows: Media Foundation H.264 HW. openh264 fallback.
                  PNG/zstd lossless for stills.
  peet-input      trait InputInjector. windows: SendInput, layout/modifier handling, DPI mapping.
  peet-platform   Windows service plumbing, session enumeration, CreateProcessAsUser,
                  desktop switching (SetThreadDesktop), token handling.
  peet-core       Client-side session engine: connect, negotiate, decode, present callbacks,
                  input send, clipboard, file transfer. UI-agnostic; used by viewer, CLI, MCP.
  peet-control    Local control API (named pipe) and its typed client. Used by CLI, MCP, tray.
bins/
  peet-host         Windows service + `--console` mode for dev
  peet-host-helper  Per-session capture/input process
  peet-tray         User tray app (later)
  peet-viewer       GPUI app
  peet              CLI: list peers, connect, screenshot, input, clipboard, file, status
  peet-mcp          MCP server wrapping peet-core / peet-control
```

## Protocol sketch

- Handshake on control stream: version, capabilities (codecs, monitors, features), auth result.
- Video: one unidirectional QUIC stream per monitor; frames are `{monitor_id, seq, pts, keyframe, codec, dirty_rects, data}`.
  Keyframe on request. Adaptive bitrate from QUIC RTT/loss stats.
- Cursor: separate small messages (shape + position), never baked into video.
- Input: datagram or ordered stream (ordered — key order matters). Absolute coords in host monitor space.
- Clipboard: text first, images/files later. File transfer: own stream, chunked, resumable.
- Still capture: `Screenshot{monitor, format: png|jpeg|raw}` request/response for agents, independent of the video stream.

## Milestones

Each milestone ends with something runnable and testable alone.

**M0 — Skeleton. Done 2026-11-12.** Workspace, CI (fmt, clippy, test on Windows + Ubuntu),
MIT licence, `peet-proto` types + framing, `peet-tailscale` LocalAPI client, `peet` CLI with
`peers`, `whoami`, `whois`, `version` (all with `--json`). Exit met: `peet peers` lists tailnet devices.

Notes from M0: on Windows the LocalAPI named pipe must be opened with `SECURITY_IMPERSONATION`
or tailscaled returns 401 (tokio's default is identification-only). `whois` returns 404 for
non-tailnet addresses; the host treats that as "not authorised".

**M1 — Capture + encode, offline. Done 2026-11-12.** `peet-capture` (DXGI Desktop Duplication
via `DuplicateOutput1`, GDI fallback with GPU upload, monitor enumeration with DPI/adapter),
`peet-codec` (Media Foundation H.264 hardware/software with async-MFT event loop and D3D11
zero-copy via video-processor BGRA→NV12, openh264 fallback, CPU NV12 conversion),
`peet-bench` (per-monitor capture+encode benchmark with animated stimulus windows, raw
duplication probe, encoder listing). Exit met: playable .h264 per monitor, verified by decoding.

Results on the dev box (RTX 4070 SUPER, 1080p per monitor, release build):

| Path | capture | encode | notes |
|---|---|---|---|
| GDI → GPU upload → NVENC (zero-copy NV12) | ~14–17 ms | ~3.5 ms | 2 monitors in parallel at 30+ fps, 1.5% total CPU |
| GDI → CPU NV12 → Microsoft H264 MFT | ~20 ms | ~6 ms | software encoder is fine for 1080p |
| GDI → CPU NV12 → NVENC (system memory) | ~20 ms | ~7 ms | |
| GDI → openh264 | ~20 ms | ~35 ms | no NASM; fallback only |

**Open issue from M1:** on the dev box DXGI Desktop Duplication delivers *no frames at all*
(every `AcquireNextFrame` times out, including the first), for both our code and an
independent implementation (Python `dxcam`). GDI works, so this is machine/driver/OS state
(Windows 11 26H2 build 26300, NVIDIA 32.0.16.1692), not a code bug. Consequences: the host
needs a capture watchdog that falls back to GDI (the bench already does this), and we should
evaluate Windows.Graphics.Capture as a third backend. DXGI timings are therefore still
unmeasured here; expect ~1 ms per frame when it works.

**M2 — Loopback viewer + GPUI spike.** (next) Decode and present in a GPUI window over an in-process
channel. Measure present path at 4K60. Decide: GPUI image element vs D3D11 child HWND. Exit:
60 fps local mirror with latency figure.

**M3 — Network.** QUIC over Tailscale, whois auth + allow-list, codec negotiation, cursor,
adaptive bitrate, keyframe recovery. Host runs as a plain user process (`--console`). Exit:
remote view between two tailnet machines.

**M4 — Input + service.** SendInput injection, Windows service as SYSTEM, per-session helper,
secure desktop (lock screen, UAC) capture and input, session switch on logon/logoff, installer
script. Exit: unattended control of a locked machine.

**M5 — Agent surface.** Local control API, `peet` CLI (connect, screenshot, click, type, key,
clipboard), headless viewer mode, MCP server. Exit: an agent can drive a remote session with
no GUI. This can start in parallel with M4 since it sits on peet-core.

**M6 — Daily-driver UX.** Multi-monitor layout, clipboard, file transfer, connection list with
favourites, tray app, settings, reconnect. Exit: replaces AnyDesk for personal use.

**Later.** Audio, multi-viewer, relay + ICE for non-Tailscale, HEVC/AV1, Linux host (PipeWire),
Android viewer, signing + auto-update, distribution.

## Known hard parts

- Secure desktop: DXGI duplication must run from a process on the desktop being captured.
  The service launches the helper with the console session token and switches desktops as needed.
- DPI and resolution changes mid-session (DXGI returns ACCESS_LOST; reinit, send new monitor layout).
- Keyboard: send scancodes not VKs where possible; handle layout mismatch and stuck modifiers on focus loss.
- GPUI on Windows: API churn and sparse docs. Pin a version; isolate UI from peet-core.
- DXGI duplication can silently deliver nothing (seen on the dev box). Always keep a GDI
  fallback behind a watchdog, and consider Windows.Graphics.Capture.
- Hardware MFTs are per-vendor; on multi-GPU machines try each until one accepts the device.
- openh264 ships as a Cisco binary for the patent licence; bundling must follow their terms.
