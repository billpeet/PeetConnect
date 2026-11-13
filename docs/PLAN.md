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
  peet-tray         User tray app: status, sessions, notifications
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

**M4 — Input + service.** (in progress)

Done: `peet-input` injects mouse move/button/wheel, scancode keys and Unicode text via
`SendInput`, with a layout-independent key-name table. The host injects every event from the
input stream on a dedicated blocking thread. `peet-core` gained an input flush barrier so
one-shot senders can be sure events reached the wire before closing. The CLI gained
`move`/`click`/`scroll`/`type`/`key` for headless control. The viewer forwards mouse and keyboard,
mapping pointer positions through the inverse of the presenter letterbox; `Ctrl+Alt+1..9` switch
monitors locally, all other keys reach the host. The Windows service (`--service`,
`--install`/`--uninstall`, `scripts/install-service.ps1`) runs as LocalSystem and launches a
worker (`--session-worker`) into the active console session via `CreateProcessAsUser`, respawning
on session change; worker capture and input threads attach to the active input desktop.

Verified: injection end to end against the dev box's own Tailscale IP — a synthetic click
dismissed a firewall dialog and typing/key chords landed in Notepad. Unicode text is paced per
character because the Win11 Notepad RichEdit coalesces a fast `VK_PACKET` burst into a repeat of
the last character. The viewer still renders at 60 fps with input wired.

Not verified: the service at runtime (the dev box has no administrator rights to install one),
secure-desktop capture/injection and the session switch (need a locked machine), and the
viewer's interactive input (the GPUI window cannot be focused from the tool session). Exit
criterion — unattended control of a locked machine — is written but untested.

**M5 — Agent surface.** Done 2026-11-13.

Done: clipboard flows over the control stream — `SetClipboard`/`GetClipboard`/`ClipboardText`
messages, `Control::set_clipboard`/`get_clipboard` in peet-core, host read/write of
`CF_UNICODETEXT` on a blocking thread, and `peet clipboard <host> [--set <text>]`. The `peet mcp`
subcommand runs a Model Context Protocol server on stdio (hand-rolled JSON-RPC 2.0, newline
framed) exposing ten tools: `peet_peers`, `peet_monitors`, `peet_screenshot` (returns a PNG
image content block), `peet_move`, `peet_click`, `peet_scroll`, `peet_type`, `peet_key`,
`peet_clipboard_get`, `peet_clipboard_set`. The server is the local control API and the headless
session store in one: it holds a live QUIC session per host across tool calls (a background task
drains host events so the control reader never stalls) and reconnects lazily on failure. All of
it sits on peet-core, so the CLI, GUI viewer and MCP behave identically.

Verified against the dev box's own Tailscale IP: `initialize`/`tools/list`/`tools/call` all
answer; `peet_screenshot` returns a decodable PNG; clipboard get/set round-trip the real Windows
clipboard including Unicode; and a full headless input loop (click to focus Notepad, `ctrl+a`,
`delete`, `peet_type`) landed the typed Unicode text in the window, confirmed by screenshot.

Note: `peet_type` injects `\n` as a literal Unicode newline, which Notepad does not treat as
Enter; agents send `peet_key enter` for a line break (same semantics as M4 text injection).

Exit met: an agent can drive a remote session with no GUI.

**M6 — Daily-driver UX.** (in progress)

Done: *Reconnect* — the viewer runs sessions back to back with 1/2/4/8/10 s backoff after a
session that once worked is lost; the decode thread survives and resets on a per-connection
generation; the chosen monitor is restored; a host that never answered or rejected us goes
straight back to the home screen with the error. QUIC idle timeout is 6 s (keep-alive 2 s) so a
dead host is noticed quickly. *Clipboard sync* (text) — the host watches the clipboard sequence
number and pushes changes as an unsolicited `ClipboardText`; the viewer writes them locally and
pushes its own changes while active; one last-seen value guards both directions against echo.
*Home screen* — live tailnet device list (3 s refresh via LocalAPI), favourites (star toggle),
recents, an address box, click-to-connect; `Ctrl+Alt+D` or a status-bar button disconnects back
to it. Settings persist in `%APPDATA%\PeetConnect\viewer.toml` (fps, bitrate, favourites,
recents). `--loopback` now selects the M2 test rig; no arguments opens the home screen.
*Multi-monitor layout* — `Ctrl+Alt+0` streams every monitor at once, laid out as on the host
(uniformly scaled, centred); `Ctrl+Alt+<n>` goes back to one. The presenter keeps its single
composition swap chain and gives each visible monitor its own GPU converter whose output lands
in that monitor's tile; every present clears the back buffer and composites the latest tile of
each monitor. The presenter publishes the tile rectangles, and the UI maps pointer positions and
places the remote cursor through them (outside every tile the nearest one is used, clamped), so
input works across monitors. The decoder keeps one H.264 decoder per visible monitor.

Verified on the dev box's own Tailscale IP: kill-and-restart of the host reconnected in ~11 s with
a rebuilt decoder; a host-side clipboard change arrived at the viewer; the home screen was driven
end to end by injected input (click the address box, scancode-type the IP, Enter → 60 fps session,
`Ctrl+Alt+D` → home with the new recent); `Ctrl+Alt+0` showed both dev-box monitors side by
side at 60 fps and `Ctrl+Alt+2` returned to one (second decoder created, first stream disabled on
the host). Finding: GPUI ignores `VK_PACKET` key events, so
`peet type` cannot type into a GPUI window (use `peet key` scancodes); physical keyboards are
unaffected. Injected clicks do activate the GPUI window, which lifts the old "cannot focus the
viewer from automation" limit for keyboard testing.

**Second machine, 2026-11-13.** First real cross-machine test: release binaries Taildropped to
DESKTOP-ER4U7GP (Windows 11, AMD iGPU + RTX 4060 Ti, three monitors: 3840x2160 at 150%, 1920x1080,
and a 1080x1920 portrait), host started there as `--console` by a non-admin user with no firewall
rule added. From FUTURA-GPC over Tailscale (rtt ~18 ms): `peet monitors` and PNG screenshots of all
three monitors worked with no firewall prompt; clipboard set/get round-tripped; `win+r`, typing
and Enter opened Notepad on the remote and a paced Unicode sentence landed intact; the viewer
decoded the 4K primary (DXGI + NVIDIA MFT on the host, 0.24 ms decode here) and `Ctrl+Alt+0` laid
all three monitors out correctly including the portrait one, each stream on DXGI + NVENC. Fix that
came out of it: QUIC path-MTU probing produced WSAEMSGSIZE warnings on both ends because
Tailscale's MTU is 1280; probing is now disabled and the 1200-byte initial MTU is kept.
Observed but not yet tuned: the 4K stream ran at ~19 fps host-side on that machine.

*Polish (2026-11-13)* — a connecting/reconnecting card over the video with a cancel button
(Escape also cancels while not yet connected); host rejections rendered in plain words with the
reject reason carried in `RemoteStatus`, and a PIN box on the home screen that a `PinRequired` or
`PinIncorrect` answer activates (Tab switches boxes, Enter retries); frame-rate and bitrate preset
chips on the home screen, persisted; a session bar with clickable monitor pills, "all",
fullscreen and disconnect; `Ctrl+Alt+F` fullscreen with the bar hidden until the pointer touches
the top edge; the window title names the host. Verified on the dev box by driving the viewer
through the host: PIN rejection, PIN retry, pills, fullscreen reveal, host kill showing
"Reconnecting" and recovering, disconnect to home.

*App icon (2026-11-13)* — `assets/icon.svg`: a P whose bowl is a screen, on the red rounded square, drawn on a 16 px grid with a flat variant for small frames. `cargo xtask icons` renders a 10-frame `.ico` (16–256) with resvg; `peet-winres` embeds it as icon resource 1 (the ID GPUI's window class loads, so it reaches the taskbar and Alt+Tab) plus VERSIONINFO in the viewer, host and CLI. Verified: Explorer icon, window-class icon and file-properties description on all three exes.

*Host tray app (2026-11-13)* — `peet-control` is the local control API from the plan: JSON lines
over `\\.\pipe\peetconnect-host` (protected DACL: SYSTEM, administrators, owner, interactive
users; no remote clients; first-instance only). The serving process (console host or service
worker) pushes a full `HostStatus` on every change plus `Notice`s (connected, disconnected,
rejected) and accepts `Disconnect`, `SetPaused`, `Shutdown` (console only). `peet-tray` is plain
Win32: hidden top-level window, `Shell_NotifyIcon` v4 with state icons (resources 2-4 generated by
`cargo xtask icons`), `TrackPopupMenu`, balloon notifications, HKCU Run key; a control-link thread
reconnects to the pipe and posts updates to the window. It starts `peet-host --console` itself if
no host appears within 1.5 s. Protocol: `RejectReason::Paused`; a host-initiated `Disconnect` now
reaches peet-core consumers as `Event::EndedByHost`, which the viewer treats as final rather than
reconnecting. Verified on the dev box against its own Tailscale IP: tray auto-started the host,
status and notices over the pipe, pause rejecting a new viewer with `Paused`, and a pipe
`Disconnect` ending a live viewer session with "the host ended the session" and no reconnect. Not
verified by the agent: the menu and notifications on screen (the session cannot drive the shell
without disturbing the user). Known: a one-shot CLI exits before its QUIC close frame leaves, so
the host sees it leave only at the 6 s idle timeout.

Not done: file transfer, per-monitor bitrate budgeting in all-monitors mode (each
stream keeps the full cap), remembering a PIN per host. Exit — replaces AnyDesk for personal use — is close for the viewer side; the service still
needs an administrator to verify unattended use (neither test machine's user is one).

**M7 — Packaging, releases and updates.** (in progress) One Inno Setup installer for all binaries,
per-user by default and per-machine with the service when run as administrator; tag-triggered
release builds on GitHub Actions; a signed update manifest on GitHub Releases, applied
automatically when idle. Plan: [PACKAGING.md](PACKAGING.md).

*Work items 1–2 (2026-11-13)* — Versions: every binary already had `--version` except the tray,
which has no console, so its menu header shows its version instead (and the host's when they
differ). Service configuration: the service and its worker read
`%ProgramData%\PeetConnect\host.toml` and log to `%ProgramData%\PeetConnect\logs` (`service.log`,
`host.log`); `HostStatus.log_path` tells the tray. `--install` stores only `--service` (and an
explicit `--config`) in the service command line, writes the given flags into a new file, and
refuses flags when the file already exists. The folder is reset to Administrators-owned with
SYSTEM/Administrators full control and Users read (`TreeResetNamedSecurityInfoW`, so a folder a
user pre-created is taken over), and `host.toml` is SYSTEM/Administrators only because it may hold
the PIN; the service repairs this at every start. Tray: "Open configuration" opens Notepad
elevated and "Restart service" runs an elevated `Restart-Service` when the host is the service.
Verified: config round-trip test; unelevated `--install` fails cleanly at taking ownership; a
hand-started `--session-worker` with `ProgramData` redirected read the machine file, logged to
`logs\host.log` and reported both paths over the pipe. Not verified: an elevated install and the
real service (no administrator on either test machine).

*Work item 3 (2026-11-13)* — `packaging/peetconnect.iss` (Inno Setup 6.7) and `cargo xtask dist`
(release build of the four shipped binaries, `target/dist/stage`, a portable zip via bsdtar, and
ISCC with the workspace version; `--no-build`, `--no-installer`; CI runs the latter). Installer:
8.6 MB. Per-user by default, per-machine through Inno's install-mode dialog; tasks for sign-in
start (the tray's own Run value), PATH (HKCU or HKLM `Environment`, removed exactly on
uninstall) and, per-machine only, the service with an allowed-logins/PIN page (or
`/ALLOWUSERS=` and `/PIN=` silently) that is skipped when `host.toml` already exists. Before
copying, it posts `WM_CLOSE` to the tray window (which stops the host it started) and stops the
service; Restart Manager (`CloseApplications=force`) handles anything else. `/UPDATE` keeps earlier
choices and does not re-apply tasks, so a sign-in entry turned off in the tray stays off. A
per-machine install offers to remove a per-user copy first; a per-user install refuses while a
per-machine one exists. It offers the Tailscale download page when Tailscale is missing.
Uninstall stops the tray and service, removes the service, the Run value (when it points at this
install) and the PATH entry, and asks before deleting settings. The tray now handles
`WM_ENDSESSION` by stopping its host before it exits. Verified per-user on the dev box, all
silent: fresh install over a running dev tray (closed, installed tray relaunched, files,
shortcuts, Run value, PATH, uninstall key), `/UPDATE` over the running installed tray (closed
before Restart Manager looked, relaunched, removed Run value not restored), uninstall (folder,
key, shortcuts, Run value and PATH exactly restored, `host.toml` kept). Not verified: the
interactive wizard pages and the per-machine mode with the service (no administrator).

*Work item 4 (2026-11-13)* — `release.yml` (tag `v*` → check tag, Inno Setup, `xtask dist`,
installer smoke test, signed manifest, provenance attestation, draft release; pre-release versions
marked prerelease). `cargo xtask release <version>` (version, lock file, CHANGELOG, commit, tag),
`manifest` (`latest.json` + ed25519 `latest.json.sig` + `SHA256SUMS`, checked against the committed
`packaging/update-key.pub`), `keygen` (will not replace an existing public key), `check-tag`,
`notes`. Update key generated 2026-11-13; private key kept outside the repository. Verified
locally: manifest signed and verified with the real key, a wrong key rejected, the release command
dry-run in a throwaway worktree (commit, tag, version, lock file, CHANGELOG, notes; older version
refused). Not yet run on GitHub.

**Later.** Audio, multi-viewer, relay + ICE for non-Tailscale, HEVC/AV1, Linux host (PipeWire),
Android viewer.

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
  Decided 2026-11-13: release builds leave it out (off-by-default `peet-codec` feature
  `openh264`, enabled by `peet-bench` only); see PACKAGING.md.
