//! `cargo xtask dist`: build the release binaries and package them.
//!
//! Output in `target/dist/`:
//! - `stage/`: the files that ship (the four binaries and `LICENSE.txt`)
//! - `PeetConnect-<version>-x64.zip`: the staged files, for a portable copy
//! - `PeetConnect-<version>-setup.exe`: the Inno Setup installer (`packaging/peetconnect.iss`)
//!
//! Flags: `--no-build` packages whatever is in `target/release`; `--no-installer` skips Inno
//! Setup (CI uses it to catch packaging breakage without installing Inno Setup).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::repo_root;

/// The binaries that ship. `peet-bench` is a development tool and stays out.
const BINARIES: [&str; 4] = ["peet-tray", "peet-host", "peet-viewer", "peet"];

pub fn dist(args: &[String]) -> Result<()> {
    let mut build = true;
    let mut installer = true;
    for arg in args {
        match arg.as_str() {
            "--no-build" => build = false,
            "--no-installer" => installer = false,
            other => {
                bail!("unknown flag {other}; usage: cargo xtask dist [--no-build] [--no-installer]")
            }
        }
    }
    let root = repo_root();
    let version = env!("CARGO_PKG_VERSION");

    if build {
        let mut cargo = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        cargo
            .current_dir(&root)
            .args(["build", "--release", "--locked"]);
        for bin in BINARIES {
            cargo.args(["-p", bin]);
        }
        run(&mut cargo, "cargo build")?;
    }

    let dist = root.join("target").join("dist");
    let stage = dist.join("stage");
    if stage.exists() {
        fs::remove_dir_all(&stage).with_context(|| format!("clearing {}", stage.display()))?;
    }
    fs::create_dir_all(&stage)?;
    let release = root.join("target").join("release");
    for bin in BINARIES {
        let name = format!("{bin}.exe");
        copy(&release.join(&name), &stage.join(&name))?;
    }
    copy(&root.join("LICENSE"), &stage.join("LICENSE.txt"))?;

    let zip = dist.join(format!("PeetConnect-{version}-x64.zip"));
    if zip.exists() {
        fs::remove_file(&zip)?;
    }
    // Windows 10+ ships bsdtar, which writes zip archives with `-a`.
    run(
        Command::new("tar")
            .current_dir(&stage)
            .arg("-a")
            .arg("-cf")
            .arg(&zip)
            .arg("*"),
        "tar",
    )?;
    println!("{}", zip.display());

    if installer {
        let iscc = find_iscc()?;
        // VERSIONINFO wants plain numbers: drop a pre-release suffix such as `-beta.1`.
        let numeric = version.split(['-', '+']).next().unwrap_or(version);
        run(
            Command::new(&iscc)
                .current_dir(root.join("packaging"))
                .arg("/Qp")
                .arg(format!("/DAppVersion={version}"))
                .arg(format!("/DNumericVersion={numeric}"))
                .arg(format!("/DStageDir={}", stage.display()))
                .arg(format!("/O{}", dist.display()))
                .arg("peetconnect.iss"),
            "ISCC",
        )?;
        println!(
            "{}",
            dist.join(format!("PeetConnect-{version}-setup.exe"))
                .display()
        );
    }
    Ok(())
}

fn copy(from: &Path, to: &Path) -> Result<()> {
    fs::copy(from, to)
        .map(|_| ())
        .with_context(|| format!("copying {} (built?)", from.display()))
}

fn run(command: &mut Command, what: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("running {what}"))?;
    if !status.success() {
        bail!("{what} failed: {status}");
    }
    Ok(())
}

/// `ISCC.exe`: `$ISCC`, PATH, or the per-user / per-machine Inno Setup 6 install folders.
fn find_iscc() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ISCC") {
        return Ok(path.into());
    }
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("ISCC.exe"))
                .collect()
        })
        .unwrap_or_default();
    for (var, sub) in [
        ("LOCALAPPDATA", r"Programs\Inno Setup 6"),
        ("ProgramFiles(x86)", "Inno Setup 6"),
        ("ProgramFiles", "Inno Setup 6"),
    ] {
        if let Some(base) = std::env::var_os(var) {
            candidates.push(PathBuf::from(base).join(sub).join("ISCC.exe"));
        }
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .context("ISCC.exe not found: install Inno Setup 6 (winget install JRSoftware.InnoSetup) or set ISCC")
}
