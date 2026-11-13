//! PeetConnect's updater: shared by the tray (per-user installs), the service (per-machine
//! installs) and `peet update`.
//!
//! Every GitHub release carries `latest.json` and `latest.json.sig`, an ed25519 signature over
//! the exact bytes of `latest.json` made with the key behind `packaging/update-key.pub`. A
//! manifest is accepted only if the signature verifies and its version is newer than ours, so an
//! older signed manifest cannot be replayed. The installer it names must then match the
//! manifest's SHA-256 and size; it is held open while it is checked and launched, so it cannot be
//! swapped in between.
//!
//! The policy around this (when to check, what "idle" means, who launches the installer) lives
//! with the callers; see docs/PACKAGING.md.

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

#[cfg(windows)]
mod http;
#[cfg(windows)]
mod system;

#[cfg(windows)]
pub use system::{
    blockers, check, download, installation, launch, processes, Blocker, InstallProcess,
    Installation, Scope, Silence, Verified,
};

/// Where clients look for the newest published release. GitHub serves the newest release that
/// is neither a draft nor a pre-release here.
pub const MANIFEST_URL: &str =
    "https://github.com/billpeet/PeetConnect/releases/latest/download/latest.json";

/// Overrides [`MANIFEST_URL`] (for testing against a local server; plain `http://` is allowed
/// then). The signature is still required, so this cannot be used to push an unsigned build.
pub const MANIFEST_URL_ENV: &str = "PEETCONNECT_UPDATE_MANIFEST";

/// The release signing key, base64 of the raw 32-byte ed25519 public key.
const PUBLIC_KEY: &str = include_str!("../../../packaging/update-key.pub");

/// The version of this build.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `latest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// RFC 3339 time the release was built.
    pub published: String,
    /// Release page.
    pub notes: String,
    pub installer: InstallerInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallerInfo {
    pub url: String,
    /// Lowercase hex.
    pub sha256: String,
    pub size: u64,
}

impl Manifest {
    /// File name the installer is stored under while staged.
    pub fn installer_file_name(&self) -> String {
        format!("PeetConnect-{}-setup.exe", self.version)
    }
}

/// The URL clients fetch the manifest from: [`MANIFEST_URL`] unless [`MANIFEST_URL_ENV`] is set.
pub fn manifest_url() -> String {
    std::env::var(MANIFEST_URL_ENV)
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| MANIFEST_URL.to_string())
}

/// The trusted public key compiled into this build.
fn public_key() -> Result<Vec<u8>> {
    let key = BASE64
        .decode(PUBLIC_KEY.trim())
        .context("update-key.pub is not base64")?;
    anyhow::ensure!(key.len() == 32, "update-key.pub is not an ed25519 key");
    Ok(key)
}

/// Verifies `signature` (base64, as in `latest.json.sig`) over `text` with `key` and parses the
/// manifest. Nothing in `text` is looked at before the signature checks out.
pub fn verify_manifest(text: &[u8], signature: &str, key: &[u8]) -> Result<Manifest> {
    let signature = BASE64
        .decode(signature.trim())
        .context("the manifest signature is not base64")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(text, &signature)
        .map_err(|_| anyhow::anyhow!("the update manifest's signature does not verify"))?;
    let manifest: Manifest =
        serde_json::from_slice(text).context("the update manifest is not valid JSON")?;
    anyhow::ensure!(
        Version::parse(&manifest.version).is_some(),
        "the update manifest has an invalid version {:?}",
        manifest.version
    );
    anyhow::ensure!(
        manifest.installer.sha256.len() == 64
            && manifest
                .installer
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit()),
        "the update manifest has an invalid SHA-256"
    );
    Ok(manifest)
}

/// Verifies a manifest against the key compiled into this build.
pub fn verify_release_manifest(text: &[u8], signature: &str) -> Result<Manifest> {
    verify_manifest(text, signature, &public_key()?)
}

/// True when `candidate` is a strictly newer version than `current`. Unparseable versions are
/// never newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate), Version::parse(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// A semantic version (`1.2.3`, `1.2.3-rc.1`; build metadata after `+` is ignored), ordered by
/// semver precedence.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    core: [u64; 3],
    pre: Vec<Identifier>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Identifier {
    // Declared first: numeric identifiers sort before alphanumeric ones.
    Numeric(u64),
    Text(String),
}

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches('v');
        let text = text.split_once('+').map_or(text, |(v, _)| v);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
        let core = [parts.next()??, parts.next()??, parts.next()??];
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => pre
                .split('.')
                .map(|id| {
                    if id.is_empty() {
                        None
                    } else if let Ok(n) = id.parse() {
                        Some(Identifier::Numeric(n))
                    } else {
                        Some(Identifier::Text(id.to_string()))
                    }
                })
                .collect::<Option<_>>()?,
        };
        Some(Self { core, pre })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        self.core.cmp(&other.core).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A pre-release sorts before the release itself.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            }
        })
    }
}

/// What an updater is doing, for display. The tray keeps it in memory for a per-user install;
/// the service writes it to `%ProgramData%\PeetConnect\update.json` for the tray to read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    /// No check has finished yet.
    #[default]
    Unknown,
    UpToDate {
        /// Unix seconds of the last successful check.
        checked: u64,
    },
    Downloading {
        version: String,
    },
    /// Downloaded and verified; waiting until nothing is in use, or for someone to install it.
    Ready {
        version: String,
        /// Installs by itself once idle. False with notify-only switched on.
        automatic: bool,
        /// Unix seconds since when it has been waiting.
        since: u64,
        /// What is keeping it from installing right now, for display.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        waiting_for: Vec<String>,
    },
    Installing {
        version: String,
    },
    Failed {
        message: String,
        /// Unix seconds of the first failure in a row.
        since: u64,
    },
}

/// Current time in Unix seconds.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Lowercase hex of `bytes`.
#[cfg_attr(not(windows), allow(dead_code))]
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Rejects anything but `https://` (and `http://` when the manifest URL was overridden for
/// testing).
#[cfg_attr(not(windows), allow(dead_code))]
fn check_scheme(url: &str) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if url.starts_with("http://") && std::env::var_os(MANIFEST_URL_ENV).is_some() {
        return Ok(());
    }
    bail!("refusing to download from {url}: not https")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair as _;

    #[test]
    fn versions_order_by_semver_precedence() {
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "1.0.0-rc.2"));
        assert!(is_newer("1.0.0-rc.10", "1.0.0-rc.2"));
        assert!(is_newer("1.0.0-rc.1", "1.0.0-beta"));
        assert!(is_newer("1.0.0-beta.1", "1.0.0-beta"));
        assert!(is_newer("v0.3.0", "0.2.0+build.5"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.2.0+other", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("garbage", "0.2.0"));
        assert!(!is_newer("1.2", "0.2.0"));
        assert!(!is_newer("1.2.3.4", "0.2.0"));
        assert!(!is_newer("1.0.0-", "0.2.0"));
    }

    fn signed(text: &str) -> (Vec<u8>, String) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let signature = BASE64.encode(pair.sign(text.as_bytes()).as_ref());
        (pair.public_key().as_ref().to_vec(), signature)
    }

    const MANIFEST: &str = r#"{
  "installer": {
    "sha256": "4db89e79a3f8512553de45523aa6260d5cdb679b9c18f8b6ab7d9abf11a02d03",
    "size": 9028820,
    "url": "https://github.com/billpeet/peetconnect/releases/download/v0.2.0/PeetConnect-0.2.0-setup.exe"
  },
  "notes": "https://github.com/billpeet/peetconnect/releases/tag/v0.2.0",
  "published": "2026-11-13T05:12:37Z",
  "version": "0.2.0"
}
"#;

    #[test]
    fn verifies_and_parses_a_signed_manifest() {
        let (key, signature) = signed(MANIFEST);
        let manifest = verify_manifest(MANIFEST.as_bytes(), &signature, &key).unwrap();
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.installer.size, 9_028_820);
        assert_eq!(
            manifest.installer_file_name(),
            "PeetConnect-0.2.0-setup.exe"
        );
    }

    #[test]
    fn rejects_tampered_manifests_and_wrong_keys() {
        let (key, signature) = signed(MANIFEST);
        let tampered = MANIFEST.replace("9028820", "9028821");
        assert!(verify_manifest(tampered.as_bytes(), &signature, &key).is_err());
        let (other_key, _) = signed(MANIFEST);
        assert!(verify_manifest(MANIFEST.as_bytes(), &signature, &other_key).is_err());
        assert!(verify_manifest(MANIFEST.as_bytes(), "not base64!", &key).is_err());
    }

    /// The v0.2.0 release on GitHub, signed in CI with the real key.
    #[test]
    fn accepts_the_published_v0_2_0_manifest() {
        let signature = "hwL+FJJfQI4G/rsQkjS2BbkAlE1TryjgczNpmq9MHDWmnTi/dms3WXe8chliCjgLgURoBFF4PLVEnvica+HhAw==";
        let manifest = verify_release_manifest(MANIFEST.as_bytes(), signature).unwrap();
        assert_eq!(manifest.version, "0.2.0");
    }

    #[test]
    fn status_serialises_with_a_state_tag() {
        let status = Status::Ready {
            version: "0.2.1".into(),
            automatic: true,
            since: 5,
            waiting_for: vec!["peet.exe".into()],
        };
        let text = serde_json::to_string(&status).unwrap();
        assert_eq!(
            text,
            r#"{"state":"ready","version":"0.2.1","automatic":true,"since":5,"waiting_for":["peet.exe"]}"#
        );
        assert_eq!(serde_json::from_str::<Status>(&text).unwrap(), status);
    }

    #[test]
    fn only_https_without_an_override() {
        assert!(check_scheme("https://github.com/x").is_ok());
        if std::env::var_os(MANIFEST_URL_ENV).is_none() {
            assert!(check_scheme("http://github.com/x").is_err());
        }
        assert!(check_scheme("file:///c:/x").is_err());
    }
}
