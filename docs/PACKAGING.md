# PeetConnect — Packaging, Releases and Updates

Plan for Milestone 7. GitHub Actions builds every release, one Windows installer carries every
binary, and installed copies update themselves.

## Decisions (2026-11-13)

| Question | Decision |
| --- | --- |
| Install scope | **Hybrid.** One installer. By default it installs per-user with no admin, and includes everything except the service. Run as administrator, it installs per-machine into Program Files and can also install the unattended-access service. |
| Installer | **Inno Setup 6**, plus our own updater. Updates are full downloads; there are no delta packages. |
| Applying updates | **Automatic when idle**: download in the background, install silently once nothing is in use, then notify. The tray has a toggle to switch to notify-only. |
| Signing | **Unsigned for now, and apply to SignPath Foundation.** Updates are always verified against our own ed25519 signature, whether or not Authenticode signing is in place. |

Velopack was considered. It has a Rust crate, delta updates and a GitHub Releases feed. It was
turned down because it is built around a single app, its per-machine installs prompt UAC on every
update, and it has no service support, so the service would have needed custom handling either way.

## What ships

| File | Role | Size (release, uncompressed) |
| --- | --- | --- |
| `peet-tray.exe` | Notification-area app. In a per-user install it also owns the update check. | 0.8 MB |
| `peet-host.exe` | Host: console mode (started by the tray), service and service worker | 5.9 MB |
| `peet-viewer.exe` | GUI viewer | 12.3 MB |
| `peet.exe` | CLI and MCP server | 5.8 MB |

`peet-bench` stays out of the package. That is about 25 MB on disk and roughly 10 MB as an
installer. Only x64 is built; ARM64 can come later.

Tailscale is a prerequisite and is not bundled. The installer checks for `tailscale.exe` (registry
or PATH) and links to the download page if it is missing.

## Layout on disk

|  | Per-user (default) | Per-machine (admin) |
| --- | --- | --- |
| Program files | `%LOCALAPPDATA%\Programs\PeetConnect` | `%ProgramFiles%\PeetConnect` |
| Host config | `%APPDATA%\PeetConnect\host.toml` (unchanged) | `%ProgramData%\PeetConnect\host.toml` for the service; console hosts keep the per-user file |
| Logs | `%LOCALAPPDATA%\PeetConnect\*.log` (unchanged) | `%ProgramData%\PeetConnect\logs` for the service |
| Update staging | `%LOCALAPPDATA%\PeetConnect\updates` | `%ProgramData%\PeetConnect\updates` |
| Uninstall entry | HKCU | HKLM |
| CLI on PATH | user PATH | system PATH |

`%ProgramData%\PeetConnect` gets an explicit ACL: SYSTEM and Administrators have full control,
Users can read. Its `updates` folder is SYSTEM and Administrators only. Otherwise a standard user
could swap the installer between verification and launch, and the service would run it as SYSTEM.

## Installer (Inno Setup)

`packaging/peetconnect.iss`, built by `cargo xtask dist`.

- `PrivilegesRequired=lowest` and `PrivilegesRequiredOverridesAllowed=dialog commandline`. The
  first page asks "Only for me" or "Everyone on this computer", and the second choice elevates.
  `UsePreviousPrivileges=yes` (the default) makes reinstalls and updates reuse the earlier choice.
  The updater passes `/CURRENTUSER` or `/ALLUSERS` explicitly anyway.
- **Tasks:**
  - Start menu shortcuts for PeetConnect (the viewer) and PeetConnect Tray
  - Start the tray at sign-in (HKCU Run, the key the tray's own menu toggle already uses)
  - Add `peet` to PATH
  - Per-machine only: install the unattended-access service. This adds a page for the first
    allowed user (pre-filled from `tailscale status --json`) and an optional PIN, and writes
    `%ProgramData%\PeetConnect\host.toml`.
- **Running processes:** the installer posts `WM_CLOSE` to the tray (which stops the console host
  it started) and stops the service before copying files; Restart Manager
  (`CloseApplications=force`) closes anything else, such as an open viewer. Afterwards it
  starts the service again and relaunches the tray if it had been running, passing
  `--updated <old version>` after an update.
- **Uninstall:** stops the tray and host, removes the service, the PATH entry and the Run key.
  Configuration and logs stay unless the user ticks "Remove my settings".
- **Silent switches for the updater:** `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CURRENTUSER|/ALLUSERS /UPDATE`.
  `/UPDATE` is our own flag. It skips the task pages and keeps the existing task choices.
- **Both scopes on one machine:** the per-machine install refuses to run while a per-user install
  exists, and offers to uninstall it first, so the tray never ends up with two hosts fighting over
  port 7735.

`scripts/install-service.ps1` stays for development only. The installer calls
`peet-host service install` directly.

## Updates

### Release manifest

Every release carries `latest.json` and `latest.json.sig`:

```json
{
  "version": "0.2.0",
  "published": "2026-11-20T03:00:00Z",
  "notes": "https://github.com/billpeet/peetconnect/releases/tag/v0.2.0",
  "installer": {
    "url": "https://github.com/billpeet/peetconnect/releases/download/v0.2.0/PeetConnect-0.2.0-setup.exe",
    "sha256": "…",
    "size": 10485760
  }
}
```

- Clients fetch `https://github.com/billpeet/peetconnect/releases/latest/download/latest.json`.
  That URL serves the newest published, non-prerelease release, so drafts stay invisible until
  you press Publish.
- `latest.json.sig` is an ed25519 signature over the exact bytes of `latest.json`. The public key
  is compiled into the binaries. Verification uses `ring::signature::ED25519`, which is already in
  the dependency tree through rustls. The private key lives in the Actions secret
  `UPDATE_SIGNING_KEY`, with an offline backup.
- Clients accept a manifest only if its signature verifies and its version is newer than their
  own, which stops anyone replaying an older signed manifest. The downloaded installer must then
  match `sha256` and `size`. Authenticode is checked in addition once releases are signed.

### Who checks and installs

|  | Per-user install | Per-machine install |
| --- | --- | --- |
| Checks | the tray, 2 minutes after start and then every 6 h with jitter | the service, on the same schedule |
| Installs | the tray launches the installer detached with `/CURRENTUSER`, then exits | the service launches the installer as SYSTEM, detached and outside the service's job, with `/ALLUSERS` |
| Shows | tray menu "Update available / Installing…", balloon "Updated to 0.2.0" | the tray shows the service's update state from `%ProgramData%\PeetConnect\update.json`, and quits when it says `installing`; the restarted service starts it again |

`peet update` (CLI) checks and installs on demand, and `peet update --check` only reports.

### "Idle" means

- the host has no sessions, and
- no `peet-viewer.exe` or `peet.exe` runs from the install folder. A long-lived `peet mcp`
  started by an agent counts, because Windows will not let the installer replace a running exe.

The check repeats every 10 minutes until the machine is idle. After 3 days of waiting the tray
shows a notification naming what blocks the update, usually an agent's MCP server, and offers
"Install now", which closes those processes.

With notify-only switched on, the update is downloaded and verified, then waits for the user to
click "Install now" in the tray. For a per-machine install that is `[update] automatic = false` in host.toml, and "Install now" runs the installer with an administrator prompt.

### Failure handling

- A download or verification failure is logged and retried at the next check, with no
  notification unless it keeps failing for a day.
- Inno Setup has no rollback after a completed install. The previous installer stays in the
  staging folder until the new version has run for a day, so a bad update can be undone by
  running it again. A "Reinstall previous version" tray item can be added if this ever matters.

### Code

A new `crates/peet-update` crate handles the manifest fetch, signature and version check,
download into staging, hash check and the idle policy. It also launches the installer. The tray,
the service worker loop (`peet-host`) and the CLI call into it. For HTTPS it uses `ureq` with
rustls/ring, or WinHTTP through `windows` if that keeps binaries smaller. Decided: WinHTTP (system
TLS, certificates and proxy, nothing added to the binaries), with `ring` for the signature and hash.

## Versioning and releases

- One version for the whole workspace (`[workspace.package] version`), already embedded in every
  exe's VERSIONINFO by `peet-winres`. Every binary gets `--version`, and `HostStatus` carries the
  host's version.
- `cargo xtask release 0.2.0` bumps the version and moves CHANGELOG "Unreleased" under 0.2.0. It
  then commits and creates tag `v0.2.0`. Pushing is left to you.

### `.github/workflows/release.yml` (trigger: push of a tag `v*`)

1. Check that the tag matches the workspace version.
2. Run `cargo build --workspace --release --locked` on `windows-latest`.
3. Sign the exes through SignPath, once that is approved. Until then this step is skipped.
4. `cargo xtask dist` stages the files and runs `iscc`, producing
   `PeetConnect-<v>-setup.exe` and a portable `PeetConnect-<v>-x64.zip`. Inno Setup is installed
   with choco if the runner image lacks it.
5. Sign the installer, again once SignPath is approved.
6. Smoke test: silent per-user install on the runner, `peet --version`, check that
   `peet-host --console` binds, silent uninstall.
7. Write `latest.json` and sign it with `UPDATE_SIGNING_KEY`, and write `SHA256SUMS`.
8. `actions/attest-build-provenance` on the installer and zip.
9. Create a **draft** GitHub release with the notes from CHANGELOG. Publishing it is the manual
   gate, and it is the moment updates go out.

The existing `ci.yml` stays as it is. It also runs `cargo xtask dist --no-installer` so that
packaging breakage shows up before a release.

## Code signing

Releases ship unsigned until SignPath approves the project. Users then see SmartScreen's "Windows
protected your PC" and have to click "More info → Run anyway". Updates are not affected, because
they come through our own signed manifest.

What SignPath Foundation expects: an OSI licence (MIT ✓), a public repository, builds on GitHub-hosted
runners, 2FA, and a code signing policy section in the README with their attribution line. Each
release needs a manual approval in their dashboard, which fits the draft-release gate. The
certificate's publisher reads "SignPath Foundation", not your name. If they decline, the fallback
is an OV certificate with cloud signing (SSL.com eSigner or Certum), roughly A$300–600 a year.

## Work items

1. **Versions:** `--version` on every binary, the version in `HostStatus`, and the version in the
   tray menu header.
2. **Service config:** the service reads `%ProgramData%\PeetConnect\host.toml` instead of
   allow flags baked into its command line. `service install` creates the folder with the ACL
   above. The tray's "Open configuration" opens an elevated editor when the host is the service.
3. **Installer:** `packaging/peetconnect.iss` and `cargo xtask dist`, verified by hand for both
   scopes, upgrades from the previous version, and uninstall.
4. **Release workflow:** `release.yml`, `cargo xtask release`, the CHANGELOG, the
   `UPDATE_SIGNING_KEY` secret, and a first `v0.2.0` release.
5. **Updater:** `peet-update`, plus integration in the tray, the service and `peet update`.
   Tested by releasing a `v0.2.1` and watching both install types pick it up.
6. **Signing:** apply to SignPath once the release pipeline exists, then add the two signing steps.

## Risks and open questions

- **H.264 licensing (decided 2026-11-13):** releases use only Media Foundation encoders and
  decoders, licensed with Windows and the GPU drivers. openh264, compiled from source and so not
  covered by Cisco's patent licence (which covers only Cisco's prebuilt binary), is behind the
  off-by-default `peet-codec` feature `openh264`, which only `peet-bench` enables. The host never
  used it (it creates Media Foundation encoders directly), so earlier builds did not contain it
  either. Windows N editions need the Media Feature Pack, as the host's error says. A future
  Linux host would load Cisco's DLL at runtime (`openh264` crate `libloading`) under their terms.
- **Antivirus:** remote-access tools are often flagged as potentially unwanted ("RemoteAdmin"),
  and unsigned ones more so. Submit each release to Microsoft's false-positive portal until
  signing is in place.
- **Agents holding `peet.exe`:** the idle rule above defers updates while an MCP server runs. If
  that turns out to block updates for days in practice, the alternative is to have `peet mcp`
  exit cleanly when it sees a newer installed version, but most MCP clients do not restart
  servers on their own.
- **Per-user installs only update while the tray runs.** Machines that never start the tray only
  update through `peet update`. Acceptable, since the tray is the default.
