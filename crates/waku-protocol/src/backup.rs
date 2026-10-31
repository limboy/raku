//! Portable backup metadata. Filesystem work belongs to the daemon.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const AUTOMATIC_BACKUP_INTERVAL: u64 = 24 * 60 * 60;
pub const AUTOMATIC_BACKUP_LIMIT: usize = 1;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BackupKind {
    #[default]
    Manual,
    Automatic,
    Safety,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupInfo {
    #[serde(default)]
    pub kind: BackupKind,
    pub path: PathBuf,
    pub created_at: u64,
    pub size_bytes: u64,
    pub project_count: usize,
    pub session_count: usize,
}

pub fn next_automatic_backup_at(backups: &[BackupInfo]) -> u64 {
    backups
        .iter()
        .map(|backup| backup.created_at.saturating_add(AUTOMATIC_BACKUP_INTERVAL))
        .max()
        .unwrap_or(0)
}

/// Opaque to the daemon: each desktop version owns its preference schema.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopBackup {
    pub settings: Value,
    pub state: Value,
}

// Shared names coordinate the next-launch handoff without making the desktop
// depend on SQLite or the daemon's backup implementation.
pub const PENDING_RESTORE: &str = ".pending-restore";
pub const READY_RESTORE: &str = ".ready-restore";
pub const RESTORED_DESKTOP: &str = ".restored-desktop.json";
pub const RESTORE_ERROR: &str = ".restore-error.json";
