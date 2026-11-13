# Changelog

User-visible changes per release. Add notes under "Unreleased" as you go;
`cargo xtask release <version>` moves them under the new version.

## Unreleased

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
