//! One-time move of data written before the app was renamed from Waku.
//!
//! The old directories are renamed in place, so the move is instant and never
//! copies transcripts or attachments. A symlink is left at each old path:
//! the database, provider sessions, and Git worktrees store absolute paths
//! such as `~/.waku/projects/...`, and those must keep resolving.

use std::fs;
use std::path::Path;

/// Moves `~/.waku` to `~/.raku` and, in Release, the per-user data
/// directory `Waku` to `Raku`. Each move happens only when the new location
/// does not exist yet, so this is a no-op on every launch after the first.
///
/// Debug never touches the Release data directory; it keeps its database in
/// the checkout. It still migrates `~/.raku` so a Debug launch cannot create
/// an empty `~/.raku` that would block the Release migration.
pub fn migrate() {
    if let Some(home) = dirs::home_dir() {
        migrate_directory(&home.join(".waku"), &home.join(".raku"));
    }
    if !cfg!(debug_assertions)
        && let Some(data) = dirs::data_local_dir()
    {
        migrate_directory(&data.join("Waku"), &data.join("Raku"));
    }
}

fn migrate_directory(old: &Path, new: &Path) {
    if fs::symlink_metadata(new).is_ok() {
        return;
    }
    // `symlink_metadata` reports a symlink as a non-directory, so an old path
    // that is already the compatibility link is left alone.
    let Ok(metadata) = fs::symlink_metadata(old) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    if let Err(error) = fs::rename(old, new) {
        eprintln!(
            "could not move {} to {}: {error}",
            old.display(),
            new.display()
        );
        return;
    }
    #[cfg(unix)]
    if let Err(error) = std::os::unix::fs::symlink(new, old) {
        eprintln!(
            "could not link {} to {}: {error}",
            old.display(),
            new.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::migrate_directory;

    #[test]
    fn moves_old_directory_and_leaves_a_link() {
        let root = std::env::temp_dir().join(format!("raku-legacy-{}", uuid::Uuid::new_v4()));
        let old = root.join(".waku");
        let new = root.join(".raku");
        std::fs::create_dir_all(old.join("projects")).unwrap();
        std::fs::write(old.join("app.json"), "{}").unwrap();

        migrate_directory(&old, &new);

        assert!(new.join("app.json").is_file());
        assert!(
            std::fs::symlink_metadata(&old)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(old.join("projects").is_dir());

        // A second launch leaves both paths as they are.
        migrate_directory(&old, &new);
        assert!(new.join("projects").is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn keeps_existing_new_directory() {
        let root = std::env::temp_dir().join(format!("raku-legacy-{}", uuid::Uuid::new_v4()));
        let old = root.join(".waku");
        let new = root.join(".raku");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();

        migrate_directory(&old, &new);

        assert!(std::fs::symlink_metadata(&old).unwrap().is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
