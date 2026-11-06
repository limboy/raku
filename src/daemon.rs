//! Desktop ownership of the Raku daemon process.

use std::path::PathBuf;

use anyhow::{Context as _, anyhow, bail};

pub fn start_process() -> anyhow::Result<raku_client::DaemonSupervisor> {
    raku_client::persistence::prepare_pending_restore()
        .context("could not prepare backup restore")?;
    let _ = raku_client::persistence::load_or_create_app_settings()
        .context("could not load desktop daemon settings")?;
    let daemon =
        raku_client::DaemonSupervisor::spawn(&daemon_executable_path()?, cfg!(debug_assertions))?;
    raku_client::persistence::finish_pending_restore()
        .context("could not restore desktop preferences")?;
    Ok(daemon)
}

fn daemon_executable_path() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("RAKU_DAEMON_PATH").filter(|path| !path.is_empty()) {
        return Ok(path.into());
    }
    let name = if cfg!(debug_assertions) {
        "raku-debug-daemon"
    } else {
        "raku-daemon"
    };
    let executable = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let current = std::env::current_exe().context("could not locate the Raku executable")?;

    // Development keeps the daemon beside Cargo's debug artifacts rather than
    // inside Raku Debug.app. The supervisor watches this file and swaps only
    // the daemon when the development watcher relinks it.
    #[cfg(debug_assertions)]
    if let Some(debug_directory) = current
        .ancestors()
        .find(|candidate| candidate.file_name().is_some_and(|name| name == "debug"))
    {
        let external = debug_directory.join(&executable);
        if external.is_file() {
            return Ok(external);
        }
    }

    let sibling = current
        .parent()
        .map(|directory| directory.join(&executable))
        .ok_or_else(|| anyhow!("Raku executable has no parent directory"))?;
    if sibling.is_file() {
        return Ok(sibling);
    }
    #[cfg(debug_assertions)]
    bail!(
        "Raku daemon was not found in Cargo's debug directory or next to the app executable: {}",
        sibling.display(),
    );
    #[cfg(not(debug_assertions))]
    bail!(
        "Raku daemon is missing next to the app executable: {}",
        sibling.display(),
    )
}
