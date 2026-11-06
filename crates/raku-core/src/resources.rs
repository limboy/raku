//! Locating packaged resources next to the running Raku executable.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};

pub(crate) fn resources_directory(executable: &Path, os: &str) -> anyhow::Result<PathBuf> {
    let directory = executable
        .parent()
        .ok_or_else(|| anyhow!("Raku executable has no parent"))?;
    Ok(match os {
        "macos" => directory
            .parent()
            .ok_or_else(|| anyhow!("Raku app bundle is malformed"))?
            .join("Resources"),
        "linux" if directory.file_name().is_some_and(|name| name == "bin") => directory
            .parent()
            .ok_or_else(|| anyhow!("Raku installation is malformed"))?
            .join("share/raku"),
        _ => directory.join("resources"),
    })
}

pub(crate) fn packaged_file(path: &Path, name: &str) -> anyhow::Result<PathBuf> {
    if !path.is_file() {
        bail!("{name} is missing from this Raku build: {}", path.display());
    }
    Ok(path.to_path_buf())
}

pub(crate) fn host_executable_path() -> anyhow::Result<PathBuf> {
    std::env::var_os(crate::APP_EXECUTABLE_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(|| std::env::current_exe().context("Raku executable path is unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_resources_follow_each_platform_layout() {
        for (executable, os, resources) in [
            (
                "/Applications/Raku.app/Contents/MacOS/Raku",
                "macos",
                "/Applications/Raku.app/Contents/Resources",
            ),
            ("/opt/raku/bin/raku", "linux", "/opt/raku/share/raku"),
            (
                "/dev/raku/target/debug/raku",
                "linux",
                "/dev/raku/target/debug/resources",
            ),
            ("/Raku/raku.exe", "windows", "/Raku/resources"),
        ] {
            assert_eq!(
                resources_directory(Path::new(executable), os).unwrap(),
                PathBuf::from(resources)
            );
        }
    }
}
