//! The Windows side: where we are installed, what runs from there, downloading and verifying
//! the installer, and launching it.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED};
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::{check_scheme, hex, http, is_newer, manifest_url, verify_release_manifest, Manifest};

/// Inno Setup's uninstall key for our AppId (packaging/peetconnect.iss).
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{0E61BA3A-C989-4990-BA9E-BBDF1304BDDD}_is1";

/// Executables that keep an update from installing while they run: Windows will not let the
/// installer replace them, and closing them would interrupt someone. The tray and host are not
/// listed; the installer closes and restarts those itself.
const BLOCKING: [&str; 2] = ["peet-viewer.exe", "peet.exe"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// "Only for me": `%LOCALAPPDATA%\Programs\PeetConnect`, updated by the tray.
    User,
    /// "Everyone on this computer": `%ProgramFiles%\PeetConnect`, updated by the service.
    Machine,
}

/// The installed copy this executable belongs to.
#[derive(Debug, Clone)]
pub struct Installation {
    pub scope: Scope,
    pub dir: PathBuf,
}

/// The installation this executable runs from, or `None` for a development build or a
/// portable copy (which never update themselves).
pub fn installation() -> Option<Installation> {
    let exe = std::env::current_exe().ok()?;
    let here = fs::canonicalize(exe.parent()?).ok()?;
    [
        (Scope::Machine, HKEY_LOCAL_MACHINE),
        (Scope::User, HKEY_CURRENT_USER),
    ]
    .into_iter()
    .find_map(|(scope, root)| {
        let location = read_string(root, UNINSTALL_KEY, "InstallLocation")?;
        let dir = fs::canonicalize(location.trim_end_matches('\\')).ok()?;
        same_path(&dir, &here).then_some(Installation { scope, dir })
    })
}

fn read_string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let key = wide(key);
    let value = wide(value);
    let mut buffer = vec![0u16; 1024];
    let mut size = u32::try_from(buffer.len() * 2).unwrap_or(u32::MAX);
    // SAFETY: the buffer and its size in bytes describe writable memory we own.
    unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    }
    .ok()
    .ok()?;
    let len = (size as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buffer[..len.min(buffer.len())]))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

/// A running process started from the installation folder.
#[derive(Debug, Clone)]
pub struct InstallProcess {
    pub pid: u32,
    /// File name, e.g. `peet.exe`.
    pub name: String,
    /// Windows session it runs in.
    pub session: u32,
}

/// Every process whose executable lives directly in `dir`. Processes we may not query (other
/// users' when we are not elevated) are skipped.
pub fn processes(dir: &Path) -> Vec<InstallProcess> {
    let Ok(dir) = fs::canonicalize(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    // SAFETY: a process snapshot, walked with a correctly sized entry and closed at the end.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return found;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0),
            ..Default::default()
        };
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            let ours = name.to_ascii_lowercase().starts_with("peet");
            if ours {
                if let Some(image) = image_path(entry.th32ProcessID) {
                    let in_dir = image
                        .parent()
                        .and_then(|p| fs::canonicalize(p).ok())
                        .is_some_and(|p| same_path(&p, &dir));
                    if in_dir {
                        let mut session = 0;
                        let _ = ProcessIdToSessionId(entry.th32ProcessID, &mut session);
                        found.push(InstallProcess {
                            pid: entry.th32ProcessID,
                            name,
                            session,
                        });
                    }
                }
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    found
}

fn image_path(pid: u32) -> Option<PathBuf> {
    // SAFETY: a query-only process handle, closed before returning.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = vec![0u16; 32_768];
        let mut size = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(process);
        result.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(
            &buffer[..size as usize],
        )))
    }
}

/// Processes in `dir` that keep an update from installing, other than this one.
pub fn blockers(dir: &Path) -> Vec<Blocker> {
    let me = std::process::id();
    processes(dir)
        .into_iter()
        .filter(|p| p.pid != me && BLOCKING.iter().any(|b| p.name.eq_ignore_ascii_case(b)))
        .collect()
}

/// A process holding up an update.
pub type Blocker = InstallProcess;

impl InstallProcess {
    /// How to name it to a person.
    pub fn describe(&self) -> String {
        if self.name.eq_ignore_ascii_case("peet-viewer.exe") {
            "the viewer".to_string()
        } else if self.name.eq_ignore_ascii_case("peet.exe") {
            "peet.exe (an agent's MCP server or a command)".to_string()
        } else {
            self.name.clone()
        }
    }
}

/// Fetches and verifies the release manifest. `Some` when it offers a version newer than
/// `current`.
pub fn check(current: &str) -> Result<Option<Manifest>> {
    let url = manifest_url();
    check_scheme(&url)?;
    let text = http::get(&url, 64 * 1024).context("fetching the update manifest")?;
    let signature =
        http::get(&format!("{url}.sig"), 1024).context("fetching the manifest signature")?;
    let signature = String::from_utf8(signature).context("the manifest signature is not text")?;
    let manifest = verify_release_manifest(&text, &signature)?;
    Ok(is_newer(&manifest.version, current).then_some(manifest))
}

/// A downloaded installer whose size and SHA-256 match the manifest. The file stays open,
/// shared for reading only, until this is dropped, so it cannot be changed or replaced
/// between the check and [`launch`].
#[derive(Debug)]
pub struct Verified {
    pub path: PathBuf,
    pub version: String,
    _lock: File,
}

/// Downloads the installer `manifest` names into `dir` (reusing an earlier complete download),
/// verifies it, and removes older installers from `dir`, keeping the newest one before this as
/// a way back.
pub fn download(manifest: &Manifest, dir: &Path) -> Result<Verified> {
    check_scheme(&manifest.installer.url)?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(manifest.installer_file_name());
    if let Ok(verified) = verify(&path, manifest) {
        return Ok(verified);
    }
    let partial = path.with_extension("part");
    {
        let mut file =
            File::create(&partial).with_context(|| format!("creating {}", partial.display()))?;
        let mut written = 0u64;
        http::fetch(&manifest.installer.url, |chunk| {
            written += chunk.len() as u64;
            anyhow::ensure!(
                written <= manifest.installer.size,
                "the installer is larger than the manifest says"
            );
            file.write_all(chunk).context("writing the installer")
        })
        .context("downloading the installer")?;
        file.sync_all().context("writing the installer")?;
    }
    let _ = fs::remove_file(&path);
    fs::rename(&partial, &path).with_context(|| format!("saving {}", path.display()))?;
    let verified = verify(&path, manifest)?;
    prune(dir, &path);
    Ok(verified)
}

/// Opens `path` so it cannot be modified, then checks its size and hash.
fn verify(path: &Path, manifest: &Manifest) -> Result<Verified> {
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let size = file.metadata()?.len();
    anyhow::ensure!(
        size == manifest.installer.size,
        "the installer is {size} bytes, the manifest says {}",
        manifest.installer.size
    );
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        context.update(&buffer[..n]);
    }
    let digest = hex(context.finish().as_ref());
    anyhow::ensure!(
        digest.eq_ignore_ascii_case(&manifest.installer.sha256),
        "the installer's SHA-256 does not match the manifest"
    );
    Ok(Verified {
        path: path.to_path_buf(),
        version: manifest.version.clone(),
        _lock: file,
    })
}

/// Deletes partial downloads and all but the newest previous installer.
fn prune(dir: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut installers = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !name.starts_with("peetconnect-") || path == keep {
            continue;
        }
        if Path::new(&name).extension().is_some_and(|e| e == "part") {
            let _ = fs::remove_file(&path);
        } else if name.ends_with("-setup.exe") {
            let modified = entry.metadata().and_then(|m| m.modified()).ok();
            installers.push((modified, path));
        }
    }
    installers.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in installers.into_iter().skip(1) {
        let _ = fs::remove_file(path);
    }
}

/// How much of the installer a person sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Silence {
    /// Nothing at all (the tray and service).
    Full,
    /// A progress window, no questions (`peet update`).
    Progress,
}

/// Starts the installer detached from this process, so it carries on when the installer closes
/// us. `log` receives Inno Setup's log.
pub fn launch(
    installer: &Verified,
    scope: Scope,
    silence: Silence,
    log: Option<&Path>,
) -> Result<()> {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let command = |flags: u32| {
        let mut command = Command::new(&installer.path);
        command
            .arg(match silence {
                Silence::Full => "/VERYSILENT",
                Silence::Progress => "/SILENT",
            })
            .args(["/SUPPRESSMSGBOXES", "/NORESTART", "/UPDATE"])
            .arg(match scope {
                Scope::User => "/CURRENTUSER",
                Scope::Machine => "/ALLUSERS",
            });
        if let Some(log) = log {
            command.raw_arg(format!("/LOG=\"{}\"", log.display()));
        }
        command.creation_flags(flags).spawn()
    };
    // Leave any job we are in (a service's, a terminal's) so stopping us does not stop the
    // installer. Not every job allows that; then stay in it.
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    match command(base | CREATE_BREAKAWAY_FROM_JOB) {
        Ok(_) => Ok(()),
        Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) => command(base)
            .map(drop)
            .with_context(|| format!("starting {}", installer.path.display())),
        Err(err) => Err(err).with_context(|| format!("starting {}", installer.path.display())),
    }
}
