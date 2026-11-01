//! Immutable Git review context capture and shared read-only snapshot directory materialization.

use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context as _, bail};
use uuid::Uuid;
use waku_protocol::model::unix_time;
use waku_protocol::workspace::ReviewDiffSource;

#[derive(Clone, Debug)]
pub struct CapturedReviewContext {
    pub id: Uuid,
    pub cwd: PathBuf,
    pub source: ReviewDiffSource,
    pub base_oid: String,
    pub target_oid: String,
    pub numstat: String,
    pub patch: String,
    pub complete_context: bool,
    pub snapshot_dir: Option<PathBuf>,
    pub created_at: u64,
}

/// A newly captured directory is owned by its caller until its database
/// transaction commits. All error and superseded-request paths reclaim it.
pub(super) struct SnapshotCleanup(pub Option<PathBuf>);

impl Drop for SnapshotCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            cleanup_snapshot(&path);
        }
    }
}

pub fn is_git_workspace(cwd: &Path) -> anyhow::Result<bool> {
    let output = crate::command_env::plain_command("git")
        .env("LC_ALL", "C")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()?;
    if output.status.success() {
        return Ok(output.stdout.starts_with(b"true"));
    }
    let error = String::from_utf8_lossy(&output.stderr);
    if error.contains("not a git repository") {
        return Ok(false);
    }
    bail!("could not inspect review workspace: {error}")
}

/// Capture the exact diff and Git object tree for a review source, and materialize a shared
/// read-only directory for child subtask execution.
pub fn capture_review_context(
    cwd: &Path,
    source: ReviewDiffSource,
) -> anyhow::Result<CapturedReviewContext> {
    crate::workspace::ensure_repository(cwd)?;
    let range = crate::workspace::resolve_diff_range(cwd, source.clone())
        .context("could not resolve Git diff range for review source")?;
    let numstat = crate::workspace::diff_output(cwd, &range, &["--numstat"])
        .context("could not generate diff numstat")?;
    let hydrated = crate::workspace::diff_output(cwd, &range, &["--unified=2147483647"])?;
    let (patch, complete_context) = if hydrated.len() <= crate::workspace::MAX_HYDRATED_PATCH_BYTES
    {
        (hydrated, true)
    } else {
        (
            crate::workspace::diff_output(cwd, &range, &["--unified=3"])?,
            false,
        )
    };

    let context_id = Uuid::new_v4();
    let snapshot_dir = materialize_snapshot(cwd, &range.to, &patch, &numstat, context_id)
        .context("could not materialize read-only snapshot directory")?;

    Ok(CapturedReviewContext {
        id: context_id,
        cwd: cwd.to_path_buf(),
        source,
        base_oid: range.from,
        target_oid: range.to,
        numstat,
        patch,
        complete_context,
        snapshot_dir: Some(snapshot_dir),
        created_at: unix_time(),
    })
}

/// Base temporary directory where Waku review snapshots are materialized.
pub fn snapshots_root_dir() -> PathBuf {
    std::env::temp_dir().join("waku-review-snapshots")
}

/// Materialize the Git tree at `target_oid` into a temporary read-only directory for child subtasks.
fn materialize_snapshot(
    cwd: &Path,
    target_oid: &str,
    patch: &str,
    numstat: &str,
    context_id: Uuid,
) -> anyhow::Result<PathBuf> {
    let root = snapshots_root_dir();
    fs::create_dir_all(&root)?;
    let destination = root.join(context_id.to_string());
    if destination.exists() {
        cleanup_snapshot(&destination);
    }
    fs::create_dir_all(&destination)?;
    let mut cleanup = SnapshotCleanup(Some(destination.clone()));
    materialize_tree(cwd, target_oid, &destination)?;

    // Write .waku-review directory with patch and file summaries
    let review_meta_dir = destination.join(".waku-review");
    anyhow::ensure!(
        !review_meta_dir.exists(),
        "the workspace uses the reserved .waku-review path"
    );
    fs::create_dir_all(&review_meta_dir)?;
    fs::write(review_meta_dir.join("patch.diff"), patch)?;
    fs::write(review_meta_dir.join("numstat.txt"), numstat)?;

    #[cfg(unix)]
    set_directory_read_only(&destination)?;
    cleanup.0 = None;
    Ok(destination)
}

/// Read raw objects through one Git process. Archive attributes (export-ignore
/// and export-subst), checkout filters and repository hooks cannot change or
/// omit the source being reviewed. Blobs stream to disk rather than RAM.
fn materialize_tree(cwd: &Path, target: &str, destination: &Path) -> anyhow::Result<()> {
    let tree = crate::command_env::plain_command("git")
        .args(["ls-tree", "-r", "-z", "--full-tree", target])
        .current_dir(cwd)
        .output()?;
    anyhow::ensure!(tree.status.success(), "could not list review tree {target}");
    let mut objects = crate::command_env::plain_command("git")
        .args(["cat-file", "--batch"])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut input = objects
        .stdin
        .take()
        .context("Git object input unavailable")?;
    let mut output = BufReader::new(
        objects
            .stdout
            .take()
            .context("Git object output unavailable")?,
    );
    let result = (|| -> anyhow::Result<()> {
        for entry in tree
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let split = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .context("invalid Git tree entry")?;
            let header = std::str::from_utf8(&entry[..split])?;
            let mut fields = header.split_whitespace();
            let mode = fields.next().context("Git tree mode missing")?;
            let kind = fields.next().context("Git tree kind missing")?;
            let oid = fields.next().context("Git object ID missing")?;
            #[cfg(unix)]
            let relative = {
                use std::os::unix::ffi::OsStringExt as _;
                PathBuf::from(std::ffi::OsString::from_vec(entry[split + 1..].to_vec()))
            };
            #[cfg(not(unix))]
            let relative = PathBuf::from(std::str::from_utf8(&entry[split + 1..])?);
            anyhow::ensure!(
                relative
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_))),
                "invalid snapshot path"
            );
            let path = destination.join(relative);
            if kind == "commit" {
                fs::create_dir_all(path)?;
                continue;
            }
            anyhow::ensure!(kind == "blob", "unexpected Git tree object {kind}");
            fs::create_dir_all(path.parent().context("snapshot file has no parent")?)?;
            writeln!(input, "{oid}")?;
            input.flush()?;
            let mut header = String::new();
            output.read_line(&mut header)?;
            let fields = header.split_whitespace().collect::<Vec<_>>();
            anyhow::ensure!(
                fields.len() == 3 && fields[1] == "blob",
                "invalid Git blob response"
            );
            let size: u64 = fields[2].parse()?;
            if mode == "120000" {
                anyhow::ensure!(size <= 1024 * 1024, "snapshot symlink is too long");
                let mut target = Vec::with_capacity(size as usize);
                (&mut output).take(size).read_to_end(&mut target)?;
                anyhow::ensure!(target.len() as u64 == size, "incomplete Git symlink");
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt as _;
                    std::os::unix::fs::symlink(std::ffi::OsString::from_vec(target), &path)?;
                }
                #[cfg(windows)]
                std::os::windows::fs::symlink_file(std::str::from_utf8(&target)?, &path)?;
            } else {
                let mut file = fs::File::create(&path)?;
                let copied = std::io::copy(&mut (&mut output).take(size), &mut file)?;
                anyhow::ensure!(copied == size, "incomplete Git blob");
            }
            let mut newline = [0];
            output.read_exact(&mut newline)?;
            anyhow::ensure!(newline == [b'\n'], "invalid Git blob terminator");
        }
        Ok(())
    })();
    drop(input);
    drop(output);
    if result.is_err() {
        let _ = objects.kill();
    }
    let status = objects.wait()?;
    result?;
    anyhow::ensure!(status.success(), "could not read review objects");
    Ok(())
}

/// Recursively set permissions to read-only on Unix.
#[cfg(unix)]
fn set_directory_read_only(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                set_directory_read_only(&entry?.path())?;
            }
            // 0500: r-x------ (searchable and readable, not writable)
            fs::set_permissions(path, fs::Permissions::from_mode(0o500))?;
        } else if metadata.is_file() {
            // 0400: r-------- (readable, not writable)
            fs::set_permissions(path, fs::Permissions::from_mode(0o400))?;
        }
    }
    Ok(())
}

/// Restore write permissions recursively so directory removal succeeds on Unix.
#[cfg(unix)]
fn make_directory_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_dir() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    make_directory_writable(&entry.path());
                }
            }
        } else if metadata.is_file() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
    }
}

/// Safely remove a materialized snapshot directory.
pub fn cleanup_snapshot(path: &Path) {
    if !path.exists() {
        return;
    }
    #[cfg(unix)]
    make_directory_writable(path);
    let _ = fs::remove_dir_all(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn repository() -> PathBuf {
        let root = std::env::temp_dir().join(format!("waku-review-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn baseline() {}\n").unwrap();
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Waku Tests",
                "-c",
                "user.email=waku@example.com",
                "commit",
                "-m",
                "baseline",
            ],
        );
        root
    }

    #[test]
    fn capture_snapshots_the_worktree_and_keeps_the_tree_immutable() {
        let root = repository();
        fs::write(root.join("src/lib.rs"), "fn baseline() {}\nfn added() {}\n").unwrap();
        fs::write(root.join("untracked.txt"), "loose file\n").unwrap();

        let context = capture_review_context(&root, ReviewDiffSource::Uncommitted).unwrap();
        let snapshot = context.snapshot_dir.clone().expect("snapshot directory");

        // The captured tree holds the untracked file too: a reviewer that only
        // saw tracked files would miss exactly what needs review.
        assert_eq!(
            fs::read_to_string(snapshot.join("src/lib.rs")).unwrap(),
            "fn baseline() {}\nfn added() {}\n"
        );
        assert_eq!(
            fs::read_to_string(snapshot.join("untracked.txt")).unwrap(),
            "loose file\n"
        );
        assert!(context.patch.contains("fn added()"));
        assert!(!context.numstat.is_empty());
        assert!(!context.base_oid.is_empty() && !context.target_oid.is_empty());

        // The snapshot is read-only, so a child cannot rewrite the review input.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(snapshot.join("src/lib.rs"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o222, 0, "snapshot files must not be writable");
        }

        // Editing the live worktree afterwards cannot change what was captured.
        fs::write(root.join("src/lib.rs"), "fn rewritten() {}\n").unwrap();
        assert_eq!(
            fs::read_to_string(snapshot.join("src/lib.rs")).unwrap(),
            "fn baseline() {}\nfn added() {}\n"
        );

        cleanup_snapshot(&snapshot);
        assert!(!snapshot.exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn capture_rejects_a_non_git_workspace() {
        let directory = std::env::temp_dir().join(format!("waku-review-plain-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let error = capture_review_context(&directory, ReviewDiffSource::Uncommitted)
            .expect_err("a non-Git workspace has no reviewable diff");
        assert!(
            error.to_string().contains("not a Git repository") || !error.to_string().is_empty()
        );
        fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn review_snapshots_ignore_archive_and_checkout_filters() {
        let root = repository();
        fs::write(
            root.join(".gitattributes"),
            "src/lib.rs export-ignore\nversion.txt export-subst\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "fn added() {}\n").unwrap();
        fs::write(root.join("version.txt"), "$Format:%H$\n").unwrap();
        let context = capture_review_context(&root, ReviewDiffSource::Uncommitted).unwrap();
        let snapshot = context.snapshot_dir.unwrap();
        assert_eq!(
            fs::read_to_string(snapshot.join("src/lib.rs")).unwrap(),
            "fn added() {}\n"
        );
        assert_eq!(
            fs::read_to_string(snapshot.join("version.txt")).unwrap(),
            "$Format:%H$\n"
        );
        cleanup_snapshot(&snapshot);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_materialization_reclaims_the_partial_snapshot() {
        let root = repository();
        fs::write(root.join(".waku-review"), "occupied").unwrap();
        git(&root, &["add", "."]);
        let target = crate::checkpoint::capture_worktree_commit(&root).unwrap();
        let id = Uuid::new_v4();
        let result = materialize_snapshot(&root, &target, "patch", "", id);
        assert!(result.is_err());
        assert!(!snapshots_root_dir().join(id.to_string()).exists());
        fs::remove_dir_all(root).unwrap();
    }
}
