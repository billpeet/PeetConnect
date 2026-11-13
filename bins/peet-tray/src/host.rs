//! Things the tray does outside its window: run a console host, open files, and manage the
//! sign-in Run key.

use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("PeetConnect");

/// Where a tray-started host writes its log (truncated on every start).
pub fn log_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("PeetConnect").join("host.log"))
}

/// Default host configuration file, as `peet-host` resolves it.
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("PeetConnect").join("host.toml"))
}

/// Starts `peet-host --console` from next to this executable, windowless, logging to
/// [`log_path`].
pub fn start() -> Result<Child> {
    let exe = std::env::current_exe().context("locating peet-tray.exe")?;
    let host = exe.with_file_name("peet-host.exe");
    anyhow::ensure!(host.exists(), "{} not found", host.display());
    let log = log_path().context("no local app data folder")?;
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let log_file = fs::File::create(&log).with_context(|| format!("creating {}", log.display()))?;
    Command::new(&host)
        .arg("--console")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| format!("starting {}", host.display()))
}

/// Waits up to `timeout` for `child` to exit, then kills it.
pub fn wait_or_kill(child: &mut Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Opens `path` with its associated program, falling back to Notepad for files with no
/// association (a fresh machine has none for `.toml` or `.log`).
pub fn open(path: &Path) {
    let file = HSTRING::from(path.as_os_str());
    // SAFETY: plain shell call with valid NUL-terminated strings.
    let result = unsafe { ShellExecuteW(None, w!("open"), &file, None, None, SW_SHOWNORMAL) };
    if result.0 as usize <= 32 {
        let _ = Command::new("notepad.exe").arg(path).spawn();
    }
}

/// Opens the service's configuration in Notepad as administrator: the file is writable only
/// by SYSTEM and Administrators. Declining the UAC prompt just does nothing.
pub fn edit_elevated(path: &Path) {
    let args = HSTRING::from(format!("\"{}\"", path.display()));
    // SAFETY: plain shell call with valid NUL-terminated strings.
    let _ = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            w!("notepad.exe"),
            &args,
            None,
            SW_SHOWNORMAL,
        )
    };
}

/// Restarts the service through an elevated, hidden PowerShell (the tray itself never runs
/// elevated). Declining the UAC prompt just does nothing.
pub fn restart_service() {
    // SAFETY: plain shell call with valid NUL-terminated strings.
    let _ = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            w!("powershell.exe"),
            w!("-NoProfile -WindowStyle Hidden -Command Restart-Service PeetConnect"),
            None,
            SW_HIDE,
        )
    };
}

/// Creates the host configuration file with a commented template if it does not exist yet.
pub fn ensure_config(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(path, CONFIG_TEMPLATE).with_context(|| format!("writing {}", path.display()))
}

const CONFIG_TEMPLATE: &str = r#"# PeetConnect host configuration.
# Restart the host after editing (tray menu: Restart host).

# Optional extra secret viewers must enter on top of their Tailscale identity.
# pin = "1234"

# Who may connect. Nobody can until at least one rule is set.
[allow]
# users = ["you@example.com"]   # Tailscale logins
# tags = ["tag:admin"]          # ACL tags
# nodes = ["laptop"]            # MagicDNS names or node IDs
# any_tailnet = false           # every device on the tailnet

[video]
# fps = 60
# max_bitrate_kbps = 12000
"#;

/// True when the Run key starts the tray at sign-in.
pub fn autostart_enabled() -> bool {
    // SAFETY: querying existence only; no output buffers.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
    }
    .is_ok()
}

/// Adds or removes the Run value that starts this executable at sign-in.
pub fn set_autostart(enabled: bool) -> Result<()> {
    if enabled {
        let exe = std::env::current_exe().context("locating peet-tray.exe")?;
        let command = format!("\"{}\"", exe.display());
        let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = u32::try_from(wide.len() * 2).context("path too long")?;
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer of `bytes` bytes.
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(wide.as_ptr().cast()),
                bytes,
            )
        }
        .ok()
        .context("writing the Run key")
    } else {
        // SAFETY: deleting a value by name.
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) }
            .ok()
            .context("removing the Run key")
    }
}
