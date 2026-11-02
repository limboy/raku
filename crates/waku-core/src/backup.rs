//! Checked, portable snapshots and an offline, recoverable restore handoff.
//!
//! Only Waku-owned data is included. Project working trees and provider login
//! directories are intentionally outside this format. All entry points do I/O
//! and must run on a worker, never in a render or measurement path.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, ensure};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;
use waku_protocol::backup::{
    AUTOMATIC_BACKUP_INTERVAL, AUTOMATIC_BACKUP_LIMIT, BackupInfo, BackupKind, DesktopBackup,
    PENDING_RESTORE, READY_RESTORE, RESTORED_DESKTOP, next_automatic_backup_at,
};

use crate::persistence::ComposerDrafts;
use crate::settings::DaemonSettings;

const MANIFEST: &str = "manifest.json";
const FORMAT_VERSION: u32 = 1;
const JOURNAL: &str = ".restore-journal.json";
const RESTORE_NAMES: [&str; 8] = [
    "app.db",
    "app.db-wal",
    "app.db-shm",
    "blobs",
    "attachments",
    "composer-drafts.json",
    "settings.json",
    RESTORED_DESKTOP,
];

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    format: String,
    version: u32,
    #[serde(default)]
    kind: BackupKind,
    created_at: u64,
    project_count: usize,
    session_count: usize,
    files: Vec<BackupFile>,
}

#[derive(Deserialize, Serialize)]
struct BackupFile {
    path: String,
    size: u64,
    sha256: String,
}

impl Manifest {
    fn info(&self, path: PathBuf) -> BackupInfo {
        BackupInfo {
            kind: self.kind,
            path,
            created_at: self.created_at,
            size_bytes: self.files.iter().map(|file| file.size).sum(),
            project_count: self.project_count,
            session_count: self.session_count,
        }
    }
}

fn private_directory(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_file(path: &Path) -> anyhow::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn write_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let mut file = private_file(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.sync_all()?;
    sync_parent(path)?;
    Ok(())
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "not a regular file: {}",
        path.display()
    );
    ensure!(
        fs::metadata(path)?.len() <= 16 * 1024 * 1024,
        "backup metadata is too large"
    );
    match serde_json::from_reader(File::open(path)?) {
        Ok(value) => Ok(value),
        Err(error) if error.is_data() => Ok(waku_protocol::persistence::from_persisted_json(
            &fs::read(path)?,
        )?),
        Err(error) => Err(error.into()),
    }
}

/// Streams file contents with a bounded buffer, also used to check a backup.
fn digest_file(path: &Path, destination: Option<&Path>) -> anyhow::Result<(u64, String)> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "backup contains a symbolic link or special file: {}",
        path.display()
    );
    let mut source = File::open(path)?;
    let mut output = destination.map(private_file).transpose()?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut size = 0;
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        if let Some(output) = &mut output {
            output.write_all(&buffer[..count])?;
        }
        size += count as u64;
    }
    if let Some(output) = output {
        output.sync_all()?;
    }
    Ok((size, format!("{:x}", digest.finalize())))
}

fn copy_tree(source: &Path, destination: &Path, depth: usize) -> anyhow::Result<()> {
    ensure!(depth < 128, "backup directory nesting is too deep");
    let kind = fs::symlink_metadata(source)?.file_type();
    if kind.is_dir() {
        private_directory(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(
                &entry.path(),
                &destination.join(entry.file_name()),
                depth + 1,
            )?;
        }
    } else {
        digest_file(source, Some(destination))?;
    }
    Ok(())
}

fn inventory(root: &Path, directory: &Path, files: &mut Vec<BackupFile>) -> anyhow::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            inventory(root, &path, files)?;
        } else {
            let (size, sha256) = digest_file(&path, None)?;
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .context("backup filename is not UTF-8")?
                .replace('\\', "/");
            files.push(BackupFile {
                path: relative,
                size,
                sha256,
            });
        }
    }
    Ok(())
}

fn read_manifest(path: &Path) -> anyhow::Result<Manifest> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "select a Waku backup folder"
    );
    let manifest: Manifest = read_json(&path.join(MANIFEST)).context("not a Waku backup")?;
    ensure!(
        manifest.format == "waku-backup" && manifest.version == FORMAT_VERSION,
        "unsupported Waku backup version"
    );
    ensure!(
        manifest.files.len() <= 100_000,
        "backup contains too many files"
    );
    let mut names = HashSet::new();
    let mut total = 0u64;
    for file in &manifest.files {
        let parts = file.path.split('/').collect::<Vec<_>>();
        ensure!(
            parts.iter().all(|part| !part.is_empty()
                && *part != "."
                && *part != ".."
                && !part.contains(['\\', ':'])),
            "invalid path in backup"
        );
        ensure!(
            matches!(parts[0], "blobs" | "attachments") && parts.len() > 1
                || parts.len() == 1
                    && matches!(
                        parts[0],
                        "app.db" | "settings.json" | "desktop.json" | "composer-drafts.json"
                    ),
            "unexpected file in backup"
        );
        ensure!(names.insert(&file.path), "duplicate file in backup");
        total = total
            .checked_add(file.size)
            .context("backup size overflow")?;
    }
    for required in [
        "app.db",
        "settings.json",
        "desktop.json",
        "composer-drafts.json",
    ] {
        ensure!(
            names.contains(&required.to_owned()),
            "backup is missing {required}"
        );
    }
    Ok(manifest)
}

fn checked_file(root: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    let mut path = root.to_owned();
    for part in relative.split('/') {
        path.push(part);
        ensure!(
            !fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "backup contains a symbolic link"
        );
    }
    Ok(path)
}

fn validate(path: &Path) -> anyhow::Result<Manifest> {
    let manifest = read_manifest(path)?;
    for sidecar in ["app.db-wal", "app.db-shm", "app.db-journal"] {
        ensure!(
            !path.join(sidecar).try_exists()?,
            "backup must contain a standalone database without sidecars"
        );
    }
    for file in &manifest.files {
        let (size, hash) = digest_file(&checked_file(path, &file.path)?, None)?;
        ensure!(
            size == file.size && hash == file.sha256,
            "backup file failed integrity check: {}",
            file.path
        );
    }
    let _: DaemonSettings = read_json(&path.join("settings.json"))?;
    let _: ComposerDrafts = read_json(&path.join("composer-drafts.json"))?;
    let desktop: DesktopBackup = read_json(&path.join("desktop.json"))?;
    ensure!(
        desktop.settings.is_object() && desktop.state.is_object(),
        "invalid desktop settings in backup"
    );
    let database =
        Connection::open_with_flags(path.join("app.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let check: String = database.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    ensure!(
        check == "ok",
        "backup database failed integrity check: {check}"
    );
    crate::persistence::validate_backup_schema(&database)?;
    ensure!(
        database.query_row("SELECT COUNT(*) FROM projects", [], |row| row
            .get::<_, usize>(0))?
            == manifest.project_count,
        "backup project count does not match"
    );
    ensure!(
        database.query_row("SELECT COUNT(*) FROM sessions", [], |row| row
            .get::<_, usize>(0))?
            == manifest.session_count,
        "backup session count does not match"
    );
    Ok(manifest)
}

pub fn list(directory: &Path) -> anyhow::Result<Vec<BackupInfo>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut backups = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "waku-backup")
            && let Ok(manifest) = read_manifest(&path)
        {
            backups.push(manifest.info(path));
        }
    }
    backups.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.path.cmp(&a.path))
    });
    Ok(backups)
}

fn outside_data(directory: &Path, database: &Path) -> anyhow::Result<PathBuf> {
    ensure!(
        directory.is_absolute(),
        "choose an absolute backup directory"
    );
    fs::create_dir_all(directory)?;
    let directory = fs::canonicalize(directory)?;
    let data = fs::canonicalize(database.parent().context("database has no directory")?)?;
    ensure!(
        !directory.starts_with(&data),
        "choose a backup directory outside Waku's data directory"
    );
    Ok(directory)
}

pub fn create(
    database: &Path,
    settings: &DaemonSettings,
    directory: &Path,
    desktop: &DesktopBackup,
) -> anyhow::Result<BackupInfo> {
    create_with_kind(
        database,
        settings,
        directory,
        desktop,
        BackupKind::Manual,
        crate::model::unix_time(),
    )
}

fn create_with_kind(
    database: &Path,
    settings: &DaemonSettings,
    directory: &Path,
    desktop: &DesktopBackup,
    kind: BackupKind,
    created_at: u64,
) -> anyhow::Result<BackupInfo> {
    let directory = outside_data(directory, database)?;
    let id = Uuid::new_v4().simple().to_string();
    let staging = directory.join(format!(".waku-backup-{id}.tmp"));
    private_directory(&staging)?;
    let result = (|| {
        let source = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        source.busy_timeout(Duration::from_secs(5))?;
        let mut destination = Connection::open(staging.join("app.db"))?;
        rusqlite::backup::Backup::new(&source, &mut destination)?.run_to_completion(
            256,
            Duration::from_millis(1),
            None,
        )?;
        // Backups are self contained; no WAL sidecars are needed to read them.
        destination.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        let project_count =
            destination.query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))?;
        let session_count =
            destination.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        drop(destination);
        File::open(staging.join("app.db"))?.sync_all()?;
        let data = database.parent().context("database has no directory")?;
        for name in ["blobs", "attachments"] {
            let source = data.join(name);
            if source.try_exists()? {
                copy_tree(&source, &staging.join(name), 0)?;
            }
        }
        let drafts_path = data.join("composer-drafts.json");
        let drafts = if drafts_path.try_exists()? {
            read_json::<ComposerDrafts>(&drafts_path)?
        } else {
            ComposerDrafts::default()
        };
        write_json(&staging.join("composer-drafts.json"), &drafts)?;
        write_json(&staging.join("settings.json"), settings)?;
        write_json(&staging.join("desktop.json"), desktop)?;
        let mut files = Vec::new();
        inventory(&staging, &staging, &mut files)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let manifest = Manifest {
            format: "waku-backup".into(),
            version: FORMAT_VERSION,
            kind,
            created_at,
            project_count,
            session_count,
            files,
        };
        write_json(&staging.join(MANIFEST), &manifest)?;
        validate(&staging)?;
        let name = format!(
            "Waku-{}-{}.waku-backup",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            &id[..8]
        );
        let path = directory.join(name);
        fs::rename(&staging, &path)?;
        sync_parent(&path)?;
        Ok(manifest.info(path))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

/// Called under the backend's backup lock. Recheck the persisted snapshots
/// so restarts or concurrent clients cannot create duplicate daily copies.
pub fn create_automatic(
    database: &Path,
    settings: &DaemonSettings,
    directory: &Path,
    desktop: &DesktopBackup,
    now: u64,
) -> anyhow::Result<(Option<BackupInfo>, u64)> {
    let data = database.parent().context("database has no directory")?;
    ensure!(
        !data.join(PENDING_RESTORE).try_exists()? && !data.join(READY_RESTORE).try_exists()?,
        "automatic backups are paused while a restore is scheduled"
    );
    let directory = outside_data(directory, database)?;
    let previous = list(&directory)?;
    let next = next_automatic_backup_at(&previous);
    if now < next
        && (previous.len() <= AUTOMATIC_BACKUP_LIMIT || validate(&previous[0].path).is_ok())
    {
        // Also finish retention if a previous successful copy was published
        // but its cleanup failed. Do not generate another copy on retry.
        prune_backups(&previous, &previous[0].path)?;
        return Ok((None, next));
    }
    let backup = create_with_kind(
        database,
        settings,
        &directory,
        desktop,
        BackupKind::Automatic,
        now,
    )?;
    // Publish and validate the replacement before deleting any older copy.
    prune_backups(&list(&directory)?, &backup.path)?;
    Ok((Some(backup), now.saturating_add(AUTOMATIC_BACKUP_INTERVAL)))
}

fn prune_backups(backups: &[BackupInfo], keep: &Path) -> anyhow::Result<()> {
    // Keep the verified replacement explicitly: a damaged snapshot with a
    // future timestamp must never displace the copy we just validated.
    ensure!(
        backups.iter().any(|backup| backup.path.as_path() == keep),
        "backup disappeared before cleanup"
    );
    for backup in backups
        .iter()
        .filter(|backup| backup.path.as_path() != keep)
    {
        fs::remove_dir_all(&backup.path)
            .with_context(|| format!("could not remove old backup {}", backup.path.display()))?;
        sync_parent(&backup.path)?;
    }
    Ok(())
}

/// Copy before publishing the handoff: removing an external disk after this
/// succeeds cannot strand the next launch without its restore source.
pub fn stage_restore(database: &Path, path: &Path) -> anyhow::Result<BackupInfo> {
    let manifest = validate(path)?;
    let data = database.parent().context("database has no directory")?;
    ensure!(
        !data.join(PENDING_RESTORE).try_exists()? && !data.join(READY_RESTORE).try_exists()?,
        "a restore is already scheduled for the next launch"
    );
    let staging = data.join(format!(".restore-stage-{}", Uuid::new_v4().simple()));
    private_directory(&staging)?;
    let result = (|| {
        for file in &manifest.files {
            digest_file(
                &checked_file(path, &file.path)?,
                Some(&staging.join(&file.path)),
            )?;
        }
        write_json(&staging.join(MANIFEST), &manifest)?;
        validate(&staging)?;
        // Check migrations and the actual load path on our private copy. The
        // external snapshot remains untouched and older schemas still work.
        crate::persistence::StateStore::daemon(staging.join("app.db")).load()?;
        // Loading can migrate the staged DB. Recreate its original bytes so
        // the handoff's checksums remain valid for startup validation.
        remove_if_exists(&staging.join("app.db"))?;
        remove_if_exists(&staging.join("app.db-wal"))?;
        remove_if_exists(&staging.join("app.db-shm"))?;
        digest_file(&path.join("app.db"), Some(&staging.join("app.db")))?;
        validate(&staging)?;
        // The offline installer journals the original files and rolls them
        // back if replacement fails. Staging does not create another visible
        // backup in the selected directory.
        fs::rename(&staging, data.join(PENDING_RESTORE))?;
        if let Err(error) = sync_parent(&data.join(PENDING_RESTORE)) {
            remove_if_exists(&data.join(PENDING_RESTORE))?;
            return Err(error);
        }
        Ok(manifest.info(path.to_owned()))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

pub fn cancel_restore(database: &Path) -> anyhow::Result<()> {
    let data = database.parent().context("database has no directory")?;
    ensure!(
        !data.join(READY_RESTORE).try_exists()?,
        "restore is already being applied"
    );
    remove_if_exists(&data.join(PENDING_RESTORE))
}

#[derive(Deserialize, Serialize)]
struct RestoreJournal {
    id: Uuid,
    committed: bool,
    entries: Vec<RestoreEntry>,
}

#[derive(Deserialize, Serialize)]
struct RestoreEntry {
    name: String,
    existed: bool,
}

fn target_path(data: &Path, settings: &Path, name: &str) -> anyhow::Result<PathBuf> {
    ensure!(
        RESTORE_NAMES.contains(&name),
        "invalid restore journal entry"
    );
    Ok(if name == "settings.json" {
        settings.to_owned()
    } else {
        data.join(name)
    })
}

fn sibling(path: &Path, id: Uuid, kind: &str) -> PathBuf {
    path.with_file_name(format!(
        ".waku-restore-{kind}-{}-{}",
        id.simple(),
        path.file_name().unwrap_or_default().to_string_lossy()
    ))
}

fn remove_if_exists(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn finish_journal(data: &Path, settings: &Path, journal: &RestoreJournal) -> anyhow::Result<()> {
    for entry in &journal.entries {
        let target = target_path(data, settings, &entry.name)?;
        let previous = sibling(&target, journal.id, "old");
        let incoming = sibling(&target, journal.id, "new");
        if journal.committed {
            remove_if_exists(&previous)?;
        } else if previous.try_exists()? {
            remove_if_exists(&target)?;
            fs::rename(&previous, &target)?;
        } else if !entry.existed {
            remove_if_exists(&target)?;
        }
        remove_if_exists(&incoming)?;
        sync_parent(&target)?;
    }
    // A committed restore is never applied twice, including after a crash
    // during cleanup. An uncommitted one leaves its checked source for retry.
    if journal.committed {
        remove_if_exists(&data.join(READY_RESTORE))?;
    }
    remove_if_exists(&data.join(JOURNAL))?;
    sync_parent(&data.join(JOURNAL))
}

/// Runs before opening SQLite or settings. A desktop launch arms this handoff;
/// an independent dev-watcher daemon restart does not apply pending restores.
pub fn apply_ready_restore(database: &Path, settings: &Path) -> anyhow::Result<bool> {
    let data = database.parent().context("database has no directory")?;
    if data.join(JOURNAL).try_exists()? {
        let journal = read_json(&data.join(JOURNAL))?;
        finish_journal(data, settings, &journal)?;
    }
    let ready = data.join(READY_RESTORE);
    if !ready.try_exists()? {
        return Ok(false);
    }
    validate(&ready)?;
    let mut journal = RestoreJournal {
        id: Uuid::new_v4(),
        committed: false,
        entries: Vec::new(),
    };
    let prepared = (|| {
        for name in RESTORE_NAMES {
            let target = target_path(data, settings, name)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let incoming = sibling(&target, journal.id, "new");
            journal.entries.push(RestoreEntry {
                name: name.into(),
                existed: target.try_exists()?,
            });
            let source = ready.join(if name == RESTORED_DESKTOP {
                "desktop.json"
            } else {
                name
            });
            if source.try_exists()? {
                copy_tree(&source, &incoming, 0)?;
            } else if matches!(name, "blobs" | "attachments") {
                private_directory(&incoming)?;
            }
        }
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(error) = prepared {
        for entry in &journal.entries {
            let target = target_path(data, settings, &entry.name)?;
            let _ = remove_if_exists(&sibling(&target, journal.id, "new"));
        }
        return Err(error);
    }
    write_json(&data.join(JOURNAL), &journal)?;
    let mut committed = false;
    let installed = (|| {
        for entry in &journal.entries {
            let target = target_path(data, settings, &entry.name)?;
            let incoming = sibling(&target, journal.id, "new");
            if entry.existed {
                fs::rename(&target, sibling(&target, journal.id, "old"))?;
                sync_parent(&target)?;
            }
            if incoming.try_exists()? {
                fs::rename(&incoming, &target)?;
                sync_parent(&target)?;
            }
        }
        journal.committed = true;
        let temporary = data.join(format!(".restore-commit-{}", journal.id.simple()));
        write_json(&temporary, &journal)?;
        fs::rename(temporary, data.join(JOURNAL))?;
        committed = true;
        sync_parent(&data.join(JOURNAL))?;
        Ok::<_, anyhow::Error>(())
    })();
    journal.committed = committed;
    finish_journal(data, settings, &journal)?;
    if committed {
        // Cleanup synced the installed entries and journal directory again.
        // A transient failure syncing the commit cannot undo that commit.
        if let Err(error) = installed {
            eprintln!("Waku completed restore after retrying directory sync: {error:#}");
        }
    } else {
        installed?;
    }
    Ok(true)
}

/// A rejected handoff must not prevent the original installation from
/// opening. Only continue when rollback has finished (no journal remains).
/// Keep failed input for inspection and hand the failure to the desktop.
pub fn restore_on_startup(database: &Path, settings: &Path) -> anyhow::Result<()> {
    if let Err(error) = apply_ready_restore(database, settings) {
        let data = database.parent().context("database has no directory")?;
        ensure!(
            !data.join(JOURNAL).try_exists()?,
            "restore recovery is incomplete: {error:#}"
        );
        let ready = data.join(READY_RESTORE);
        if ready.try_exists()? {
            fs::rename(
                ready,
                data.join(format!(".failed-restore-{}", Uuid::new_v4().simple())),
            )?;
        }
        let error_path = data.join(waku_protocol::backup::RESTORE_ERROR);
        remove_if_exists(&error_path)?;
        write_json(&error_path, &format!("{error:#}"))?;
        eprintln!("Waku kept the original data after a failed restore: {error:#}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{PersistedState, StateStore};
    use serde_json::json;

    struct Fixture {
        root: PathBuf,
        database: PathBuf,
        settings: PathBuf,
        backups: PathBuf,
        desktop: DesktopBackup,
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("waku-backup-test-{}", Uuid::new_v4().simple()));
            let database = root.join("data/app.db");
            let settings = root.join("config/settings.json");
            let backups = root.join("backups");
            fs::create_dir_all(database.parent().unwrap()).unwrap();
            fs::create_dir_all(settings.parent().unwrap()).unwrap();
            fs::write(&settings, b"{\"disabled_providers\":[]}").unwrap();
            let store = StateStore::daemon(database.clone());
            let mut state = PersistedState::fresh(PathBuf::from("/workspace/example"));
            state.sessions[0].begin_turn("Keep this conversation");
            store.save(&mut state).unwrap();
            let data = database.parent().unwrap();
            fs::create_dir_all(data.join("blobs")).unwrap();
            fs::write(data.join("blobs/image.png"), b"original image bytes").unwrap();
            fs::create_dir_all(data.join("attachments/example/nested")).unwrap();
            fs::write(
                data.join("attachments/example/nested/file.txt"),
                b"original attachment",
            )
            .unwrap();
            fs::write(data.join("composer-drafts.json"), b"{}").unwrap();
            Self {
                root,
                database,
                settings,
                backups,
                desktop: DesktopBackup {
                    settings: json!({"theme":"dark"}),
                    state: json!({"app_state_version":1}),
                },
            }
        }

        fn create(&self) -> BackupInfo {
            create(
                &self.database,
                &DaemonSettings::default(),
                &self.backups,
                &self.desktop,
            )
            .unwrap()
        }

        fn arm(&self) {
            fs::rename(
                self.database.with_file_name(PENDING_RESTORE),
                self.database.with_file_name(READY_RESTORE),
            )
            .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn automatic_backups_run_every_24_hours_and_keep_only_the_latest() {
        let fixture = Fixture::new();
        let start = crate::model::unix_time();
        let run = |now| {
            create_automatic(
                &fixture.database,
                &DaemonSettings::default(),
                &fixture.backups,
                &fixture.desktop,
                now,
            )
            .unwrap()
        };
        let (first, next) = run(start);
        let first = first.unwrap();
        assert_eq!(first.kind, BackupKind::Automatic);
        assert_eq!(next, start + AUTOMATIC_BACKUP_INTERVAL);
        assert!(run(next - 1).0.is_none());
        let second = run(next).0.unwrap();
        validate(&second.path).unwrap();
        assert!(!first.path.exists());
        let third = run(next + AUTOMATIC_BACKUP_INTERVAL).0.unwrap();
        let backups = list(&fixture.backups).unwrap();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::canonicalize(&backups[0].path).unwrap(), third.path);
        assert!(!second.path.exists());
        validate(&third.path).unwrap();
        // Restarting the app or moving the clock backwards cannot produce
        // another premature daily copy.
        assert!(run(start).0.is_none());
        assert_eq!(list(&fixture.backups).unwrap().len(), 1);
    }

    #[test]
    fn migrates_old_backup_formats_to_one_copy_and_keeps_unrelated_files() {
        let fixture = Fixture::new();
        let manual = fixture.create();
        let legacy = fixture.create();
        let manifest_path = legacy.path.join(MANIFEST);
        let mut manifest: serde_json::Value = read_json(&manifest_path).unwrap();
        manifest.as_object_mut().unwrap().remove("kind");
        fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert_eq!(
            read_manifest(&legacy.path).unwrap().kind,
            BackupKind::Manual
        );
        let start = crate::model::unix_time();
        let safety = create_with_kind(
            &fixture.database,
            &DaemonSettings::default(),
            &fixture.backups,
            &fixture.desktop,
            BackupKind::Safety,
            start,
        )
        .unwrap();
        fs::write(fixture.backups.join("notes.txt"), b"keep this file").unwrap();
        let unrelated = fixture.backups.join("other.waku-backup");
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("important.txt"), b"not a Waku snapshot").unwrap();
        for day in 0..3 {
            create_automatic(
                &fixture.database,
                &DaemonSettings::default(),
                &fixture.backups,
                &fixture.desktop,
                start + 3 + day * AUTOMATIC_BACKUP_INTERVAL,
            )
            .unwrap();
        }
        let backups = list(&fixture.backups).unwrap();
        assert_eq!(backups.len(), 1);
        assert_eq!(backups[0].kind, BackupKind::Automatic);
        validate(&backups[0].path).unwrap();
        for old in [&manual, &legacy, &safety] {
            assert!(!old.path.exists());
        }
        assert_eq!(
            fs::read(fixture.backups.join("notes.txt")).unwrap(),
            b"keep this file"
        );
        assert!(unrelated.join("important.txt").exists());
    }

    #[test]
    fn failed_automatic_backup_does_not_delete_the_existing_copy() {
        let fixture = Fixture::new();
        let start = crate::model::unix_time();
        for day in 0..2 {
            create_automatic(
                &fixture.database,
                &DaemonSettings::default(),
                &fixture.backups,
                &fixture.desktop,
                start + day * AUTOMATIC_BACKUP_INTERVAL,
            )
            .unwrap();
        }
        let previous = list(&fixture.backups).unwrap();
        assert_eq!(previous.len(), 1);
        let connection = Connection::open(&fixture.database).unwrap();
        connection
            .execute(
                "INSERT INTO migrations(tag, applied_at) VALUES('9999_future', 1)",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(
            create_automatic(
                &fixture.database,
                &DaemonSettings::default(),
                &fixture.backups,
                &fixture.desktop,
                start + 2 * AUTOMATIC_BACKUP_INTERVAL,
            )
            .is_err()
        );
        assert_eq!(list(&fixture.backups).unwrap().len(), 1);
        validate(&previous[0].path).unwrap();
    }

    #[test]
    fn retries_retention_without_creating_a_duplicate_daily_backup() {
        let fixture = Fixture::new();
        let start = crate::model::unix_time();
        // Simulate snapshots published before an interrupted cleanup.
        for day in 0..3 {
            create_with_kind(
                &fixture.database,
                &DaemonSettings::default(),
                &fixture.backups,
                &fixture.desktop,
                BackupKind::Automatic,
                start + day * AUTOMATIC_BACKUP_INTERVAL,
            )
            .unwrap();
        }
        let (created, next) = create_automatic(
            &fixture.database,
            &DaemonSettings::default(),
            &fixture.backups,
            &fixture.desktop,
            start + 2 * AUTOMATIC_BACKUP_INTERVAL + 1,
        )
        .unwrap();
        assert!(created.is_none());
        assert_eq!(next, start + 3 * AUTOMATIC_BACKUP_INTERVAL);
        assert_eq!(list(&fixture.backups).unwrap().len(), 1);
    }

    #[test]
    fn replaces_a_corrupt_latest_copy_before_pruning_the_good_older_backup() {
        let fixture = Fixture::new();
        let start = crate::model::unix_time();
        let old = create_with_kind(
            &fixture.database,
            &DaemonSettings::default(),
            &fixture.backups,
            &fixture.desktop,
            BackupKind::Automatic,
            start,
        )
        .unwrap();
        let corrupt = create_with_kind(
            &fixture.database,
            &DaemonSettings::default(),
            &fixture.backups,
            &fixture.desktop,
            BackupKind::Automatic,
            start + AUTOMATIC_BACKUP_INTERVAL,
        )
        .unwrap();
        fs::write(corrupt.path.join("blobs/image.png"), b"broken").unwrap();
        let (replacement, _) = create_automatic(
            &fixture.database,
            &DaemonSettings::default(),
            &fixture.backups,
            &fixture.desktop,
            start + 2,
        )
        .unwrap();
        let replacement = replacement.expect("a valid replacement before pruning");
        validate(&replacement.path).unwrap();
        assert!(!old.path.exists());
        assert!(!corrupt.path.exists());
        assert_eq!(list(&fixture.backups).unwrap().len(), 1);
    }

    #[test]
    fn captures_uncheckpointed_wal_and_publishes_independent_snapshots() {
        let fixture = Fixture::new();
        let connection = Connection::open(&fixture.database).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; UPDATE projects SET name='Saved in WAL';").unwrap();
        assert!(fixture.database.with_file_name("app.db-wal").exists());
        let first = fixture.create();
        let second = fixture.create();
        assert_ne!(first.path, second.path);
        assert_eq!(first.session_count, 1);
        assert_eq!(first.project_count, 1);
        let backup = Connection::open(first.path.join("app.db")).unwrap();
        assert_eq!(
            backup
                .query_row("SELECT name FROM projects", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "Saved in WAL"
        );
        assert_eq!(list(&fixture.backups).unwrap().len(), 2);
        assert!(!first.path.join("app.db-wal").exists());
        validate(&first.path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&first.path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn restores_sessions_assets_drafts_and_settings_without_an_extra_backup() {
        let fixture = Fixture::new();
        let original = fixture.create();
        let connection = Connection::open(&fixture.database).unwrap();
        connection
            .execute("UPDATE projects SET name='Current data'", [])
            .unwrap();
        drop(connection);
        let data = fixture.database.parent().unwrap();
        fs::write(data.join("blobs/image.png"), b"changed image bytes").unwrap();
        fs::write(data.join("blobs/extra.png"), b"remove on restore").unwrap();
        let staged = stage_restore(&fixture.database, &original.path).unwrap();
        assert_eq!(staged.path, original.path);
        assert_eq!(list(&fixture.backups).unwrap().len(), 1);
        validate(&original.path).unwrap();
        // A daemon-only rebuild must not replace a live desktop's data.
        assert!(!apply_ready_restore(&fixture.database, &fixture.settings).unwrap());
        assert!(data.join(PENDING_RESTORE).exists());
        // The checked handoff survives removal of the external source.
        fs::remove_dir_all(&original.path).unwrap();
        fixture.arm();
        fs::write(data.join("app.db-wal"), b"leftover WAL").unwrap();
        fs::write(data.join("app.db-shm"), b"leftover shared memory").unwrap();
        assert!(apply_ready_restore(&fixture.database, &fixture.settings).unwrap());
        assert!(!data.join("app.db-wal").exists());
        assert!(!data.join("app.db-shm").exists());
        let restored = StateStore::daemon(fixture.database.clone());
        let mut state = restored.load().unwrap();
        assert_ne!(state.projects[0].name, "Current data");
        restored.hydrate(&mut state.sessions[0]).unwrap();
        assert_eq!(
            state.sessions[0].messages[0].content,
            "Keep this conversation"
        );
        assert_eq!(
            fs::read(data.join("blobs/image.png")).unwrap(),
            b"original image bytes"
        );
        assert!(!data.join("blobs/extra.png").exists());
        assert_eq!(
            fs::read(data.join("attachments/example/nested/file.txt")).unwrap(),
            b"original attachment"
        );
        let _: ComposerDrafts = read_json(&data.join("composer-drafts.json")).unwrap();
        let desktop: DesktopBackup = read_json(&data.join(RESTORED_DESKTOP)).unwrap();
        assert_eq!(desktop.settings["theme"], "dark");
        let _: DaemonSettings = read_json(&fixture.settings).unwrap();
        assert!(!data.join(READY_RESTORE).exists());
        assert!(!data.join(JOURNAL).exists());
        assert_eq!(fs::read_dir(&fixture.backups).unwrap().count(), 0);
        assert!(!apply_ready_restore(&fixture.database, &fixture.settings).unwrap());
    }

    #[test]
    fn rejects_corruption_without_changing_live_data_or_scheduling_restore() {
        let fixture = Fixture::new();
        let backup = fixture.create();
        let before = fs::read(&fixture.database).unwrap();
        fs::write(backup.path.join("blobs/image.png"), b"corrupted same size!").unwrap();
        assert!(stage_restore(&fixture.database, &backup.path).is_err());
        assert_eq!(fs::read(&fixture.database).unwrap(), before);
        assert!(!fixture.database.with_file_name(PENDING_RESTORE).exists());
        assert_eq!(list(&fixture.backups).unwrap().len(), 1);
    }

    #[test]
    fn rejects_newer_versions_and_paths_that_escape_the_snapshot() {
        let fixture = Fixture::new();
        let backup = fixture.create();
        let path = backup.path.join(MANIFEST);
        let mut manifest: Manifest = read_json(&path).unwrap();
        manifest.version += 1;
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(validate(&backup.path).is_err());
        manifest.version = FORMAT_VERSION;
        manifest.files[0].path = "../outside".into();
        fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(validate(&backup.path).is_err());
    }

    #[test]
    fn rejects_future_database_migrations() {
        let fixture = Fixture::new();
        let connection = Connection::open(&fixture.database).unwrap();
        connection
            .execute(
                "INSERT INTO migrations(tag, applied_at) VALUES('9999_future', 1)",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(
            create(
                &fixture.database,
                &DaemonSettings::default(),
                &fixture.backups,
                &fixture.desktop
            )
            .is_err()
        );
        assert!(list(&fixture.backups).unwrap().is_empty());
        assert_eq!(fs::read_dir(&fixture.backups).unwrap().count(), 0);
    }

    #[test]
    fn corrupt_staged_restore_keeps_the_app_open_on_original_data() {
        let fixture = Fixture::new();
        let backup = fixture.create();
        stage_restore(&fixture.database, &backup.path).unwrap();
        fixture.arm();
        fs::write(
            fixture
                .database
                .with_file_name(READY_RESTORE)
                .join("app.db"),
            b"damaged snapshot",
        )
        .unwrap();
        let original = fs::read(&fixture.database).unwrap();
        restore_on_startup(&fixture.database, &fixture.settings).unwrap();
        assert_eq!(fs::read(&fixture.database).unwrap(), original);
        assert!(
            fixture
                .database
                .with_file_name(waku_protocol::backup::RESTORE_ERROR)
                .exists()
        );
        assert!(!fixture.database.with_file_name(READY_RESTORE).exists());
        StateStore::daemon(fixture.database.clone()).load().unwrap();
    }

    #[test]
    fn pending_restore_can_be_cancelled_without_touching_current_data() {
        let fixture = Fixture::new();
        let backup = fixture.create();
        stage_restore(&fixture.database, &backup.path).unwrap();
        let before = fs::read(&fixture.database).unwrap();
        cancel_restore(&fixture.database).unwrap();
        assert_eq!(fs::read(&fixture.database).unwrap(), before);
        assert!(!fixture.database.with_file_name(PENDING_RESTORE).exists());
        assert!(!apply_ready_restore(&fixture.database, &fixture.settings).unwrap());
    }

    #[test]
    fn recovers_an_interrupted_restore_from_its_journal() {
        let fixture = Fixture::new();
        let data = fixture.database.parent().unwrap();
        let target = data.join("blobs");
        let journal = RestoreJournal {
            id: Uuid::new_v4(),
            committed: false,
            entries: vec![
                RestoreEntry {
                    name: "blobs".into(),
                    existed: true,
                },
                RestoreEntry {
                    name: RESTORED_DESKTOP.into(),
                    existed: false,
                },
            ],
        };
        fs::rename(&target, sibling(&target, journal.id, "old")).unwrap();
        private_directory(&target).unwrap();
        fs::write(target.join("partial.png"), b"partial installation").unwrap();
        fs::write(data.join(RESTORED_DESKTOP), b"partial settings").unwrap();
        write_json(&data.join(JOURNAL), &journal).unwrap();
        assert!(!apply_ready_restore(&fixture.database, &fixture.settings).unwrap());
        assert_eq!(
            fs::read(target.join("image.png")).unwrap(),
            b"original image bytes"
        );
        assert!(!target.join("partial.png").exists());
        assert!(!data.join(RESTORED_DESKTOP).exists());
        assert!(!data.join(JOURNAL).exists());
    }

    #[test]
    fn committed_journal_cleanup_keeps_the_restored_data() {
        let fixture = Fixture::new();
        let data = fixture.database.parent().unwrap();
        let target = data.join("blobs");
        let journal = RestoreJournal {
            id: Uuid::new_v4(),
            committed: true,
            entries: vec![RestoreEntry {
                name: "blobs".into(),
                existed: true,
            }],
        };
        fs::rename(&target, sibling(&target, journal.id, "old")).unwrap();
        private_directory(&target).unwrap();
        fs::write(target.join("restored.png"), b"committed").unwrap();
        write_json(&data.join(JOURNAL), &journal).unwrap();
        assert!(!apply_ready_restore(&fixture.database, &fixture.settings).unwrap());
        assert_eq!(fs::read(target.join("restored.png")).unwrap(), b"committed");
        assert!(!sibling(&target, journal.id, "old").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_and_backup_destinations_inside_live_data() {
        let fixture = Fixture::new();
        assert!(
            create(
                &fixture.database,
                &DaemonSettings::default(),
                &fixture.database.with_file_name("backups"),
                &fixture.desktop
            )
            .is_err()
        );
        std::os::unix::fs::symlink(
            &fixture.settings,
            fixture.database.with_file_name("blobs/link.json"),
        )
        .unwrap();
        assert!(
            create(
                &fixture.database,
                &DaemonSettings::default(),
                &fixture.backups,
                &fixture.desktop
            )
            .is_err()
        );
        assert!(list(&fixture.backups).unwrap().is_empty());
    }
}
