# PeetConnect — Architecture and Plan

Remote desktop (AnyDesk/RustDesk class), optimised for latency, UX and automation.
Windows host + viewer first; Linux and Android later. MIT licensed, clean room.

## Decisions (2026-11-12)

| Topic | Decision |
|---|---|
| Language | Rust end to end |
| Viewer UI | GPUI (Zed's UI framework) for chrome. Video is presented by our own D3D11 swap chain on a DirectComposition visual attached beneath GPUI's window, showing through a transparent region (decided in M2; GPUI's image path and plain child HWNDs both ruled out) |
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

**M2 — Loopback viewer + GPUI spike. Done 2026-11-12.** `peet-codec` gained a Media Foundation
H.264 decoder with DXVA output (NV12 D3D11 textures) and a general `GpuConverter`
(convert/scale between BGRA and NV12, blit into any render target). `peet-capture` gained
`FrameSource` (DXGI with GDI watchdog fallback, shared by bench and viewer). `peet-viewer`
is a GPUI app running capture→encode→decode→present in-process with two presenters.

Measured (RTX 4070 SUPER, GDI capture at ~25 ms is the bottleneck, release build):

| Presenter | 1080p | 4K (upscaled) | Verdict |
|---|---|---|---|
| GPUI `img` element (NV12→BGRA→staging readback→`RenderImage`) | 7–9 ms/frame, pipeline drops to 22 fps, +25 ms UI latency | 23–40 ms/frame, pipeline drops to 10 fps | rejected |
| Child HWND with own swap chain | never visible | never visible | rejected: GPUI's window is `WS_EX_NOREDIRECTIONBITMAP`, children have no surface |
| **DirectComposition visual under GPUI** (NV12→BGRA by video processor→copy to back buffer) | 0.1 ms/frame, 30 fps (capture-bound) | 0.1 ms/frame, 30 fps | **chosen** |

Decode costs 0.25 ms per frame at both sizes (DXVA). Capture-to-present is capture + encode +
one vsync. GPUI renders its chrome at 60 fps over the video; input goes to GPUI as normal.

Design that follows: the viewer opens its GPUI window with a transparent background, leaves
the video element unpainted, publishes the HWND and the element's physical-pixel bounds, and
the video thread owns a composition target (`topmost = false`; GPUI holds the topmost one),
a visual and a flip-model swap chain sized to the bounds. Multi-monitor views are more
visuals on the same target.

**M3 — Network. Done 2026-11-12.** `peet-transport` (quinn 0.11, rustls/ring, self-signed
cert per host start, any-cert client verifier because Tailscale is the trust layer, one tag
byte per stream, length-prefixed postcard frames). `peet-core` (client session: handshake,
control writer/reader, ping/RTT, per-stream video readers, screenshot request/response,
lazily opened input stream). `peet-host` (whois authorisation with allow-list by user, tag,
node or whole tailnet, optional PIN, per-monitor capture+encode thread feeding a
unidirectional QUIC stream, skip-capture-when-queue-full with AIMD bitrate control via
`ICodecAPI` mid-stream, keyframe on request, cursor position+shape out of band, PNG
screenshots on a blocking thread, TOML config). Viewer `--connect` mode decodes on its own
thread, requests a keyframe on sequence gaps or decode errors, draws the remote cursor as a
GPUI image mapped through the presenter's letterbox, and switches monitors on keys 1-9.
`peet monitors` and `peet screenshot` exercise the same core headlessly.

Verified on one machine by connecting to its own Tailscale IP (tailscaled answers `whois`
for the local node): 1080p at 58 fps viewer-side, ~5 Mbit/s, 1.1 ms decode, under 1 ms RTT.
Loopback (127.0.0.1) is rejected unless `--allow-local`. Not yet verified: a second tailnet
machine, real packet loss, and the keyboard monitor switch (UI automation could not focus
the GPUI window; the code path is the same `SetStream` the CLI exercises).

Interesting: DXGI duplication delivered frames to `peet-host` on the dev box at 54 fps, where
`peet-bench` saw none in M1. The watchdog fallback stays.

**M4 — Input + service.** (next) SendInput injection, Windows service as SYSTEM, per-session helper,
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
- GPUI has no surface/texture element on Windows and its window has no redirection bitmap:
  video must go through DirectComposition (see M2). Do not try child HWNDs again.
- DXGI duplication can silently deliver nothing (seen on the dev box). Always keep a GDI
  fallback behind a watchdog, and consider Windows.Graphics.Capture.
- Hardware MFTs are per-vendor; on multi-GPU machines try each until one accepts the device.
- openh264 ships as a Cisco binary for the patent licence; bundling must follow their terms.
