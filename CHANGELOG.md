# Changelog

User-visible changes per release. Add notes under "Unreleased" as you go;
`cargo xtask release <version>` moves them under the new version.

## Unreleased

### Added

- Single-window streaming. The viewer's "window ▾" chip (or Ctrl+Alt+W) lists the host's
  windows; pick one to stream just that window instead of a monitor. Moves are followed, a
  minimised or closed window is shown as such, and input brings the window to the front.
  "Match size" resizes the host window to the viewer's, or the viewer to the window.
- `peet windows`, `peet focus`, `peet resize` and `peet screenshot --window` on the command
  line, and the same in the MCP server (`peet_windows`, `peet_focus_window`,
  `peet_resize_window`, `peet_screenshot` with `window`).
- Host settings under `[video]`: `window_popups` (include the app's menus and popups, default
  on) and `window_capture = "crop"` (cut the window out of the monitor capture).
- The host wakes the displays and keeps them on while a viewer is connected
  (`[video] wake_display`).

### Changed

- Protocol version 2. 0.3.0 cannot talk to 0.2.x, so update the host and the viewers together.
  Installed copies update themselves, but a machine that has not updated yet cannot connect
  until it does.
- Windows Graphics Capture is used for monitors when Desktop Duplication is unavailable, before
  falling back to GDI. Still frames are sent as keyframes, so a quiet screen recovers from
  packet loss.
- The unattended-access service streams windows by cutting them out of the monitor capture,
  because Windows Graphics Capture does not work for the SYSTEM account.

## 0.2.1 - 2026-11-13

### Added

- Automatic updates. Installed copies check GitHub for a newer signed release, download it in
  the background and install it silently once no viewer is connected and no viewer window or
  `peet` command is running. Per-user installs are updated by the tray, which can switch to
  notify-only ("Install updates automatically"); per-machine installs are updated by the service
  (`[update] automatic = false` in host.toml to wait instead). `peet update` installs now and
  `peet update --check` only reports. Installs of 0.2.0 have to be updated by hand once.

## 0.2.0 - 2026-11-13

First packaged release.

### Added

- Windows installer (`PeetConnect-<version>-setup.exe`) with the viewer, host, tray app and
  `peet` command-line tool. Installs for the current user without administrator rights, or for
  everyone (as administrator) with the optional unattended-access service. A portable zip is
  attached too.
- Viewer: home screen with tailnet devices, favourites and recent connections; several monitors
  at once, laid out as on the host; hardware H.264 decode presented through DirectComposition.
- Host: authorises viewers by Tailscale identity (users, ACL tags, nodes) with an optional PIN;
  hardware H.264 encode; mouse, keyboard and clipboard; runs as a console app or a Windows
  service that follows the active session.
- Tray app: status icon, connected viewers with disconnect, pause new connections,
  connect/disconnect notifications, start with Windows.
- `peet` command line and MCP server so agents can see and drive a host without a window.

### Requirements

- Windows 10 1809 or later, x64, with Tailscale. Windows N editions need the Media Feature Pack
  (Settings > Apps > Optional features) for H.264 encoding and decoding.
