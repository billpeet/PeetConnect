//! Machine-wide state for the Windows service: `%ProgramData%\PeetConnect`.
//!
//! The service and its worker run as SYSTEM, whose own profile folders are the wrong place for
//! settings an administrator edits, so they use `%ProgramData%\PeetConnect` instead:
//!
//! - `host.toml`: the service's configuration. SYSTEM and Administrators only, since it may
//!   hold the PIN.
//! - `logs\service.log`, `logs\host.log`: the service and the worker. Users may read them.
//!
//! Any user may create folders under `%ProgramData%`, so the folder could already exist, owned
//! by someone else, before the service is installed. [`secure`] therefore takes ownership for
//! Administrators and resets the security of the whole tree, not just the top folder.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
    TreeResetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, ACL, DACL_SECURITY_INFORMATION,
    OBJECT_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID,
};

/// The folder: owned by Administrators; SYSTEM and Administrators have full control, Users may
/// read and list. Inherited by everything inside.
const DIR_SDDL: &str = "O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";
/// `host.toml`: SYSTEM and Administrators only.
const CONFIG_SDDL: &str = "O:BAD:PAI(A;;FA;;;SY)(A;;FA;;;BA)";

/// Log files are renamed to `*.old` at startup once they pass this size.
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

pub fn dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
        .join("PeetConnect")
}

pub fn config_path() -> PathBuf {
    dir().join("host.toml")
}

pub fn log_dir() -> PathBuf {
    dir().join("logs")
}

/// Creates the folder if needed and resets its security as described in the module docs.
/// Needs Administrator or SYSTEM rights.
pub fn secure() -> Result<()> {
    let dir = dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let descriptor = Descriptor::parse(DIR_SDDL)?;
    let (owner, dacl) = descriptor.owner_and_dacl()?;
    let name = HSTRING::from(dir.as_os_str());
    // SAFETY: `name` is a valid path string; `owner` and `dacl` point into `descriptor`, which
    // outlives the call. No progress callback.
    let result = unsafe {
        TreeResetNamedSecurityInfoW(
            &name,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            Some(owner),
            None,
            Some(dacl),
            None,
            false,
            None,
            windows::Win32::Security::Authorization::ProgressInvokeNever,
            None,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(windows::core::Error::from(result.to_hresult()))
            .with_context(|| format!("securing {} (needs an elevated prompt)", dir.display()));
    }
    let config = config_path();
    if config.exists() {
        secure_config(&config)?;
    }
    Ok(())
}

/// Restricts a configuration file to SYSTEM and Administrators.
pub fn secure_config(path: &Path) -> Result<()> {
    set_security(path, CONFIG_SDDL)
}

fn set_security(path: &Path, sddl: &str) -> Result<()> {
    let descriptor = Descriptor::parse(sddl)?;
    let (owner, dacl) = descriptor.owner_and_dacl()?;
    let name = HSTRING::from(path.as_os_str());
    let info: OBJECT_SECURITY_INFORMATION = OWNER_SECURITY_INFORMATION
        | DACL_SECURITY_INFORMATION
        | PROTECTED_DACL_SECURITY_INFORMATION;
    // SAFETY: as in `secure`.
    let result = unsafe {
        SetNamedSecurityInfoW(
            &name,
            SE_FILE_OBJECT,
            info,
            Some(owner),
            None,
            Some(dacl),
            None,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(windows::core::Error::from(result.to_hresult()))
            .with_context(|| format!("securing {}", path.display()));
    }
    Ok(())
}

/// Opens `logs\<name>` for appending, first moving an oversized file aside to `<name>.old`.
pub fn open_log(name: &str) -> Result<File> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(name);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))
}

/// A security descriptor parsed from SDDL, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn parse(sddl: &str) -> Result<Self> {
        let text = HSTRING::from(sddl);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `text` is a valid NUL-terminated wide string; the out-pointer is ours.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .context("parsing a security descriptor")?;
        Ok(Self(descriptor))
    }

    /// Pointers into the descriptor; valid while `self` lives.
    fn owner_and_dacl(&self) -> Result<(PSID, *const ACL)> {
        let mut owner = PSID::default();
        let mut owner_defaulted = false.into();
        let mut present = false.into();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut dacl_defaulted = false.into();
        // SAFETY: `self.0` is a valid descriptor; the out-pointers are ours.
        unsafe {
            GetSecurityDescriptorOwner(self.0, &mut owner, &mut owner_defaulted)
                .context("reading the descriptor's owner")?;
            GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut dacl_defaulted)
                .context("reading the descriptor's DACL")?;
        }
        anyhow::ensure!(
            present.as_bool() && !dacl.is_null(),
            "descriptor has no DACL"
        );
        Ok((owner, dacl))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}
