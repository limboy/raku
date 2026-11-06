//! Provider session discovery and catalog helpers.

use std::fs;
use std::path::PathBuf;

use anyhow::Context as _;

/// Empty, app-owned cwd for provider catalog subprocesses.
///
/// Agent CLIs may index their process cwd during startup. Launching a catalog
/// from `$HOME` therefore makes a harmless session-list request recursively
/// touch Desktop, Documents, Downloads, Photos, and other macOS TCC locations.
/// Keeping discovery in an isolated temp directory prevents that implicit
/// workspace scan; actual session loading still receives the selected cwd.
pub(crate) fn catalog_working_directory() -> anyhow::Result<PathBuf> {
    let directory = std::env::temp_dir().join(format!(
        "raku-provider-session-catalog-{}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).with_context(|| {
        format!(
            "could not create provider-session catalog directory {}",
            directory.display()
        )
    })?;
    Ok(directory)
}
