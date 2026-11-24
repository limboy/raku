//! Private workspaces for tasks that are not attached to a user project.
//!
//! Codex allocates ordinary projectless chats beneath a per-user root using
//! `<root>/<local date>/<prompt slug>`, with numeric collision suffixes and a
//! random fallback. Raku mirrors that layout beneath `~/.raku/projects` so
//! generated workspaces do not sit beside configuration documents.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use chrono::{Local, NaiveDate};
use uuid::Uuid;

const DEFAULT_SLUG: &str = "new-chat";
const MAX_SLUG_BYTES: usize = 80;
const MAX_NUMBERED_CANDIDATES: usize = 100;
const MAX_RANDOM_CANDIDATES: usize = 5;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
}

/// The daemon root is cached because `Project::is_projectless` is reached from
/// row builders and render paths. A remote client installs the daemon-host
/// value during task-state loading; callers then perform path compares only.
fn workspace_root_slot() -> &'static RwLock<Option<PathBuf>> {
    static ROOT: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();
    ROOT.get_or_init(|| {
        RwLock::new(dirs::home_dir().map(|home| home.join(".raku").join("projects")))
    })
}

pub fn set_workspace_root(root: Option<PathBuf>) {
    raku_protocol::projectless::set_workspace_root(root.clone());
    if let Ok(mut current) = workspace_root_slot().write() {
        *current = root;
    }
}

pub fn workspace_root() -> Option<PathBuf> {
    workspace_root_slot().read().ok()?.clone()
}

/// Home directory on the host that owns the configured projectless root.
/// Remote desktops use this only to abbreviate daemon paths for display.
pub fn home_directory() -> Option<PathBuf> {
    let root = workspace_root()?;
    root.parent()?.parent().map(Path::to_path_buf)
}

/// Existing builds created dated workspaces directly under `~/.raku`; keep
/// recognizing those paths while all new workspaces live under `projects/`.
pub fn is_projectless_path(path: &Path) -> bool {
    workspace_root()
        .is_some_and(|root| raku_protocol::projectless::is_projectless_path_in(path, &root))
}

/// Whether an existing projectless workspace still uses the pre-`projects/`
/// layout and should be moved by the daemon.
pub fn needs_migration(path: &Path) -> bool {
    let Some(root) = workspace_root() else {
        return false;
    };
    !path.starts_with(&root)
        && (root
            .parent()
            .is_some_and(|legacy_root| is_legacy_workspace_path(path, legacy_root))
            || raku_protocol::projectless::pre_rename_workspace_relative(path, &root).is_some())
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

pub fn is_legacy_root_path(path: &Path) -> bool {
    workspace_root()
        .is_some_and(|root| root.parent().is_some_and(|legacy_root| path == legacy_root))
}

/// Raku no longer keeps `~/.waku` as a link to `~/.raku`. Remove one an
/// earlier build left, but only when it is a symlink that resolves to the
/// current data directory and none of `recorded_paths` still lives under it.
/// Returns whether a link was removed.
pub fn remove_pre_rename_link<'a>(
    recorded_paths: impl IntoIterator<Item = &'a Path>,
) -> io::Result<bool> {
    let Some(root) = workspace_root() else {
        return Ok(false);
    };
    let Some(data_directory) = root.parent() else {
        return Ok(false);
    };
    let Some(home) = data_directory.parent() else {
        return Ok(false);
    };
    remove_pre_rename_link_in(
        &home.join(raku_protocol::projectless::PRE_RENAME_DATA_DIRECTORY),
        data_directory,
        recorded_paths,
    )
}

fn remove_pre_rename_link_in<'a>(
    link: &Path,
    data_directory: &Path,
    recorded_paths: impl IntoIterator<Item = &'a Path>,
) -> io::Result<bool> {
    match fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    if fs::canonicalize(link)? != fs::canonicalize(data_directory)? {
        return Ok(false);
    }
    if recorded_paths
        .into_iter()
        .any(|path| path.starts_with(link))
    {
        return Ok(false);
    }
    fs::remove_file(link)?;
    Ok(true)
}

pub fn create_workspace(prompt: Option<&str>) -> io::Result<Workspace> {
    let root = workspace_root().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate the home directory for ~/.raku/projects",
        )
    })?;
    create_workspace_in(&root, Local::now().date_naive(), None, prompt)
}

/// Move one old dated workspace from `~/.raku/<date>/<slug>` into
/// `~/.raku/projects/<date>/<slug>` without copying its contents through the
/// client. The oldest layout used `~/.raku` itself; that path contains Raku's
/// configuration now, so it receives a fresh private workspace instead of
/// moving the configuration directory.
pub fn migrate_workspace(path: &Path) -> io::Result<Workspace> {
    let root = workspace_root().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate the home directory for ~/.raku/projects",
        )
    })?;
    migrate_workspace_in(&root, path)
}

fn migrate_workspace_in(root: &Path, path: &Path) -> io::Result<Workspace> {
    if path.starts_with(root) {
        validate_real_directory(path)?;
        return Ok(Workspace {
            cwd: path.to_owned(),
            workspace_root: root.to_owned(),
        });
    }
    if let Some(relative) = raku_protocol::projectless::pre_rename_workspace_relative(path, root) {
        return migrate_pre_rename_workspace(root, path, relative);
    }
    let legacy_root = root.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid projectless workspace root",
        )
    })?;
    if path == legacy_root {
        return create_workspace_in(root, Local::now().date_naive(), None, None);
    }

    let relative = path.strip_prefix(legacy_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not a legacy projectless workspace",
        )
    })?;
    move_dated_workspace(root, path, relative)
}

/// The rename moved `~/.waku` to `~/.raku` wholesale, so a recorded
/// `~/.waku/projects/<date>/<slug>` normally names a directory that already
/// sits at the same place under the current root and only the record changes.
/// That holds whether the old path still resolves through a leftover link or
/// no longer exists. A real old directory that the rename never moved, because
/// `~/.raku` already existed, is moved like any other legacy workspace.
fn migrate_pre_rename_workspace(
    root: &Path,
    path: &Path,
    relative: &Path,
) -> io::Result<Workspace> {
    let destination = root.join(relative);
    let already_moved = match fs::canonicalize(path) {
        Ok(resolved) => fs::canonicalize(&destination).is_ok_and(|moved| moved == resolved),
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => return Err(error),
    };
    if already_moved {
        return Ok(Workspace {
            cwd: destination,
            workspace_root: root.to_owned(),
        });
    }
    move_dated_workspace(root, path, relative)
}

/// Move `<old root>/<date>/<slug>` to `<root>/<date>/<slug>`, adding a numeric
/// suffix when that name is taken.
fn move_dated_workspace(root: &Path, path: &Path, relative: &Path) -> io::Result<Workspace> {
    let components = relative.components().collect::<Vec<_>>();
    if components.len() != 2
        || components
            .iter()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        || !is_date_component(components[0].as_os_str().to_string_lossy().as_ref())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not a dated legacy projectless workspace",
        ));
    }
    validate_real_directory(path)?;
    ensure_real_directory(root)?;
    let date_directory = root.join(components[0].as_os_str());
    ensure_real_directory(&date_directory)?;
    let original_name = components[1].as_os_str().to_string_lossy();
    for index in 0..MAX_NUMBERED_CANDIDATES {
        let name = if index == 0 {
            original_name.to_string()
        } else {
            format!("{original_name}-{}", index + 1)
        };
        let destination = date_directory.join(name);
        if destination.exists() {
            continue;
        }
        match fs::rename(path, &destination) {
            Ok(()) => {
                return Ok(Workspace {
                    cwd: destination,
                    workspace_root: root.to_owned(),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to allocate a destination for the legacy projectless workspace",
    ))
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

fn workspace_slug(directory_name: Option<&str>, prompt: Option<&str>) -> String {
    let directory_name = directory_name.filter(|value| !value.trim().is_empty());
    let prompt = prompt.filter(|value| !value.trim().is_empty());
    let source = directory_name.or(prompt).unwrap_or_default().to_lowercase();
    let mut words = source
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty());
    let maximum_words = if directory_name.is_some() {
        usize::MAX
    } else {
        6
    };
    let mut slug = words
        .by_ref()
        .take(maximum_words)
        .collect::<Vec<_>>()
        .join("-");
    slug.truncate(MAX_SLUG_BYTES);
    if slug.is_empty() {
        DEFAULT_SLUG.to_owned()
    } else {
        slug
    }
}

fn create_workspace_in(
    root: &Path,
    date: NaiveDate,
    directory_name: Option<&str>,
    prompt: Option<&str>,
) -> io::Result<Workspace> {
    ensure_real_directory(root)?;
    let date_directory = root.join(date.format("%Y-%m-%d").to_string());
    ensure_real_directory(&date_directory)?;
    let slug = workspace_slug(directory_name, prompt);

    for index in 0..MAX_NUMBERED_CANDIDATES {
        let name = if index == 0 {
            slug.clone()
        } else {
            format!("{slug}-{}", index + 1)
        };
        if let Some(workspace) = try_create_workspace(root, &date_directory, &name)? {
            return Ok(workspace);
        }
    }

    for _ in 0..MAX_RANDOM_CANDIDATES {
        let name = format!("{slug}-{}", Uuid::new_v4());
        if let Some(workspace) = try_create_workspace(root, &date_directory, &name)? {
            return Ok(workspace);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to create a unique projectless task directory",
    ))
}

fn try_create_workspace(
    root: &Path,
    date_directory: &Path,
    name: &str,
) -> io::Result<Option<Workspace>> {
    let cwd = date_directory.join(name);
    match fs::create_dir(&cwd) {
        Ok(()) => Ok(Some(Workspace {
            cwd,
            workspace_root: root.to_owned(),
        })),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            validate_real_directory(&cwd)?;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn ensure_real_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_real_directory(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            validate_real_directory(path)
        }
        Err(error) => Err(error),
    }
}

fn validate_real_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "projectless task directory must be a real directory",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!("raku-projectless-{}", Uuid::new_v4()))
    }

    #[test]
    fn prompt_slug_matches_codex_word_and_length_rules() {
        assert_eq!(
            workspace_slug(
                None,
                Some("Build A polished, LOCAL coding agent please now")
            ),
            "build-a-polished-local-coding-agent"
        );
        assert_eq!(workspace_slug(None, Some("你好 👋")), "new-chat");
        assert_eq!(
            workspace_slug(Some("Release Candidate Number 12"), Some("ignored prompt")),
            "release-candidate-number-12"
        );
        assert_eq!(
            workspace_slug(Some("  "), Some("Prompt fallback")),
            "prompt-fallback"
        );
        assert_eq!(
            workspace_slug(Some(&"a".repeat(100)), None).len(),
            MAX_SLUG_BYTES
        );
    }

    #[test]
    fn creates_date_and_unique_task_directories_without_split_folders() {
        let root = test_root();
        let date = NaiveDate::from_ymd_opt(2026, 8, 8).unwrap();

        let first = create_workspace_in(&root, date, None, Some("Fix projectless sessions"))
            .expect("first workspace");
        let second = create_workspace_in(&root, date, None, Some("Fix projectless sessions"))
            .expect("second workspace");

        assert_eq!(first.cwd, root.join("2026-08-08/fix-projectless-sessions"));
        assert_eq!(
            second.cwd,
            root.join("2026-08-08/fix-projectless-sessions-2")
        );
        assert_eq!(first.workspace_root, root);
        assert_eq!(fs::read_dir(&first.cwd).unwrap().count(), 0);

        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlinked_workspace_root() {
        use std::os::unix::fs::symlink;

        let parent = test_root();
        let target = parent.join("target");
        let root = parent.join("root");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &root).unwrap();

        let error = create_workspace_in(
            &root,
            NaiveDate::from_ymd_opt(2026, 8, 8).unwrap(),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        fs::remove_dir_all(&parent).ok();
    }

    #[test]
    fn rewrites_pre_rename_workspaces_that_the_rename_already_moved() {
        let home = test_root();
        let root = home.join(".raku/projects");
        let moved = root.join("2026-10-25/new-chat-4");
        fs::create_dir_all(&moved).unwrap();
        fs::write(moved.join("notes.txt"), "kept").unwrap();
        let recorded = home.join(".waku/projects/2026-10-25/new-chat-4");

        // The old path no longer exists.
        let migrated = migrate_workspace_in(&root, &recorded).unwrap();
        assert_eq!(migrated.cwd, moved);

        // The old path resolves through a leftover link to the same place.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(home.join(".raku"), home.join(".waku")).unwrap();
            let migrated = migrate_workspace_in(&root, &recorded).unwrap();
            assert_eq!(migrated.cwd, moved);
        }
        assert_eq!(fs::read_to_string(moved.join("notes.txt")).unwrap(), "kept");
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn moves_a_pre_rename_workspace_the_rename_left_behind() {
        let home = test_root();
        let root = home.join(".raku/projects");
        fs::create_dir_all(root.join("2026-10-25/new-chat")).unwrap();
        let recorded = home.join(".waku/projects/2026-10-25/new-chat");
        fs::create_dir_all(&recorded).unwrap();
        fs::write(recorded.join("notes.txt"), "old").unwrap();

        let migrated = migrate_workspace_in(&root, &recorded).unwrap();

        assert_eq!(migrated.cwd, root.join("2026-10-25/new-chat-2"));
        assert_eq!(
            fs::read_to_string(migrated.cwd.join("notes.txt")).unwrap(),
            "old"
        );
        assert!(!recorded.exists());
        fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn removes_the_pre_rename_link_only_when_nothing_recorded_uses_it() {
        let home = test_root();
        let data_directory = home.join(".raku");
        let link = home.join(".waku");
        fs::create_dir_all(data_directory.join("projects/2026-10-25/new-chat")).unwrap();
        std::os::unix::fs::symlink(&data_directory, &link).unwrap();
        let old = link.join("projects/2026-10-25/new-chat");
        let current = data_directory.join("projects/2026-10-25/new-chat");

        assert!(!remove_pre_rename_link_in(&link, &data_directory, [old.as_path()]).unwrap());
        assert!(fs::symlink_metadata(&link).is_ok());

        assert!(remove_pre_rename_link_in(&link, &data_directory, [current.as_path()]).unwrap());
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(current.is_dir());

        // A link elsewhere and a real directory are both left alone.
        let elsewhere = home.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(!remove_pre_rename_link_in(&link, &data_directory, []).unwrap());
        fs::remove_file(&link).unwrap();
        fs::create_dir_all(&link).unwrap();
        assert!(!remove_pre_rename_link_in(&link, &data_directory, []).unwrap());
        assert!(link.is_dir());
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn migrates_legacy_workspace_with_contents_under_projects() {
        let legacy_root = test_root();
        let root = legacy_root.join("projects");
        let legacy = legacy_root.join("2026-08-08/fix-projectless-sessions");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("notes.txt"), "kept").unwrap();

        let migrated = migrate_workspace_in(&root, &legacy).unwrap();

        assert_eq!(
            migrated.cwd,
            root.join("2026-08-08/fix-projectless-sessions")
        );
        assert_eq!(
            fs::read_to_string(migrated.cwd.join("notes.txt")).unwrap(),
            "kept"
        );
        assert!(!legacy.exists());
        fs::remove_dir_all(&legacy_root).ok();
    }
}
