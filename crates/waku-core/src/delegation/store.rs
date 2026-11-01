//! SQLite persistence layer for delegation groups, tasks, events, and review contexts.

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;
use waku_protocol::delegation::{
    DelegatedTaskEvent, DelegatedTaskEvents, DelegatedTaskStatus, DelegatedTaskSummary,
    DelegationGroupStatus, ModelReference,
};
use waku_protocol::model::unix_time;
use waku_protocol::workspace::ReviewDiffSource;

use super::context::CapturedReviewContext;

#[derive(Clone, Debug)]
pub struct DelegationGroupRow {
    pub id: Uuid,
    pub parent_session_id: Uuid,
    pub parent_turn_id: Option<String>,
    pub parent_runtime_id: Option<String>,
    pub request_key: String,
    pub request_hash: String,
    pub context_id: Uuid,
    pub status: DelegationGroupStatus,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug)]
pub struct DelegatedTaskRow {
    pub id: Uuid,
    pub group_id: Uuid,
    pub task_key: String,
    pub child_session_id: Uuid,
    pub runtime_id: Option<Uuid>,
    pub target_config: ModelReference,
    pub instructions: String,
    pub status: DelegatedTaskStatus,
    pub final_conclusion: Option<String>,
    pub error_message: Option<String>,
    pub latest_activity: Option<String>,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
}

impl DelegatedTaskRow {
    pub fn to_summary(&self) -> DelegatedTaskSummary {
        let duration_ms = match (self.started_at, self.finished_at) {
            (Some(start), Some(finish)) => Some(finish.saturating_sub(start).saturating_mul(1000)),
            _ => None,
        };
        DelegatedTaskSummary {
            task_id: self.id,
            attempt_id: self.runtime_id,
            task_key: self.task_key.clone(),
            target: self.target_config.clone(),
            status: self.status,
            duration_ms,
            final_conclusion: self.final_conclusion.clone(),
            error_message: self.error_message.clone(),
            latest_activity: self
                .latest_activity
                .as_ref()
                .filter(|_| !self.status.is_terminal())
                .cloned(),
        }
    }
}

pub fn insert_review_context(
    connection: &Connection,
    context: &CapturedReviewContext,
) -> anyhow::Result<()> {
    let source_json = serde_json::to_string(&context.source)?;
    connection.execute(
        "INSERT INTO review_contexts (
            id, cwd, source, base_oid, target_oid, numstat, patch, complete_context, snapshot_dir, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            context.id.to_string(),
            context.cwd.to_string_lossy(),
            source_json,
            context.base_oid,
            context.target_oid,
            context.numstat,
            context.patch,
            context.complete_context,
            context.snapshot_dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
            context.created_at as i64,
        ],
    )?;
    Ok(())
}

pub fn get_review_context(
    connection: &Connection,
    context_id: Uuid,
) -> anyhow::Result<Option<CapturedReviewContext>> {
    let mut statement = connection.prepare(
        "SELECT id, cwd, source, base_oid, target_oid, numstat, patch, complete_context, snapshot_dir, created_at
         FROM review_contexts WHERE id = ?1",
    )?;
    let row = statement
        .query_row(params![context_id.to_string()], |row| {
            let id_str: String = row.get(0)?;
            let cwd_str: String = row.get(1)?;
            let source_str: String = row.get(2)?;
            let base_oid: String = row.get(3)?;
            let target_oid: String = row.get(4)?;
            let numstat: String = row.get(5)?;
            let patch: String = row.get(6)?;
            let complete_context: bool = row.get(7)?;
            let snapshot_dir_str: Option<String> = row.get(8)?;
            let created_at: i64 = row.get(9)?;
            Ok((
                id_str,
                cwd_str,
                source_str,
                base_oid,
                target_oid,
                numstat,
                patch,
                complete_context,
                snapshot_dir_str,
                created_at,
            ))
        })
        .optional()?;

    let Some((
        id_str,
        cwd_str,
        source_str,
        base_oid,
        target_oid,
        numstat,
        patch,
        complete_context,
        snapshot_dir_str,
        created_at,
    )) = row
    else {
        return Ok(None);
    };

    let id = Uuid::parse_str(&id_str)?;
    let source: ReviewDiffSource = serde_json::from_str(&source_str)?;
    Ok(Some(CapturedReviewContext {
        id,
        cwd: PathBuf::from(cwd_str),
        source,
        base_oid,
        target_oid,
        numstat,
        patch,
        complete_context,
        snapshot_dir: snapshot_dir_str.map(PathBuf::from),
        created_at: created_at as u64,
    }))
}

pub fn insert_delegation_group(
    connection: &Connection,
    group: &DelegationGroupRow,
) -> anyhow::Result<()> {
    let status_str = serde_json::to_string(&group.status)?;
    let status_trimmed = status_str.trim_matches('"');
    connection.execute(
        "INSERT INTO delegation_groups (
            id, parent_session_id, parent_turn_id, parent_runtime_id,
            request_key, request_hash, context_id, status, created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            group.id.to_string(),
            group.parent_session_id.to_string(),
            group.parent_turn_id,
            group.parent_runtime_id,
            group.request_key,
            group.request_hash,
            group.context_id.to_string(),
            status_trimmed,
            group.created_at as i64,
            group.updated_at as i64,
        ],
    )?;
    Ok(())
}

pub fn get_delegation_group(
    connection: &Connection,
    group_id: Uuid,
) -> anyhow::Result<Option<DelegationGroupRow>> {
    let mut statement = connection.prepare(
        "SELECT id, parent_session_id, parent_turn_id, parent_runtime_id,
                request_key, request_hash, context_id, status, created_at, updated_at
         FROM delegation_groups WHERE id = ?1",
    )?;
    let row = statement
        .query_row(params![group_id.to_string()], decode_delegation_group_row)
        .optional()?;
    Ok(row)
}

pub fn get_delegation_group_by_request_key(
    connection: &Connection,
    parent_session_id: Uuid,
    request_key: &str,
) -> anyhow::Result<Option<DelegationGroupRow>> {
    let mut statement = connection.prepare(
        "SELECT id, parent_session_id, parent_turn_id, parent_runtime_id,
                request_key, request_hash, context_id, status, created_at, updated_at
         FROM delegation_groups WHERE parent_session_id = ?1 AND request_key = ?2",
    )?;
    let row = statement
        .query_row(
            params![parent_session_id.to_string(), request_key],
            decode_delegation_group_row,
        )
        .optional()?;
    Ok(row)
}

fn decode_delegation_group_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DelegationGroupRow> {
    let id: String = row.get(0)?;
    let parent_session_id: String = row.get(1)?;
    let parent_turn_id: Option<String> = row.get(2)?;
    let parent_runtime_id: Option<String> = row.get(3)?;
    let request_key: String = row.get(4)?;
    let request_hash: String = row.get(5)?;
    let context_id: String = row.get(6)?;
    let status_str: String = row.get(7)?;
    let created_at: i64 = row.get(8)?;
    let updated_at: i64 = row.get(9)?;

    let status = match status_str.as_str() {
        "preparing" => DelegationGroupStatus::Preparing,
        "running" => DelegationGroupStatus::Running,
        "completed" => DelegationGroupStatus::Completed,
        "failed" => DelegationGroupStatus::Failed,
        "cancelled" => DelegationGroupStatus::Cancelled,
        _ => DelegationGroupStatus::Running,
    };

    Ok(DelegationGroupRow {
        id: Uuid::parse_str(&id).unwrap_or_default(),
        parent_session_id: Uuid::parse_str(&parent_session_id).unwrap_or_default(),
        parent_turn_id,
        parent_runtime_id,
        request_key,
        request_hash,
        context_id: Uuid::parse_str(&context_id).unwrap_or_default(),
        status,
        created_at: created_at as u64,
        updated_at: updated_at as u64,
    })
}

pub fn update_delegation_group_status(
    connection: &Connection,
    group_id: Uuid,
    status: DelegationGroupStatus,
) -> anyhow::Result<()> {
    let status_str = serde_json::to_string(&status)?;
    let status_trimmed = status_str.trim_matches('"');
    let now = unix_time();
    connection.execute(
        "UPDATE delegation_groups SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![status_trimmed, now as i64, group_id.to_string()],
    )?;
    Ok(())
}

pub fn list_delegation_groups_by_session(
    connection: &Connection,
    parent_session_id: Uuid,
) -> anyhow::Result<Vec<DelegationGroupRow>> {
    let mut statement = connection.prepare(
        "SELECT id, parent_session_id, parent_turn_id, parent_runtime_id,
                request_key, request_hash, context_id, status, created_at, updated_at
         FROM delegation_groups WHERE parent_session_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = statement
        .query_map(
            params![parent_session_id.to_string()],
            decode_delegation_group_row,
        )?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

pub fn insert_delegated_task(
    connection: &Connection,
    task: &DelegatedTaskRow,
) -> anyhow::Result<()> {
    let target_json = serde_json::to_string(&task.target_config)?;
    let status_str = serde_json::to_string(&task.status)?;
    let status_trimmed = status_str.trim_matches('"');
    connection.execute(
        "INSERT INTO delegated_tasks (
            id, group_id, task_key, child_session_id, runtime_id,
            target_config, instructions, status, final_conclusion, error_message,
            latest_activity, started_at, finished_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            task.id.to_string(),
            task.group_id.to_string(),
            task.task_key,
            task.child_session_id.to_string(),
            task.runtime_id.map(|id| id.to_string()),
            target_json,
            task.instructions,
            status_trimmed,
            task.final_conclusion,
            task.error_message,
            task.latest_activity,
            task.started_at.map(|t| t as i64),
            task.finished_at.map(|t| t as i64),
        ],
    )?;
    Ok(())
}

fn decode_delegated_task_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DelegatedTaskRow> {
    let id: String = row.get(0)?;
    let group_id: String = row.get(1)?;
    let task_key: String = row.get(2)?;
    let child_session_id: String = row.get(3)?;
    let runtime_id: Option<String> = row.get(4)?;
    let target_json: String = row.get(5)?;
    let instructions: String = row.get(6)?;
    let status_str: String = row.get(7)?;
    let final_conclusion: Option<String> = row.get(8)?;
    let error_message: Option<String> = row.get(9)?;
    let latest_activity: Option<String> = row.get(10)?;
    let started_at: Option<i64> = row.get(11)?;
    let finished_at: Option<i64> = row.get(12)?;

    let target_config: ModelReference = serde_json::from_str(&target_json).unwrap_or_else(|_| {
        ModelReference::new(
            "unknown",
            waku_protocol::model::ProviderKind::Codex,
            "unknown",
            "Unknown",
        )
    });

    let status = match status_str.as_str() {
        "queued" => DelegatedTaskStatus::Queued,
        "starting" => DelegatedTaskStatus::Starting,
        "running" => DelegatedTaskStatus::Running,
        "completed" => DelegatedTaskStatus::Completed,
        "failed" => DelegatedTaskStatus::Failed,
        "cancelled" => DelegatedTaskStatus::Cancelled,
        "timedOut" => DelegatedTaskStatus::TimedOut,
        "interrupted" => DelegatedTaskStatus::Interrupted,
        _ => DelegatedTaskStatus::Running,
    };

    Ok(DelegatedTaskRow {
        id: Uuid::parse_str(&id).unwrap_or_default(),
        group_id: Uuid::parse_str(&group_id).unwrap_or_default(),
        task_key,
        child_session_id: Uuid::parse_str(&child_session_id).unwrap_or_default(),
        runtime_id: runtime_id.and_then(|id| Uuid::parse_str(&id).ok()),
        target_config,
        instructions,
        status,
        final_conclusion,
        error_message,
        latest_activity,
        started_at: started_at.map(|t| t as u64),
        finished_at: finished_at.map(|t| t as u64),
    })
}

pub fn list_delegated_tasks_by_group(
    connection: &Connection,
    group_id: Uuid,
) -> anyhow::Result<Vec<DelegatedTaskRow>> {
    let mut statement = connection.prepare(
        "SELECT id, group_id, task_key, child_session_id, runtime_id,
                target_config, instructions, status, final_conclusion, error_message,
                latest_activity, started_at, finished_at
         FROM delegated_tasks WHERE group_id = ?1",
    )?;
    let rows = statement
        .query_map(params![group_id.to_string()], decode_delegated_task_row)?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

pub fn get_delegated_task(
    connection: &Connection,
    task_id: Uuid,
) -> anyhow::Result<Option<DelegatedTaskRow>> {
    let mut statement = connection.prepare(
        "SELECT id, group_id, task_key, child_session_id, runtime_id,
                target_config, instructions, status, final_conclusion, error_message,
                latest_activity, started_at, finished_at
         FROM delegated_tasks WHERE id = ?1",
    )?;
    let row = statement
        .query_row(params![task_id.to_string()], decode_delegated_task_row)
        .optional()?;
    Ok(row)
}

pub fn update_delegated_task_progress(
    connection: &Connection,
    task_id: Uuid,
    runtime_id: Option<Uuid>,
    status: DelegatedTaskStatus,
    latest_activity: Option<&str>,
) -> anyhow::Result<bool> {
    let status_str = serde_json::to_string(&status)?;
    let status_trimmed = status_str.trim_matches('"');
    let changed = connection.execute(
        "UPDATE delegated_tasks
         SET status = ?1,
             latest_activity = COALESCE(?2, latest_activity),
             started_at = CASE WHEN ?1 = 'running' THEN COALESCE(started_at, ?4) ELSE started_at END
         WHERE id = ?3 AND runtime_id IS ?5 AND status IN ('queued', 'starting', 'running')",
        params![
            status_trimmed,
            latest_activity,
            task_id.to_string(),
            unix_time() as i64,
            runtime_id.map(|id| id.to_string()),
        ],
    )?;
    Ok(changed != 0)
}

/// Claims a queued generation once. A runner queued before cancellation or
/// retry cannot claim a later generation of the same logical task.
pub fn begin_delegated_attempt(
    connection: &Connection,
    task_id: Uuid,
    expected_runtime: Option<Uuid>,
    runtime_id: Uuid,
) -> anyhow::Result<bool> {
    let tx =
        rusqlite::Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE delegated_tasks SET status = 'starting', runtime_id = ?1
         WHERE id = ?2 AND runtime_id IS ?3 AND status = 'queued'",
        params![
            runtime_id.to_string(),
            task_id.to_string(),
            expected_runtime.map(|id| id.to_string())
        ],
    )?;
    if changed != 0 {
        let event = waku_protocol::WireDriverEvent::new(
            "delegationAttemptStarted",
            serde_json::json!({"attemptId": runtime_id}),
        );
        append_delegation_event_for_runtime(
            &tx,
            task_id,
            runtime_id,
            0,
            &serde_json::to_string(&event)?,
        )?;
    }
    tx.commit()?;
    Ok(changed != 0)
}

pub fn projected_group_status(
    stored: DelegationGroupStatus,
    tasks: &[DelegatedTaskRow],
) -> DelegationGroupStatus {
    if tasks.is_empty() {
        return stored;
    }
    if tasks.iter().any(|task| !task.status.is_terminal()) {
        return DelegationGroupStatus::Running;
    }
    if stored == DelegationGroupStatus::Cancelled {
        return stored;
    }
    if tasks.iter().any(|task| {
        matches!(
            task.status,
            DelegatedTaskStatus::Failed
                | DelegatedTaskStatus::TimedOut
                | DelegatedTaskStatus::Interrupted
        )
    }) {
        DelegationGroupStatus::Failed
    } else if tasks
        .iter()
        .any(|task| task.status == DelegatedTaskStatus::Cancelled)
    {
        DelegationGroupStatus::Cancelled
    } else {
        DelegationGroupStatus::Completed
    }
}

fn refresh_group_status(connection: &Connection, group_id: Uuid) -> anyhow::Result<()> {
    let tasks = list_delegated_tasks_by_group(connection, group_id)?;
    update_delegation_group_status(
        connection,
        group_id,
        projected_group_status(DelegationGroupStatus::Running, &tasks),
    )
}

pub fn reset_delegated_task(
    connection: &Connection,
    group_id: Uuid,
    task_id: Uuid,
    runtime_id: Uuid,
) -> anyhow::Result<()> {
    let tx =
        rusqlite::Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE delegated_tasks SET status = 'queued', runtime_id = ?1,
         final_conclusion = NULL, error_message = NULL, latest_activity = NULL,
         started_at = NULL, finished_at = NULL
         WHERE id = ?2 AND group_id = ?3 AND status IN ('completed','failed','cancelled','timedOut','interrupted')",
        params![runtime_id.to_string(), task_id.to_string(), group_id.to_string()],
    )?;
    anyhow::ensure!(
        changed != 0,
        "only a finished task in this group can be retried"
    );
    refresh_group_status(&tx, group_id)?;
    tx.commit()?;
    Ok(())
}

pub fn finish_delegated_task(
    connection: &Connection,
    task_id: Uuid,
    runtime_id: Option<Uuid>,
    status: DelegatedTaskStatus,
    final_conclusion: Option<&str>,
    error_message: Option<&str>,
) -> anyhow::Result<bool> {
    anyhow::ensure!(
        status.is_terminal(),
        "finish requires a terminal task status"
    );
    let tx =
        rusqlite::Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate)?;
    let status_str = serde_json::to_string(&status)?;
    let status_trimmed = status_str.trim_matches('"');
    let now = unix_time();
    let changed = tx.execute(
        "UPDATE delegated_tasks
         SET status = ?1, final_conclusion = ?2, error_message = ?3, finished_at = ?4,
             latest_activity = NULL
         WHERE id = ?5 AND runtime_id IS ?6 AND status IN ('queued', 'starting', 'running')",
        params![
            status_trimmed,
            final_conclusion,
            error_message,
            now as i64,
            task_id.to_string(),
            runtime_id.map(|id| id.to_string()),
        ],
    )?;
    if changed != 0
        && let Some(task) = get_delegated_task(&tx, task_id)?
    {
        refresh_group_status(&tx, task.group_id)?;
    }
    tx.commit()?;
    Ok(changed != 0)
}

/// A daemon restart cannot resume provider work in flight. Every row that is
/// still non-terminal becomes `Interrupted` so no client waits forever on a
/// subtask nothing will ever finish.
pub fn interrupt_unfinished_delegations(connection: &Connection) -> anyhow::Result<()> {
    let tx =
        rusqlite::Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE delegated_tasks
         SET status = 'interrupted', error_message = COALESCE(error_message, 'Waku daemon restarted'),
             latest_activity = NULL, finished_at = ?1
         WHERE status IN ('queued', 'starting', 'running')",
        params![unix_time() as i64],
    )?;
    tx.execute(
        "UPDATE delegation_groups SET status = 'failed'
         WHERE status IN ('preparing', 'running')",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn append_delegation_event_for_runtime(
    connection: &Connection,
    task_id: Uuid,
    runtime_id: Uuid,
    sequence: u64,
    event_json: &str,
) -> anyhow::Result<bool> {
    let changed = connection.execute(
        "INSERT INTO delegation_events (task_id, sequence, event, created_at)
         SELECT id, ?1, ?2, ?3 FROM delegated_tasks
         WHERE id = ?4 AND runtime_id = ?5 AND status IN ('starting', 'running')",
        params![
            sequence as i64,
            event_json,
            unix_time() as i64,
            task_id.to_string(),
            runtime_id.to_string()
        ],
    )?;
    Ok(changed != 0)
}

pub fn append_delegation_event(
    connection: &Connection,
    task_id: Uuid,
    sequence: u64,
    event_json: &str,
) -> anyhow::Result<()> {
    let now = unix_time();
    connection.execute(
        "INSERT INTO delegation_events (task_id, sequence, event, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![task_id.to_string(), sequence as i64, event_json, now as i64],
    )?;
    Ok(())
}

const DELEGATION_EVENT_PAGE_ROWS: usize = 1024;
const DELEGATION_EVENT_PAGE_BYTES: usize = 256 * 1024;

/// Read only the requested task's events, without replaying its whole history.
/// Row IDs are durable cursors: provider sequence numbers restart on retry.
pub fn read_delegated_task_events(
    connection: &Connection,
    session_id: Uuid,
    group_id: Uuid,
    task_id: Uuid,
    after_sequence: u64,
) -> anyhow::Result<DelegatedTaskEvents> {
    let belongs: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM delegated_tasks t
            JOIN delegation_groups g ON g.id = t.group_id
            WHERE t.id = ?1 AND t.group_id = ?2 AND g.parent_session_id = ?3
         )",
        params![
            task_id.to_string(),
            group_id.to_string(),
            session_id.to_string()
        ],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        belongs,
        "delegated task is not in the requested session and group"
    );

    let mut statement = connection.prepare(
        "SELECT id, event, created_at FROM delegation_events
         WHERE task_id = ?1 AND id > ?2 ORDER BY id LIMIT ?3",
    )?;
    let mut rows = statement.query(params![
        task_id.to_string(),
        after_sequence.min(i64::MAX as u64) as i64,
        (DELEGATION_EVENT_PAGE_ROWS + 1) as i64,
    ])?;
    let mut events = Vec::new();
    let mut bytes = 0;
    let mut next_sequence = after_sequence;
    let mut has_more = false;
    let mut scanned = 0;
    while let Some(row) = rows.next()? {
        let sequence = row.get::<_, i64>(0)? as u64;
        let json: String = row.get(1)?;
        // A single large provider event must still advance the cursor. Leave
        // every following event for the next page rather than truncating it.
        if scanned != 0
            && (scanned == DELEGATION_EVENT_PAGE_ROWS
                || bytes + json.len() > DELEGATION_EVENT_PAGE_BYTES)
        {
            has_more = true;
            break;
        }
        scanned += 1;
        bytes += json.len();
        next_sequence = sequence;
        let event = match serde_json::from_str(&json) {
            Ok(event) => event,
            Err(error) => {
                eprintln!(
                    "skipping invalid delegation event {sequence} for task {task_id}: {error}"
                );
                continue;
            }
        };
        events.push(DelegatedTaskEvent {
            sequence,
            created_at_ms: (row.get::<_, i64>(2)?.max(0) as u64).saturating_mul(1000),
            event,
        });
    }
    Ok(DelegatedTaskEvents {
        events,
        next_sequence,
        has_more,
    })
}

pub fn delete_delegations_for_session(
    connection: &Connection,
    session_id: Uuid,
) -> anyhow::Result<()> {
    let tx =
        rusqlite::Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate)?;
    let connection = &*tx;
    let groups = list_delegation_groups_by_session(connection, session_id)?;
    let mut snapshots = Vec::new();
    for group in groups {
        if let Some(context) = get_review_context(connection, group.context_id)?
            && let Some(path) = context.snapshot_dir
        {
            snapshots.push(path);
        }
        let tasks = list_delegated_tasks_by_group(connection, group.id)?;
        for task in tasks {
            connection.execute(
                "DELETE FROM delegation_events WHERE task_id = ?1",
                params![task.id.to_string()],
            )?;
        }
        connection.execute(
            "DELETE FROM delegated_tasks WHERE group_id = ?1",
            params![group.id.to_string()],
        )?;
        connection.execute(
            "DELETE FROM review_contexts WHERE id = ?1",
            params![group.context_id.to_string()],
        )?;
    }
    connection.execute(
        "DELETE FROM delegation_groups WHERE parent_session_id = ?1",
        params![session_id.to_string()],
    )?;
    tx.commit()?;
    for snapshot in snapshots {
        super::context::cleanup_snapshot(&snapshot);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use waku_protocol::model::ProviderKind;

    fn connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        crate::persistence::apply_migrations(&connection).unwrap();
        connection
    }

    fn context(id: Uuid) -> CapturedReviewContext {
        CapturedReviewContext {
            id,
            cwd: PathBuf::from("/tmp/project"),
            source: ReviewDiffSource::Uncommitted,
            base_oid: "base".into(),
            target_oid: "target".into(),
            numstat: "1\t0\tsrc/lib.rs".into(),
            patch: "+fn added() {}".into(),
            complete_context: true,
            snapshot_dir: None,
            created_at: unix_time(),
        }
    }

    fn group(id: Uuid, session: Uuid, context: Uuid, request_key: &str) -> DelegationGroupRow {
        DelegationGroupRow {
            id,
            parent_session_id: session,
            parent_turn_id: Some("turn-1".into()),
            parent_runtime_id: None,
            request_key: request_key.into(),
            request_hash: "hash".into(),
            context_id: context,
            status: DelegationGroupStatus::Running,
            created_at: unix_time(),
            updated_at: unix_time(),
        }
    }

    fn task(id: Uuid, group: Uuid) -> DelegatedTaskRow {
        DelegatedTaskRow {
            id,
            group_id: group,
            task_key: "security".into(),
            child_session_id: Uuid::new_v4(),
            runtime_id: None,
            target_config: ModelReference::new(
                "target-1",
                ProviderKind::Codex,
                "gpt-5.5",
                "Codex · GPT-5.5",
            ),
            instructions: "review the diff".into(),
            status: DelegatedTaskStatus::Queued,
            final_conclusion: None,
            error_message: None,
            latest_activity: None,
            started_at: None,
            finished_at: None,
        }
    }

    fn seeded_task() -> (Connection, Uuid, Uuid, Uuid) {
        let connection = connection();
        let session = Uuid::new_v4();
        let context_id = Uuid::new_v4();
        let group_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        insert_review_context(&connection, &context(context_id)).unwrap();
        insert_delegation_group(&connection, &group(group_id, session, context_id, "key")).unwrap();
        insert_delegated_task(&connection, &task(task_id, group_id)).unwrap();
        (connection, session, group_id, task_id)
    }

    #[test]
    fn cancellation_and_retry_reject_late_events_from_older_attempts() {
        let (connection, session, group, task) = seeded_task();
        let old = Uuid::new_v4();
        assert!(begin_delegated_attempt(&connection, task, None, old).unwrap());
        assert!(!begin_delegated_attempt(&connection, task, None, Uuid::new_v4()).unwrap());
        assert!(
            finish_delegated_task(
                &connection,
                task,
                Some(old),
                DelegatedTaskStatus::Cancelled,
                None,
                None
            )
            .unwrap()
        );
        assert!(
            !finish_delegated_task(
                &connection,
                task,
                Some(old),
                DelegatedTaskStatus::Completed,
                Some("late"),
                None
            )
            .unwrap()
        );
        assert!(
            !update_delegated_task_progress(
                &connection,
                task,
                Some(old),
                DelegatedTaskStatus::Running,
                Some("late activity")
            )
            .unwrap()
        );
        let next = Uuid::new_v4();
        reset_delegated_task(&connection, group, task, next).unwrap();
        assert!(begin_delegated_attempt(&connection, task, Some(next), next).unwrap());
        assert!(!append_delegation_event_for_runtime(&connection, task, old, 2, "{}").unwrap());
        assert!(
            !finish_delegated_task(
                &connection,
                task,
                Some(old),
                DelegatedTaskStatus::Failed,
                None,
                Some("late failure")
            )
            .unwrap()
        );
        assert!(
            finish_delegated_task(
                &connection,
                task,
                Some(next),
                DelegatedTaskStatus::Completed,
                Some("new conclusion"),
                None
            )
            .unwrap()
        );
        let saved = get_delegated_task(&connection, task).unwrap().unwrap();
        assert_eq!(saved.to_summary().attempt_id, Some(next));
        assert_eq!(saved.final_conclusion.as_deref(), Some("new conclusion"));
        assert_eq!(
            get_delegation_group(&connection, group)
                .unwrap()
                .unwrap()
                .status,
            DelegationGroupStatus::Completed
        );
        assert_eq!(
            read_delegated_task_events(&connection, session, group, task, 0)
                .unwrap()
                .events
                .len(),
            2
        );
    }

    #[test]
    fn every_non_successful_outcome_commits_the_correct_group_status() {
        for (status, expected) in [
            (
                DelegatedTaskStatus::Completed,
                DelegationGroupStatus::Completed,
            ),
            (
                DelegatedTaskStatus::Cancelled,
                DelegationGroupStatus::Cancelled,
            ),
            (DelegatedTaskStatus::Failed, DelegationGroupStatus::Failed),
            (DelegatedTaskStatus::TimedOut, DelegationGroupStatus::Failed),
            (
                DelegatedTaskStatus::Interrupted,
                DelegationGroupStatus::Failed,
            ),
        ] {
            let (connection, _, group, task) = seeded_task();
            finish_delegated_task(&connection, task, None, status, None, None).unwrap();
            assert_eq!(
                get_delegation_group(&connection, group)
                    .unwrap()
                    .unwrap()
                    .status,
                expected,
                "{status:?}"
            );
        }
    }

    #[test]
    fn invalid_event_json_advances_the_cursor_without_poisoning_replay() {
        let (connection, session, group, task) = seeded_task();
        append_delegation_event(&connection, task, 1, "invalid json").unwrap();
        let event = waku_protocol::WireDriverEvent::new("textDelta", serde_json::json!("kept"));
        append_delegation_event(
            &connection,
            task,
            2,
            &serde_json::to_string(&event).unwrap(),
        )
        .unwrap();
        let page = read_delegated_task_events(&connection, session, group, task, 0).unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].event.payload, serde_json::json!("kept"));
        assert_eq!(page.next_sequence, page.events[0].sequence);
        assert!(
            read_delegated_task_events(&connection, session, group, task, page.next_sequence)
                .unwrap()
                .events
                .is_empty()
        );
    }

    #[test]
    fn session_deletion_reclaims_its_snapshot_and_late_events_cannot_recreate_rows() {
        let (connection, session, group, task) = seeded_task();
        let snapshot =
            std::env::temp_dir().join(format!("waku-deleted-snapshot-{}", Uuid::new_v4()));
        std::fs::create_dir(&snapshot).unwrap();
        std::fs::write(snapshot.join("source.rs"), "source").unwrap();
        connection.execute("UPDATE review_contexts SET snapshot_dir = ?1 WHERE id = (SELECT context_id FROM delegation_groups WHERE id = ?2)",
            params![snapshot.to_string_lossy(), group.to_string()]).unwrap();
        let runtime = Uuid::new_v4();
        begin_delegated_attempt(&connection, task, None, runtime).unwrap();
        delete_delegations_for_session(&connection, session).unwrap();
        assert!(!snapshot.exists());
        assert!(!append_delegation_event_for_runtime(&connection, task, runtime, 2, "{}").unwrap());
        assert!(get_delegated_task(&connection, task).unwrap().is_none());
    }

    #[test]
    fn review_context_round_trips_through_sqlite() {
        let connection = connection();
        let id = Uuid::new_v4();
        insert_review_context(&connection, &context(id)).unwrap();

        let loaded = get_review_context(&connection, id).unwrap().unwrap();
        assert_eq!(loaded.patch, "+fn added() {}");
        assert_eq!(loaded.base_oid, "base");
        assert!(loaded.complete_context);
        assert!(
            get_review_context(&connection, Uuid::new_v4())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn request_key_dedupes_within_a_parent_session_only() {
        let connection = connection();
        let session = Uuid::new_v4();
        let context_id = Uuid::new_v4();
        insert_review_context(&connection, &context(context_id)).unwrap();

        let first = Uuid::new_v4();
        insert_delegation_group(
            &connection,
            &group(first, session, context_id, "review-diff"),
        )
        .unwrap();

        assert_eq!(
            get_delegation_group_by_request_key(&connection, session, "review-diff")
                .unwrap()
                .map(|row| row.id),
            Some(first)
        );
        // Another session reusing the same key is a different batch.
        assert!(
            get_delegation_group_by_request_key(&connection, Uuid::new_v4(), "review-diff")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn terminal_transitions_record_results_and_clear_running_tasks() {
        let connection = connection();
        let session = Uuid::new_v4();
        let context_id = Uuid::new_v4();
        let group_id = Uuid::new_v4();
        insert_review_context(&connection, &context(context_id)).unwrap();
        insert_delegation_group(&connection, &group(group_id, session, context_id, "key")).unwrap();

        let task_id = Uuid::new_v4();
        insert_delegated_task(&connection, &task(task_id, group_id)).unwrap();

        update_delegated_task_progress(
            &connection,
            task_id,
            None,
            DelegatedTaskStatus::Running,
            Some("reading src/lib.rs"),
        )
        .unwrap();
        let running = get_delegated_task(&connection, task_id).unwrap().unwrap();
        assert_eq!(running.status, DelegatedTaskStatus::Running);
        assert!(running.started_at.is_some());
        assert_eq!(
            running.latest_activity.as_deref(),
            Some("reading src/lib.rs")
        );

        finish_delegated_task(
            &connection,
            task_id,
            None,
            DelegatedTaskStatus::Completed,
            Some("no blocking issues"),
            None,
        )
        .unwrap();
        let done = get_delegated_task(&connection, task_id).unwrap().unwrap();
        assert!(done.status.is_terminal());
        assert_eq!(done.final_conclusion.as_deref(), Some("no blocking issues"));
        assert!(done.finished_at.is_some());
        assert!(done.latest_activity.is_none());
        assert!(done.to_summary().latest_activity.is_none());

        let summary = done.to_summary();
        assert_eq!(summary.task_key, "security");
        assert_eq!(summary.status, DelegatedTaskStatus::Completed);
    }

    #[test]
    fn restart_marks_unfinished_tasks_interrupted_instead_of_leaving_them_running() {
        let connection = connection();
        let session = Uuid::new_v4();
        let context_id = Uuid::new_v4();
        let group_id = Uuid::new_v4();
        insert_review_context(&connection, &context(context_id)).unwrap();
        insert_delegation_group(&connection, &group(group_id, session, context_id, "key")).unwrap();
        let task_id = Uuid::new_v4();
        insert_delegated_task(&connection, &task(task_id, group_id)).unwrap();
        update_delegated_task_progress(
            &connection,
            task_id,
            None,
            DelegatedTaskStatus::Running,
            Some("Search files"),
        )
        .unwrap();

        interrupt_unfinished_delegations(&connection).unwrap();

        let recovered = get_delegated_task(&connection, task_id).unwrap().unwrap();
        assert_eq!(recovered.status, DelegatedTaskStatus::Interrupted);
        assert!(recovered.error_message.is_some());
        assert!(recovered.latest_activity.is_none());
        assert!(recovered.to_summary().latest_activity.is_none());
        assert_eq!(
            get_delegation_group(&connection, group_id)
                .unwrap()
                .unwrap()
                .status,
            DelegationGroupStatus::Failed
        );
    }

    #[test]
    fn removing_a_session_drops_its_groups_tasks_events_and_context() {
        let connection = connection();
        let session = Uuid::new_v4();
        let context_id = Uuid::new_v4();
        let group_id = Uuid::new_v4();
        insert_review_context(&connection, &context(context_id)).unwrap();
        insert_delegation_group(&connection, &group(group_id, session, context_id, "key")).unwrap();
        let task_id = Uuid::new_v4();
        insert_delegated_task(&connection, &task(task_id, group_id)).unwrap();
        append_delegation_event(&connection, task_id, 1, "{}").unwrap();

        delete_delegations_for_session(&connection, session).unwrap();

        assert!(
            get_delegation_group(&connection, group_id)
                .unwrap()
                .is_none()
        );
        assert!(get_delegated_task(&connection, task_id).unwrap().is_none());
        assert!(
            get_review_context(&connection, context_id)
                .unwrap()
                .is_none()
        );
        let events: i64 = connection
            .query_row("SELECT COUNT(*) FROM delegation_events", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(events, 0);
    }

    #[test]
    fn legacy_terminal_rows_never_project_stale_activity() {
        for status in [
            DelegatedTaskStatus::Completed,
            DelegatedTaskStatus::Failed,
            DelegatedTaskStatus::Cancelled,
            DelegatedTaskStatus::TimedOut,
            DelegatedTaskStatus::Interrupted,
        ] {
            let mut row = task(Uuid::new_v4(), Uuid::new_v4());
            row.status = status;
            row.latest_activity = Some("Search files".into());
            assert!(row.to_summary().latest_activity.is_none(), "{status:?}");
        }
    }

    #[test]
    fn event_pages_preserve_order_and_resume_across_provider_sequence_restarts() {
        let connection = connection();
        let session = Uuid::new_v4();
        let context_id = Uuid::new_v4();
        let group_id = Uuid::new_v4();
        insert_review_context(&connection, &context(context_id)).unwrap();
        insert_delegation_group(&connection, &group(group_id, session, context_id, "key")).unwrap();
        let task_id = Uuid::new_v4();
        insert_delegated_task(&connection, &task(task_id, group_id)).unwrap();
        for index in 0..DELEGATION_EVENT_PAGE_ROWS + 1 {
            let event = waku_protocol::WireDriverEvent::new(
                if index % 2 == 0 {
                    "reasoningDelta"
                } else {
                    "textDelta"
                },
                serde_json::json!(format!("chunk {index}")),
            );
            // Simulate a retry: original provider sequence numbers repeat.
            append_delegation_event(
                &connection,
                task_id,
                1,
                &serde_json::to_string(&event).unwrap(),
            )
            .unwrap();
        }
        connection
            .execute(
                "UPDATE delegation_events SET created_at = 1234567890 + id",
                [],
            )
            .unwrap();
        let first = read_delegated_task_events(&connection, session, group_id, task_id, 0).unwrap();
        assert!(first.has_more);
        assert_eq!(first.events.len(), DELEGATION_EVENT_PAGE_ROWS);
        for (index, event) in first.events.iter().enumerate() {
            assert_eq!(event.created_at_ms, (1234567890 + event.sequence) * 1000);
            assert_eq!(
                event.event.payload,
                serde_json::json!(format!("chunk {index}"))
            );
            assert_eq!(
                event.event.kind,
                if index % 2 == 0 {
                    "reasoningDelta"
                } else {
                    "textDelta"
                }
            );
        }
        assert!(
            first
                .events
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence)
        );
        assert_eq!(first.next_sequence, first.events.last().unwrap().sequence);
        let second = read_delegated_task_events(
            &connection,
            session,
            group_id,
            task_id,
            first.next_sequence,
        )
        .unwrap();
        assert!(!second.has_more);
        assert_eq!(second.events.len(), 1);
        assert_eq!(
            second.events[0].event.payload,
            serde_json::json!(format!("chunk {}", DELEGATION_EVENT_PAGE_ROWS)),
        );
        assert!(second.next_sequence > first.next_sequence);
        let empty = read_delegated_task_events(
            &connection,
            session,
            group_id,
            task_id,
            second.next_sequence,
        )
        .unwrap();
        assert!(empty.events.is_empty());
        assert!(!empty.has_more);
        assert_eq!(empty.next_sequence, second.next_sequence);
        for (session, group, task) in [
            (Uuid::new_v4(), group_id, task_id),
            (session, Uuid::new_v4(), task_id),
            (session, group_id, Uuid::new_v4()),
        ] {
            assert!(read_delegated_task_events(&connection, session, group, task, 0).is_err());
        }
    }

    #[test]
    fn a_large_event_is_not_truncated_or_allowed_to_stall_pagination() {
        let connection = connection();
        let session = Uuid::new_v4();
        let context_id = Uuid::new_v4();
        let group_id = Uuid::new_v4();
        insert_review_context(&connection, &context(context_id)).unwrap();
        insert_delegation_group(&connection, &group(group_id, session, context_id, "key")).unwrap();
        let task_id = Uuid::new_v4();
        insert_delegated_task(&connection, &task(task_id, group_id)).unwrap();
        let thought = "思考".repeat(DELEGATION_EVENT_PAGE_BYTES / 6 + 1);
        for (sequence, text) in [(1, thought.as_str()), (2, "conclusion")] {
            let event = waku_protocol::WireDriverEvent::new("textDelta", serde_json::json!(text));
            append_delegation_event(
                &connection,
                task_id,
                sequence,
                &serde_json::to_string(&event).unwrap(),
            )
            .unwrap();
        }
        let first = read_delegated_task_events(&connection, session, group_id, task_id, 0).unwrap();
        assert!(first.has_more);
        assert_eq!(first.events.len(), 1);
        assert_eq!(first.events[0].event.payload, serde_json::json!(thought));
        let second = read_delegated_task_events(
            &connection,
            session,
            group_id,
            task_id,
            first.next_sequence,
        )
        .unwrap();
        assert!(!second.has_more);
        assert_eq!(second.events.len(), 1);
        assert_eq!(
            second.events[0].event.payload,
            serde_json::json!("conclusion")
        );
    }
}
