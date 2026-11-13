//! Release chores: versioning, release notes, and the signed update manifest.
//!
//! - `cargo xtask release <version>`: bump the workspace version, move the CHANGELOG's
//!   "Unreleased" notes under the new version, commit and tag `v<version>`. Pushing is left to
//!   the caller (`git push origin master v<version>`), which starts `release.yml`.
//! - `cargo xtask check-tag <tag>`: fail unless the tag is `v` + the workspace version.
//! - `cargo xtask notes [<version>]`: print that version's CHANGELOG section.
//! - `cargo xtask manifest`: write `latest.json`, its signature `latest.json.sig` and
//!   `SHA256SUMS` next to the installer in `target/dist/`. The ed25519 private key comes from
//!   `$UPDATE_SIGNING_KEY` (base64 PKCS#8); the signature is checked against
//!   `packaging/update-key.pub` before anything is written, so a key that does not match what
//!   the binaries trust fails the release instead of shipping updates nobody accepts.
//! - `cargo xtask keygen <private-key-file>`: make a new key pair. The private key goes to the
//!   file (never into the repository); the public key to `packaging/update-key.pub`.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair as _, UnparsedPublicKey, ED25519};

use crate::repo_root;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPOSITORY: &str = "billpeet/peetconnect";

pub fn check_tag(args: &[String]) -> Result<()> {
    let [tag] = args else {
        bail!("usage: cargo xtask check-tag <tag>")
    };
    ensure!(
        tag.strip_prefix('v') == Some(VERSION),
        "tag {tag} does not match the workspace version {VERSION} (expected v{VERSION})"
    );
    println!("tag {tag} matches the workspace version");
    Ok(())
}

pub fn notes(args: &[String]) -> Result<()> {
    let version = args.first().map_or(VERSION, String::as_str);
    let changelog = fs::read_to_string(repo_root().join("CHANGELOG.md"))?;
    let section = changelog_section(&changelog, version)
        .with_context(|| format!("CHANGELOG.md has no section for {version}"))?;
    println!("{section}");
    Ok(())
}

pub fn release(args: &[String]) -> Result<()> {
    let [version] = args else {
        bail!("usage: cargo xtask release <version>   (e.g. 0.2.0)")
    };
    let version = version.trim_start_matches('v');
    let new = parse_version(version)?;
    let current = parse_version(VERSION)?;
    ensure!(
        new > current,
        "{version} is not newer than the current version {VERSION}"
    );
    let root = repo_root();
    git(&root, &["diff", "--quiet", "HEAD"]).context("the working tree has uncommitted changes")?;

    // CHANGELOG: "## Unreleased" keeps its heading; its notes move under the new version.
    let changelog_path = root.join("CHANGELOG.md");
    let changelog = fs::read_to_string(&changelog_path)?;
    let unreleased = changelog_section(&changelog, "Unreleased")
        .context("CHANGELOG.md has no \"## Unreleased\" section")?;
    ensure!(
        !unreleased.trim().is_empty(),
        "CHANGELOG.md has nothing under \"## Unreleased\"; describe the release first"
    );
    // Keep whatever line endings the checkout has (git may convert to CRLF on Windows).
    let nl = if changelog.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let changelog = changelog.replacen(
        &format!("## Unreleased{nl}"),
        &format!("## Unreleased{nl}{nl}## {version} - {}{nl}", today()),
        1,
    );
    fs::write(&changelog_path, changelog)?;

    // Cargo.toml: the version line inside [workspace.package].
    let manifest_path = root.join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)?;
    let start = manifest
        .find("[workspace.package]")
        .context("Cargo.toml has no [workspace.package]")?;
    let old_line = format!("version = \"{VERSION}\"");
    let offset = manifest[start..]
        .find(&old_line)
        .context("no version line in [workspace.package]")?;
    let mut updated = manifest.clone();
    updated.replace_range(
        start + offset..start + offset + old_line.len(),
        &format!("version = \"{version}\""),
    );
    fs::write(&manifest_path, updated)?;

    // Refresh the lock file's workspace entries.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(&root)
        .args(["update", "--workspace", "--offline"])
        .status()?;
    ensure!(status.success(), "cargo update --workspace failed");

    git(&root, &["add", "Cargo.toml", "Cargo.lock", "CHANGELOG.md"])?;
    git(
        &root,
        &["commit", "-q", "-m", &format!("Release v{version}")],
    )?;
    git(
        &root,
        &[
            "tag",
            "-a",
            &format!("v{version}"),
            "-m",
            &format!("PeetConnect {version}"),
        ],
    )?;
    println!("Committed and tagged v{version}. Publish with: git push origin HEAD v{version}");
    Ok(())
}

pub fn manifest(args: &[String]) -> Result<()> {
    ensure!(args.is_empty(), "usage: cargo xtask manifest");
    let dist = repo_root().join("target").join("dist");
    let installer_name = format!("PeetConnect-{VERSION}-setup.exe");
    let installer = fs::read(dist.join(&installer_name))
        .with_context(|| format!("reading target/dist/{installer_name} (run cargo xtask dist)"))?;

    let manifest = serde_json::json!({
        "version": VERSION,
        "published": now_rfc3339(),
        "notes": format!("https://github.com/{REPOSITORY}/releases/tag/v{VERSION}"),
        "installer": {
            "url": format!("https://github.com/{REPOSITORY}/releases/download/v{VERSION}/{installer_name}"),
            "sha256": hex(ring::digest::digest(&ring::digest::SHA256, &installer).as_ref()),
            "size": installer.len(),
        },
    });
    let mut text = serde_json::to_string_pretty(&manifest)?;
    text.push('\n');

    let key = std::env::var("UPDATE_SIGNING_KEY").context("UPDATE_SIGNING_KEY is not set")?;
    let pkcs8 = BASE64
        .decode(key.trim())
        .context("UPDATE_SIGNING_KEY is not base64")?;
    let pair = Ed25519KeyPair::from_pkcs8(&pkcs8)
        .map_err(|e| anyhow::anyhow!("UPDATE_SIGNING_KEY is not an ed25519 PKCS#8 key: {e}"))?;
    let signature = pair.sign(text.as_bytes());

    let trusted = read_public_key()?;
    UnparsedPublicKey::new(&ED25519, &trusted)
        .verify(text.as_bytes(), signature.as_ref())
        .map_err(|_| {
            anyhow::anyhow!("UPDATE_SIGNING_KEY does not match packaging/update-key.pub")
        })?;

    fs::write(dist.join("latest.json"), &text)?;
    fs::write(
        dist.join("latest.json.sig"),
        format!("{}\n", BASE64.encode(signature.as_ref())),
    )?;

    let mut sums = String::new();
    for name in [
        installer_name.clone(),
        format!("PeetConnect-{VERSION}-x64.zip"),
    ] {
        let bytes = fs::read(dist.join(&name)).with_context(|| format!("reading {name}"))?;
        let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
        let _ = writeln!(sums, "{}  {name}", hex(digest.as_ref()));
    }
    fs::write(dist.join("SHA256SUMS"), sums)?;
    for name in ["latest.json", "latest.json.sig", "SHA256SUMS"] {
        println!("{}", dist.join(name).display());
    }
    Ok(())
}

pub fn keygen(args: &[String]) -> Result<()> {
    let [out] = args else {
        bail!("usage: cargo xtask keygen <private-key-file>   (keep it outside the repository)")
    };
    let out = PathBuf::from(out);
    let root = repo_root();
    ensure!(
        !std::path::absolute(&out)?.starts_with(&root),
        "refusing to write the private key inside the repository"
    );
    ensure!(!out.exists(), "{} already exists", out.display());
    let public = root.join("packaging").join("update-key.pub");
    ensure!(
        !public.exists(),
        "{} exists. Installed copies trust that key, so they would reject every update signed \
         with a new one. Delete the file first only if you really mean to rotate the key.",
        public.display()
    );
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| anyhow::anyhow!("key generation failed"))?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|e| anyhow::anyhow!("reading the new key: {e}"))?;
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    fs::write(&out, format!("{}\n", BASE64.encode(pkcs8.as_ref())))?;
    fs::write(
        &public,
        format!("{}\n", BASE64.encode(pair.public_key().as_ref())),
    )?;
    println!("private key: {}", out.display());
    println!("public key:  packaging/update-key.pub (commit it)");
    println!(
        "Next: gh secret set UPDATE_SIGNING_KEY < \"{}\", then store the file offline.",
        out.display()
    );
    Ok(())
}

fn read_public_key() -> Result<Vec<u8>> {
    let path = repo_root().join("packaging").join("update-key.pub");
    let text = fs::read_to_string(&path)
        .with_context(|| format!("reading {} (run cargo xtask keygen)", path.display()))?;
    let key = BASE64
        .decode(text.trim())
        .context("update-key.pub is not base64")?;
    ensure!(
        key.len() == 32,
        "update-key.pub is not a 32-byte ed25519 key"
    );
    Ok(key)
}

/// The text under `## <heading>` up to the next `## `, trimmed. Headings may carry a date
/// after the version (`## 0.2.0 - 2026-11-20`).
fn changelog_section<'a>(changelog: &'a str, heading: &str) -> Option<&'a str> {
    let mut start = None;
    let mut offset = 0;
    for line in changelog.split_inclusive('\n') {
        if let Some(title) = line.strip_prefix("## ") {
            if start.is_some() {
                return Some(changelog[start?..offset].trim());
            }
            let name = title.split_whitespace().next().unwrap_or("");
            if name == heading {
                start = Some(offset + line.len());
            }
        }
        offset += line.len();
    }
    start.map(|s| changelog[s..].trim())
}

/// `major.minor.patch` with an optional `-pre` suffix; a pre-release sorts before its release.
fn parse_version(text: &str) -> Result<(u64, u64, u64, bool)> {
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, !pre.is_empty()),
        None => (text, false),
    };
    let parts: Vec<u64> = core
        .split('.')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .with_context(|| format!("{text} is not a version like 0.2.0"))?;
    let [major, minor, patch] = parts[..] else {
        bail!("{text} is not a version like 0.2.0")
    };
    Ok((major, minor, patch, !pre))
}

fn git(root: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git").current_dir(root).args(args).status()?;
    ensure!(status.success(), "git {} failed", args.join(" "));
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn today() -> String {
    now_rfc3339()[..10].to_string()
}

/// UTC now as `YYYY-MM-DDTHH:MM:SSZ`, without pulling in a date crate.
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(i64::try_from(days).expect("days fit"));
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).expect("day in range");
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).expect("month in range");
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_405), (2025, 11, 13));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn versions() {
        assert!(parse_version("0.2.0").unwrap() > parse_version("0.1.9").unwrap());
        assert!(parse_version("0.2.0").unwrap() > parse_version("0.2.0-beta.1").unwrap());
        assert!(parse_version("0.2").is_err());
    }

    #[test]
    fn sections() {
        let text = "# Changelog\n\n## Unreleased\n\n- next\n\n## 0.2.0 - 2026-11-20\n\n- first\n";
        assert_eq!(changelog_section(text, "Unreleased"), Some("- next"));
        assert_eq!(changelog_section(text, "0.2.0"), Some("- first"));
        assert_eq!(changelog_section(text, "0.1.0"), None);
    }
}
