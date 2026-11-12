//! Build-script helper that gives a PeetConnect executable its Windows resources: the app
//! icon (`assets/peetconnect.ico`) and a VERSIONINFO block, so Explorer, the taskbar,
//! Task Manager and the file properties dialog show PeetConnect rather than a blank exe.
//!
//! The icon gets resource ID 1 because GPUI's Windows backend loads exactly that
//! (`LoadImageW(module, 1, IMAGE_ICON)`) for its window class, which puts it in the
//! title bar, taskbar and Alt+Tab. Use from a binary's `build.rs`:
//!
//! ```ignore
//! fn main() {
//!     peet_winres::embed("PeetConnect Viewer");
//! }
//! ```
//!
//! Does nothing when the target is not Windows, so the same `build.rs` works on every
//! platform.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Embed the icon and version info into the binary being built. `description` becomes
/// the FileDescription, which Task Manager shows as the process name.
///
/// # Panics
///
/// Panics (failing the build) when called outside a build script or when the resource
/// compiler fails.
pub fn embed(description: &str) {
    let icon = icon_path();
    println!("cargo:rerun-if-changed={}", icon.display());
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let name = env::var("CARGO_PKG_NAME").expect("CARGO_PKG_NAME is set for build scripts");
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is set");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set for build scripts"));
    let rc_path = out_dir.join("peetconnect.rc");
    fs::write(
        &rc_path,
        resource_script(&icon, &name, &version, description),
    )
    .expect("write resource script");

    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_optional()
        .expect("compile Windows resources");
}

/// `assets/peetconnect.ico`, found relative to this crate so callers need no paths.
fn icon_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/peetconnect.ico")
        .canonicalize()
        .expect("assets/peetconnect.ico exists (regenerate with `cargo xtask icons`)")
}

/// The `.rc` source. Numeric constants instead of `winver.h` names keep it independent
/// of the SDK include path.
fn resource_script(icon: &Path, name: &str, version: &str, description: &str) -> String {
    let numeric = numeric_version(version);
    // rc string literals: backslashes double, and canonicalize's `\\?\` prefix confuses rc.
    let icon = icon.display().to_string();
    let icon = icon
        .strip_prefix(r"\\?\")
        .unwrap_or(&icon)
        .replace('\\', r"\\");
    let mut rc = String::new();
    let _ = write!(
        rc,
        r#"1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK 0x3f
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "PeetConnect contributors"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{name}"
      VALUE "LegalCopyright", "Copyright (c) 2026 PeetConnect contributors. MIT License."
      VALUE "OriginalFilename", "{name}.exe"
      VALUE "ProductName", "PeetConnect"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    rc
}

/// `0.1.0` / `1.2.3-beta` → `0,1,0,0` / `1,2,3,0` for the binary version fields.
fn numeric_version(version: &str) -> String {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts: Vec<u16> = core.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    parts.resize(4, 0);
    parts
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_version_pads_and_strips_prerelease() {
        assert_eq!(numeric_version("0.1.0"), "0,1,0,0");
        assert_eq!(numeric_version("1.2.3-beta.1"), "1,2,3,0");
        assert_eq!(numeric_version("2.0"), "2,0,0,0");
    }

    #[test]
    fn resource_script_escapes_icon_path() {
        let rc = resource_script(
            Path::new(r"\\?\C:\src\assets\peetconnect.ico"),
            "peet-viewer",
            "0.1.0",
            "PeetConnect Viewer",
        );
        assert!(rc.starts_with(r#"1 ICON "C:\\src\\assets\\peetconnect.ico""#));
        assert!(rc.contains(r#"VALUE "OriginalFilename", "peet-viewer.exe""#));
        assert!(rc.contains("FILEVERSION 0,1,0,0"));
    }
}
