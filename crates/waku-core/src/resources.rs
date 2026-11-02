//! Locating packaged resources next to the running Waku executable.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};

pub(crate) fn resources_directory(executable: &Path, os: &str) -> anyhow::Result<PathBuf> {
    let directory = executable
        .parent()
        .ok_or_else(|| anyhow!("Waku executable has no parent"))?;
    Ok(match os {
        "macos" => directory
            .parent()
            .ok_or_else(|| anyhow!("Waku app bundle is malformed"))?
            .join("Resources"),
        "linux" if directory.file_name().is_some_and(|name| name == "bin") => directory
            .parent()
            .ok_or_else(|| anyhow!("Waku installation is malformed"))?
            .join("share/waku"),
        _ => directory.join("resources"),
    })
}

pub(crate) fn packaged_file(path: &Path, name: &str) -> anyhow::Result<PathBuf> {
    if !path.is_file() {
        bail!("{name} is missing from this Waku build: {}", path.display());
    }
    Ok(path.to_path_buf())
}

pub(crate) fn host_executable_path() -> anyhow::Result<PathBuf> {
    std::env::var_os(crate::APP_EXECUTABLE_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(|| std::env::current_exe().context("Waku executable path is unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_resources_follow_each_platform_layout() {
        for (executable, os, resources) in [
            (
                "/Applications/Waku.app/Contents/MacOS/Waku",
                "macos",
                "/Applications/Waku.app/Contents/Resources",
            ),
            ("/opt/waku/bin/waku", "linux", "/opt/waku/share/waku"),
            (
                "/dev/waku/target/debug/waku",
                "linux",
                "/dev/waku/target/debug/resources",
            ),
            ("/Waku/waku.exe", "windows", "/Waku/resources"),
        ] {
            assert_eq!(
                resources_directory(Path::new(executable), os).unwrap(),
                PathBuf::from(resources)
            );
        }
    }
}
