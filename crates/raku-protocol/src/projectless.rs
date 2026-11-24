//! Pure daemon-path classification shared by clients and the daemon.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::{OnceLock, RwLock};

fn workspace_root_slot() -> &'static RwLock<Option<PathBuf>> {
    static ROOT: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();
    ROOT.get_or_init(|| {
        RwLock::new(dirs::home_dir().map(|home| home.join(".raku").join("projects")))
    })
}

pub fn set_workspace_root(root: Option<PathBuf>) {
    if let Ok(mut current) = workspace_root_slot().write() {
        *current = root;
    }
}

pub fn workspace_root() -> Option<PathBuf> {
    workspace_root_slot().read().ok()?.clone()
}

pub fn home_directory() -> Option<PathBuf> {
    let root = workspace_root()?;
    root.parent()?.parent().map(Path::to_path_buf)
}

pub fn is_projectless_path(path: &Path) -> bool {
    workspace_root().is_some_and(|root| is_projectless_path_in(path, &root))
}

/// Pure classification against an explicit workspace root, for callers that
/// resolve the root once per pass. Covers the current `<root>/<date>/<slug>`
/// layout, the older dated layout directly under the data directory, and
/// workspaces stored before the Waku → Raku rename.
pub fn is_projectless_path_in(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
        || root
            .parent()
            .is_some_and(|legacy_root| is_legacy_workspace_path(path, legacy_root))
        || is_pre_rename_workspace_path(path, root)
}

/// The rename moved `~/.waku` to `~/.raku` and left a symlink at the old path
/// so stored absolute paths keep resolving. Projects created before it still
/// record `~/.waku/projects/<date>/<slug>`; they are the same workspaces and
/// must classify as projectless. This compares path components only, so it
/// stays safe on render paths.
fn is_pre_rename_workspace_path(path: &Path, root: &Path) -> bool {
    let (Some(data_directory), Some(projects)) = (root.parent(), root.file_name()) else {
        return false;
    };
    if data_directory.file_name() != Some(OsStr::new(CURRENT_DATA_DIRECTORY)) {
        return false;
    }
    let Some(relative) = data_directory
        .parent()
        .and_then(|home| path.strip_prefix(home).ok())
    else {
        return false;
    };
    let mut components = relative.components();
    components.next() == Some(Component::Normal(OsStr::new(PRE_RENAME_DATA_DIRECTORY)))
        && components.next() == Some(Component::Normal(projects))
}

const CURRENT_DATA_DIRECTORY: &str = ".raku";
const PRE_RENAME_DATA_DIRECTORY: &str = ".waku";

pub fn needs_migration(path: &Path) -> bool {
    let Some(root) = workspace_root() else {
        return false;
    };
    !path.starts_with(&root)
        && root
            .parent()
            .is_some_and(|legacy_root| is_legacy_workspace_path(path, legacy_root))
}

pub fn is_legacy_root_path(path: &Path) -> bool {
    workspace_root()
        .is_some_and(|root| root.parent().is_some_and(|legacy_root| path == legacy_root))
}

fn is_legacy_workspace_path(path: &Path, legacy_root: &Path) -> bool {
    if path == legacy_root {
        return true;
    }
    let Some(date) = path
        .strip_prefix(legacy_root)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
    else {
        return false;
    };
    is_date_component(date)
}

fn is_date_component(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_rename_workspaces_classify_as_projectless() {
        let root = Path::new("/Users/me/.raku/projects");
        assert!(is_projectless_path_in(
            Path::new("/Users/me/.raku/projects/2026-11-24/new-chat"),
            root
        ));
        assert!(is_projectless_path_in(
            Path::new("/Users/me/.waku/projects/2026-10-25/new-chat-4"),
            root
        ));
        assert!(is_projectless_path_in(
            Path::new("/Users/me/.raku/2026-08-23/task"),
            root
        ));
        assert!(!is_projectless_path_in(
            Path::new("/Users/me/.waku/settings.json"),
            root
        ));
        assert!(!is_projectless_path_in(
            Path::new("/Users/other/.waku/projects/2026-10-25/new-chat"),
            root
        ));
        assert!(!is_projectless_path_in(
            Path::new("/Users/me/Projects/waku"),
            root
        ));
        // A custom root outside the default data directory has no alias.
        assert!(!is_projectless_path_in(
            Path::new("/srv/.waku/projects/2026-10-25/new-chat"),
            Path::new("/srv/workspaces/projects")
        ));
    }
}
