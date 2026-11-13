//! `peet update`: check for a newer release and install it now.

use anyhow::Result;

#[cfg(windows)]
pub fn run(check_only: bool, json: bool) -> Result<()> {
    use anyhow::Context as _;
    use peet_update::{Scope, Silence, VERSION};

    let available = peet_update::check(VERSION)?;
    let installation = peet_update::installation();
    if json {
        crate::print_json(&serde_json::json!({
            "current": VERSION,
            "available": available.as_ref().map(|m| &m.version),
            "notes": available.as_ref().map(|m| &m.notes),
            "installed": installation.as_ref().map(|i| match i.scope {
                Scope::User => "user",
                Scope::Machine => "machine",
            }),
        }))?;
    }
    let Some(manifest) = available else {
        if !json {
            println!("PeetConnect {VERSION} is up to date.");
        }
        return Ok(());
    };
    if !json {
        println!(
            "PeetConnect {} is available (this is {VERSION}): {}",
            manifest.version, manifest.notes
        );
    }
    if check_only {
        return Ok(());
    }
    let installation = installation.with_context(|| {
        format!(
            "this copy was not installed by the installer (a portable or development build), so \
             it cannot update itself. Download the new version from {}",
            manifest.notes
        )
    })?;

    let blockers = peet_update::blockers(&installation.dir);
    if !blockers.is_empty() {
        let names: Vec<String> = blockers
            .iter()
            .map(peet_update::InstallProcess::describe)
            .collect();
        eprintln!("The installer will close {}.", names.join(", "));
    }
    // A per-machine update stages in this user's temp folder; the installer asks for
    // administrator rights itself.
    let staging = match installation.scope {
        Scope::User => dirs_local().join("PeetConnect").join("updates"),
        Scope::Machine => std::env::temp_dir().join("PeetConnect-updates"),
    };
    eprintln!("Downloading {}...", manifest.installer_file_name());
    let installer = peet_update::download(&manifest, &staging)?;
    let log = staging.join("install.log");
    peet_update::launch(
        &installer,
        installation.scope,
        Silence::Progress,
        Some(&log),
    )?;
    eprintln!(
        "Installing PeetConnect {}. PeetConnect programs close and start again by themselves; \
         the installer's log is {}.",
        manifest.version,
        log.display()
    );
    Ok(())
}

#[cfg(windows)]
fn dirs_local() -> std::path::PathBuf {
    std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, std::path::PathBuf::from)
}

#[cfg(not(windows))]
pub fn run(_check_only: bool, _json: bool) -> Result<()> {
    anyhow::bail!("updates are only available for the Windows installer")
}
