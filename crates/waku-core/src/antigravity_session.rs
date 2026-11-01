//! Google Antigravity CLI (`agy`) native SQLite session index and history import.
//!
//! `agy` stores top-level and subagent metadata in
//! `~/.gemini/antigravity-cli/conversation_summaries.db` and each conversation's
//! step trajectory in `~/.gemini/antigravity-cli/conversations/<uuid>.db`.
//! Reading those SQLite files directly in read-only mode avoids spawning `agy`
//! subprocesses for `/resume` discovery, cold history import, or background
//! title refreshes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use uuid::Uuid;

use crate::model::{
    AgentTurn, Message, MessageRole, ProviderKind, ProviderResumeCursor, ProviderSessionHistory,
    ProviderSessionSummary, TurnStatus,
};

const STEP_TYPE_USER_INPUT: i64 = 14;
const STEP_TYPE_AGENT_RESPONSE: i64 = 15;

pub fn data_directory() -> anyhow::Result<PathBuf> {
    if let Some(override_dir) = std::env::var_os("AGY_DATA_DIR").filter(|val| !val.is_empty()) {
        return Ok(PathBuf::from(override_dir));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow!("Antigravity CLI's data directory could not be located"))?;
    Ok(home.join(".gemini/antigravity-cli"))
}

pub fn session_title(session_id: &str) -> Option<String> {
    let dir = data_directory().ok()?;
    session_title_in(&dir, session_id)
}

pub(crate) fn session_title_in(data_dir: &Path, session_id: &str) -> Option<String> {
    let db_path = data_dir.join("conversation_summaries.db");
    if !db_path.is_file() {
        return None;
    }
    let conn = open_readonly(&db_path).ok()?;
    let title: String = conn
        .query_row(
            "SELECT title FROM conversation_summaries WHERE conversation_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .ok()?;
    clean_single_line(&title)
}

/// List top-level Antigravity CLI conversations that can be resumed with
/// `agy --conversation <id>`. Subagent trajectories (`nesting_depth > 0` or
/// non-empty `parent_conversation_id`) and empty or killed runs are excluded.
pub fn list_provider_sessions(limit: usize) -> anyhow::Result<Vec<ProviderSessionSummary>> {
    list_provider_sessions_in(&data_directory()?, limit)
}

pub(crate) fn list_provider_sessions_in(
    data_dir: &Path,
    limit: usize,
) -> anyhow::Result<Vec<ProviderSessionSummary>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let db_path = data_dir.join("conversation_summaries.db");
    if !db_path.is_file() {
        return Ok(Vec::new());
    }
    let conn = open_readonly(&db_path).with_context(|| {
        format!(
            "could not open Antigravity session index at {}",
            db_path.display()
        )
    })?;
    let query_limit = i64::try_from(limit.saturating_mul(2).max(64)).unwrap_or(256);
    let mut stmt = conn.prepare(
        "SELECT
            conversation_id,
            title,
            preview,
            CAST(last_modified_time AS TEXT),
            CAST(last_user_input_time AS TEXT),
            workspace_uris
         FROM conversation_summaries
         WHERE nesting_depth = 0
           AND parent_conversation_id = ''
           AND step_count > 0
           AND killed = 0
         ORDER BY last_modified_time DESC
         LIMIT ?1",
    )?;
    let fallback_cwd = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
    let conversations_dir = data_dir.join("conversations");
    let rows = stmt.query_map([query_limit], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        ))
    })?;

    let mut sessions = Vec::new();
    for row in rows {
        let Ok((session_id, title, preview, modified_at, user_input_at, workspace_uris)) = row
        else {
            continue;
        };
        if Uuid::parse_str(&session_id).is_err() {
            continue;
        }
        let session_db = conversations_dir.join(format!("{session_id}.db"));
        if !session_db.is_file() {
            continue;
        }
        let updated_at = parse_sqlite_timestamp(&modified_at)
            .or_else(|| parse_sqlite_timestamp(&user_input_at))
            .or_else(|| file_modified_unix(&session_db))
            .unwrap_or_default();
        let created_at = parse_sqlite_timestamp(&user_input_at)
            .filter(|ts| *ts > 0 && (*ts <= updated_at || updated_at == 0))
            .unwrap_or(updated_at);
        let cwd = parse_workspace_cwd(&workspace_uris).unwrap_or_else(|| fallback_cwd.clone());
        let raw_title = clean_single_line(&title).or_else(|| clean_single_line(&preview));
        let title = crate::session_catalog::session_title(
            ProviderKind::Antigravity,
            raw_title.as_deref(),
            &session_id,
        );
        sessions.push(ProviderSessionSummary {
            cursor: ProviderResumeCursor::Antigravity { session_id },
            title,
            cwd,
            created_at,
            updated_at: updated_at.max(created_at),
        });
        if sessions.len() >= limit {
            break;
        }
    }
    Ok(sessions)
}

/// Import displayable user and assistant turns from an Antigravity SQLite
/// trajectory database (`conversations/<session_id>.db`).
pub fn provider_session_history(
    session_id: &str,
    turn_limit: usize,
) -> anyhow::Result<ProviderSessionHistory> {
    provider_session_history_in(&data_directory()?, session_id, turn_limit)
}

pub(crate) fn provider_session_history_in(
    data_dir: &Path,
    session_id: &str,
    turn_limit: usize,
) -> anyhow::Result<ProviderSessionHistory> {
    Uuid::parse_str(session_id).context("Antigravity returned an invalid conversation ID")?;
    let db_path = data_dir
        .join("conversations")
        .join(format!("{session_id}.db"));
    if !db_path.is_file() {
        bail!("Antigravity conversation {session_id} was not found on disk");
    }
    let conn = open_readonly(&db_path).with_context(|| {
        format!(
            "could not open Antigravity conversation {}",
            db_path.display()
        )
    })?;
    let fallback_ts = file_modified_unix(&db_path).unwrap_or_else(crate::model::unix_time);
    let mut stmt = conn.prepare(
        "SELECT step_type, step_payload
         FROM steps
         WHERE step_type IN (?1, ?2)
         ORDER BY idx ASC",
    )?;
    let rows = stmt.query_map([STEP_TYPE_USER_INPUT, STEP_TYPE_AGENT_RESPONSE], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<Vec<u8>>>(1)?.unwrap_or_default(),
        ))
    })?;

    let mut history = ProviderSessionHistory::default();
    let mut skipping_compaction_turn = false;

    for row in rows {
        let (step_type, payload) = row?;
        let timestamp = decode_step_timestamp(&payload).unwrap_or(fallback_ts);
        if step_type == STEP_TYPE_USER_INPUT {
            let Some(prompt) = decode_user_input_text(&payload) else {
                continue;
            };
            if prompt.trim() == "/compact" {
                skipping_compaction_turn = true;
                continue;
            }
            skipping_compaction_turn = false;
            let turn_id = Uuid::new_v4();
            history.turns.push(AgentTurn {
                id: turn_id,
                turn_count: history.turns.len() + 1,
                status: TurnStatus::Completed,
                provider_turn_started: true,
                provider_resume_at: None,
                model: None,
                reasoning_effort: None,
                started_at: timestamp,
                completed_at: Some(timestamp),
                checkpoint: None,
            });
            let mut message = Message::new_for_turn(MessageRole::User, prompt, turn_id);
            message.created_at = timestamp;
            history.messages.push(message);
        } else if step_type == STEP_TYPE_AGENT_RESPONSE {
            if skipping_compaction_turn {
                continue;
            }
            let Some(turn) = history.turns.last_mut() else {
                continue;
            };
            turn.completed_at = Some(turn.completed_at.unwrap_or(timestamp).max(timestamp));
            let Some(content) = decode_agent_response_text(&payload) else {
                continue;
            };
            if let Some(previous) = history.messages.last_mut().filter(|message| {
                message.turn_id == Some(turn.id) && message.role == MessageRole::Assistant
            }) {
                if !previous.content.is_empty() {
                    previous.content.push_str("\n\n");
                }
                previous.content.push_str(&content);
                previous.created_at = previous.created_at.max(timestamp);
            } else {
                let mut message = Message::new_for_turn(MessageRole::Assistant, content, turn.id);
                message.created_at = timestamp;
                history.messages.push(message);
            }
        }
    }

    if history.turns.len() > turn_limit {
        let retained = history
            .turns
            .iter()
            .rev()
            .take(turn_limit)
            .map(|turn| turn.id)
            .collect::<HashSet<_>>();
        history
            .messages
            .retain(|message| message.turn_id.is_some_and(|id| retained.contains(&id)));
    }

    Ok(history)
}

/// Fork an Antigravity SQLite conversation database (`conversations/<id>.db`)
/// through `provider_turn_count` completed user turns, returning the new
/// conversation UUID.
pub fn fork_session_at_turn(
    session_id: &str,
    provider_turn_count: usize,
    title: &str,
) -> anyhow::Result<String> {
    fork_session_at_turn_in(&data_directory()?, session_id, provider_turn_count, title)
}

pub(crate) fn fork_session_at_turn_in(
    data_dir: &Path,
    session_id: &str,
    provider_turn_count: usize,
    title: &str,
) -> anyhow::Result<String> {
    Uuid::parse_str(session_id).context("Antigravity returned an invalid conversation ID")?;
    if provider_turn_count == 0 {
        bail!("Antigravity has no native turn at that checkpoint");
    }
    let conversations_dir = data_dir.join("conversations");
    let source_db = conversations_dir.join(format!("{session_id}.db"));
    if !source_db.is_file() {
        bail!("Antigravity conversation {session_id} was not found on disk");
    }
    let source_conn = open_readonly(&source_db).with_context(|| {
        format!(
            "could not open Antigravity conversation {}",
            source_db.display()
        )
    })?;
    let cutoff_idx = find_turn_cutoff_idx(&source_conn, provider_turn_count)?;

    let forked_session_id = Uuid::new_v4().to_string();
    let forked_trajectory_id = Uuid::new_v4().to_string();
    let fork_db = conversations_dir.join(format!("{forked_session_id}.db"));
    let mut dest_conn = Connection::open(&fork_db).with_context(|| {
        format!(
            "could not create Antigravity conversation fork {}",
            fork_db.display()
        )
    })?;
    let _ = dest_conn.busy_timeout(Duration::from_millis(250));
    {
        let backup = rusqlite::backup::Backup::new(&source_conn, &mut dest_conn)
            .context("could not initialize Antigravity SQLite backup")?;
        backup
            .run_to_completion(256, Duration::from_millis(5), None)
            .context("could not copy Antigravity conversation database")?;
    }
    drop(source_conn);

    let finalize = (|| -> anyhow::Result<(i64, i64)> {
        if table_exists(&dest_conn, "trajectory_meta") {
            dest_conn.execute(
                "UPDATE trajectory_meta SET trajectory_id = ?1, cascade_id = ?2",
                [&forked_trajectory_id, &forked_session_id],
            )?;
        }
        if table_exists(&dest_conn, "trajectory_metadata_blob") {
            let blobs = {
                let mut stmt =
                    dest_conn.prepare("SELECT id, data FROM trajectory_metadata_blob")?;
                stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (id, data) in blobs {
                if let Some(bytes) = data
                    && let Some(rewritten) = pb_replace_string_fields(
                        &bytes,
                        &[(3, &forked_trajectory_id), (6, &forked_session_id)],
                    )
                {
                    dest_conn.execute(
                        "UPDATE trajectory_metadata_blob SET data = ?1 WHERE id = ?2",
                        rusqlite::params![rewritten, id],
                    )?;
                }
            }
        }
        if let Some(cutoff) = cutoff_idx {
            let cutoff_u64 = u64::try_from(cutoff).unwrap_or(0);
            dest_conn.execute("DELETE FROM steps WHERE idx >= ?1", [cutoff])?;
            prune_step_indexed_table(&dest_conn, "gen_metadata", |data| {
                pb_any_repeated_varint_at_least(data, 2, cutoff_u64)
            })?;
            prune_step_indexed_table(&dest_conn, "executor_metadata", |data| {
                pb_find_varint(data, 3).is_some_and(|step_idx| step_idx >= cutoff_u64)
            })?;
            prune_step_indexed_table(&dest_conn, "battle_mode_infos", |data| {
                // BattleModeInfo: start_step_index = 3, end_step_index = 4.
                // Drop a battle if either boundary reaches the discarded steps.
                [3, 4].into_iter().any(|field| {
                    pb_find_varint(data, field).is_some_and(|step_idx| step_idx >= cutoff_u64)
                })
            })?;
        }

        let step_count: i64 =
            dest_conn.query_row("SELECT COUNT(*) FROM steps", [], |row| row.get(0))?;
        if step_count == 0 {
            bail!("Antigravity conversation {session_id} has no steps to fork");
        }
        let last_user_step_idx: i64 = dest_conn
            .query_row(
                "SELECT COALESCE(MAX(idx), -1) FROM steps WHERE step_type = ?1",
                [STEP_TYPE_USER_INPUT],
                |row| row.get(0),
            )
            .unwrap_or(-1);
        Ok((step_count, last_user_step_idx))
    })();

    let (step_count, last_user_step_idx) = match finalize {
        Ok(counts) => counts,
        Err(error) => {
            drop(dest_conn);
            let _ = std::fs::remove_file(&fork_db);
            return Err(error);
        }
    };
    drop(dest_conn);

    let _ = clone_summary_row(
        data_dir,
        session_id,
        &forked_session_id,
        title,
        step_count,
        last_user_step_idx,
    );

    Ok(forked_session_id)
}

fn find_turn_cutoff_idx(
    conn: &Connection,
    provider_turn_count: usize,
) -> anyhow::Result<Option<i64>> {
    let mut stmt = conn.prepare(
        "SELECT idx, step_payload
         FROM steps
         WHERE step_type = ?1
         ORDER BY idx ASC",
    )?;
    let rows = stmt.query_map([STEP_TYPE_USER_INPUT], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<Vec<u8>>>(1)?.unwrap_or_default(),
        ))
    })?;

    let mut seen_turns = 0usize;
    let mut cutoff_idx = None;
    for row in rows {
        let (idx, payload) = row?;
        let Some(prompt) = decode_user_input_text(&payload) else {
            continue;
        };
        if prompt.trim() == "/compact" {
            if seen_turns >= provider_turn_count {
                cutoff_idx = Some(idx);
                break;
            }
            continue;
        }
        if seen_turns >= provider_turn_count {
            cutoff_idx = Some(idx);
            break;
        }
        seen_turns += 1;
    }

    if seen_turns < provider_turn_count {
        bail!("Antigravity's checkpoint for that turn was not found");
    }
    Ok(cutoff_idx)
}

fn table_exists(conn: &Connection, table_name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1",
        [table_name],
        |_| Ok(()),
    )
    .is_ok()
}

fn prune_step_indexed_table(
    conn: &Connection,
    table_name: &str,
    mut should_prune: impl FnMut(&[u8]) -> bool,
) -> anyhow::Result<()> {
    if !table_exists(conn, table_name) {
        return Ok(());
    }
    let rows = {
        let mut stmt = conn.prepare(&format!("SELECT idx, data FROM \"{table_name}\""))?;
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?.unwrap_or_default(),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let delete_sql = format!("DELETE FROM \"{table_name}\" WHERE idx = ?1");
    for (idx, data) in rows {
        if should_prune(&data) {
            conn.execute(&delete_sql, [idx])?;
        }
    }
    Ok(())
}

fn clone_summary_row(
    data_dir: &Path,
    source_session_id: &str,
    forked_session_id: &str,
    title: &str,
    step_count: i64,
    last_user_step_idx: i64,
) -> anyhow::Result<()> {
    let summary_db = data_dir.join("conversation_summaries.db");
    if !summary_db.is_file() {
        return Ok(());
    }
    let conn = Connection::open_with_flags(
        &summary_db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let _ = conn.busy_timeout(Duration::from_millis(250));
    if !table_exists(&conn, "conversation_summaries") {
        return Ok(());
    }
    let columns = {
        let mut stmt = conn.prepare("PRAGMA table_info(conversation_summaries)")?;
        stmt.query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if columns.is_empty() {
        return Ok(());
    }
    let fork_title = clean_single_line(title).unwrap_or_else(|| "Waku fork".to_owned());
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let col_list = columns
        .iter()
        .map(|col| format!("\"{}\"", col.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let select_list = columns
        .iter()
        .map(|col| match col.as_str() {
            "conversation_id" => "?2".to_owned(),
            "title" => "?3".to_owned(),
            "last_modified_time" => "?4".to_owned(),
            "step_count" => step_count.to_string(),
            "last_user_input_step_index" => last_user_step_idx.to_string(),
            "raw_summary" => "NULL".to_owned(),
            "parent_conversation_id" => "''".to_owned(),
            "nesting_depth" | "killed" | "not_fully_idle" => "0".to_owned(),
            other => format!("\"{}\"", other.replace('"', "\"\"")),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT OR REPLACE INTO conversation_summaries ({col_list})
         SELECT {select_list}
         FROM conversation_summaries
         WHERE conversation_id = ?1"
    );
    conn.execute(
        &sql,
        rusqlite::params![source_session_id, forked_session_id, fork_title, now],
    )?;
    Ok(())
}

fn open_readonly(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let _ = conn.busy_timeout(Duration::from_millis(250));
    Ok(conn)
}

fn clean_single_line(text: &str) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?.trim();
    if line.is_empty() {
        return None;
    }
    let truncated: String = line.chars().take(120).collect();
    Some(truncated)
}

fn parse_workspace_cwd(raw_json: &str) -> Option<PathBuf> {
    let trimmed = raw_json.trim();
    if trimmed.is_empty() {
        return None;
    }
    let uris = serde_json::from_str::<Vec<String>>(trimmed).ok()?;
    uris.into_iter().find_map(|entry| {
        let entry = entry.trim();
        if entry.is_empty() {
            return None;
        }
        if let Ok(url) = url::Url::parse(entry)
            && url.scheme() == "file"
            && let Ok(path) = url.to_file_path()
        {
            return Some(path);
        }
        let path = PathBuf::from(entry);
        path.is_absolute().then_some(path)
    })
}

fn parse_sqlite_timestamp(raw: &str) -> Option<u64> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return u64::try_from(dt.timestamp()).ok();
    }
    // Go's `time.Time.String()` / SQLite datetime variants (`2026-11-01 18:07:23.123 +0800 ...`).
    let prefix = text
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f %z",
        "%Y-%m-%d %H:%M:%S %z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%:z",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(&prefix, fmt) {
            return u64::try_from(dt.timestamp()).ok();
        }
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, fmt) {
            return u64::try_from(naive.and_utc().timestamp()).ok();
        }
    }
    if let Ok(num) = text.parse::<u64>() {
        return Some(if num > 10_000_000_000 {
            num / 1_000
        } else {
            num
        });
    }
    let _ = Value::Null;
    None
}

fn file_modified_unix(path: &Path) -> Option<u64> {
    path.metadata()
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

// ── Minimal Protobuf wire readers for `gemini_coder_go_proto.Step` ──────────
//
// Verified against live `agy` 1.4.x SQLite trajectories:
// - `Step.metadata` is field 5 (`StepMetadata`), whose `created_at` is field 1
//   (`google.protobuf.Timestamp`, `seconds` = field 1).
// - `Step.user_input` is field 19 (`UserInput`), whose user prompt string is
//   field 2.
// - `Step.planner_response` is field 20 (`PlannerResponse`), whose markdown
//   response string is field 1 (with modified/display copy at field 8).

fn decode_step_timestamp(step_bytes: &[u8]) -> Option<u64> {
    let metadata = pb_find_bytes(step_bytes, 5)?;
    let ts = pb_find_bytes(metadata, 1)?;
    let seconds = pb_find_varint(ts, 1)?;
    (seconds > 0).then_some(seconds)
}

fn decode_user_input_text(step_bytes: &[u8]) -> Option<String> {
    let user_input = pb_find_bytes(step_bytes, 19)?;
    pb_find_non_empty_string(user_input, 2)
}

fn decode_agent_response_text(step_bytes: &[u8]) -> Option<String> {
    let response = pb_find_bytes(step_bytes, 20)?;
    pb_find_non_empty_string(response, 1).or_else(|| pb_find_non_empty_string(response, 8))
}

fn pb_find_non_empty_string(buf: &[u8], target_field: u32) -> Option<String> {
    let raw = pb_find_bytes(buf, target_field)?;
    let text = std::str::from_utf8(raw).ok()?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn pb_find_varint(buf: &[u8], target_field: u32) -> Option<u64> {
    let mut pos = 0;
    while pos < buf.len() {
        let (tag, next) = pb_read_varint(buf, pos)?;
        pos = next;
        let field_number = u32::try_from(tag >> 3).ok()?;
        let wire_type = tag & 0x07;
        match wire_type {
            0 => {
                let (val, next) = pb_read_varint(buf, pos)?;
                pos = next;
                if field_number == target_field {
                    return Some(val);
                }
            }
            1 => pos = pos.checked_add(8)?,
            2 => {
                let (len, next) = pb_read_varint(buf, pos)?;
                let len = usize::try_from(len).ok()?;
                pos = next.checked_add(len)?;
            }
            5 => pos = pos.checked_add(4)?,
            _ => return None,
        }
    }
    None
}

fn pb_find_bytes(buf: &[u8], target_field: u32) -> Option<&[u8]> {
    let mut pos = 0;
    while pos < buf.len() {
        let (tag, next) = pb_read_varint(buf, pos)?;
        pos = next;
        let field_number = u32::try_from(tag >> 3).ok()?;
        let wire_type = tag & 0x07;
        match wire_type {
            0 => {
                let (_, next) = pb_read_varint(buf, pos)?;
                pos = next;
            }
            1 => pos = pos.checked_add(8)?,
            2 => {
                let (len, next) = pb_read_varint(buf, pos)?;
                let len = usize::try_from(len).ok()?;
                let end = next.checked_add(len)?;
                if end > buf.len() {
                    return None;
                }
                if field_number == target_field {
                    return Some(&buf[next..end]);
                }
                pos = end;
            }
            5 => pos = pos.checked_add(4)?,
            _ => return None,
        }
    }
    None
}

fn pb_read_varint(buf: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut value = 0_u64;
    let mut shift = 0_u32;
    while pos < buf.len() {
        let byte = buf[pos];
        pos += 1;
        if shift >= 64 {
            return None;
        }
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some((value, pos));
        }
        shift += 7;
    }
    None
}

fn pb_push_varint(out: &mut Vec<u8>, mut val: u64) {
    while val >= 0x80 {
        out.push((val as u8 & 0x7F) | 0x80);
        val >>= 7;
    }
    out.push(val as u8);
}

fn pb_any_repeated_varint_at_least(buf: &[u8], target_field: u32, min_val: u64) -> bool {
    let mut pos = 0;
    while pos < buf.len() {
        let Some((tag, next)) = pb_read_varint(buf, pos) else {
            return false;
        };
        pos = next;
        let Ok(field_number) = u32::try_from(tag >> 3) else {
            return false;
        };
        let wire_type = tag & 0x07;
        match wire_type {
            0 => {
                let Some((val, next)) = pb_read_varint(buf, pos) else {
                    return false;
                };
                pos = next;
                if field_number == target_field && val >= min_val {
                    return true;
                }
            }
            1 => {
                let Some(next) = pos.checked_add(8) else {
                    return false;
                };
                pos = next;
            }
            2 => {
                let Some((len, next)) = pb_read_varint(buf, pos) else {
                    return false;
                };
                let Ok(len) = usize::try_from(len) else {
                    return false;
                };
                let Some(end) = next.checked_add(len) else {
                    return false;
                };
                if end > buf.len() {
                    return false;
                }
                if field_number == target_field {
                    let mut sub = next;
                    while sub < end {
                        let Some((val, sub_next)) = pb_read_varint(buf, sub) else {
                            break;
                        };
                        sub = sub_next;
                        if val >= min_val {
                            return true;
                        }
                    }
                }
                pos = end;
            }
            5 => {
                let Some(next) = pos.checked_add(4) else {
                    return false;
                };
                pos = next;
            }
            _ => return false,
        }
    }
    false
}

fn pb_replace_string_fields(buf: &[u8], replacements: &[(u32, &str)]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(buf.len());
    let mut pos = 0;
    while pos < buf.len() {
        let field_start = pos;
        let (tag, next) = pb_read_varint(buf, pos)?;
        pos = next;
        let field_number = u32::try_from(tag >> 3).ok()?;
        let wire_type = tag & 0x07;
        match wire_type {
            0 => {
                let (_, end) = pb_read_varint(buf, pos)?;
                out.extend_from_slice(&buf[field_start..end]);
                pos = end;
            }
            1 => {
                let end = pos.checked_add(8)?;
                if end > buf.len() {
                    return None;
                }
                out.extend_from_slice(&buf[field_start..end]);
                pos = end;
            }
            2 => {
                let (len, data_start) = pb_read_varint(buf, pos)?;
                let len = usize::try_from(len).ok()?;
                let end = data_start.checked_add(len)?;
                if end > buf.len() {
                    return None;
                }
                if let Some((_, replacement)) = replacements
                    .iter()
                    .find(|(target, _)| *target == field_number)
                {
                    pb_push_varint(&mut out, tag);
                    pb_push_varint(&mut out, replacement.len() as u64);
                    out.extend_from_slice(replacement.as_bytes());
                } else {
                    out.extend_from_slice(&buf[field_start..end]);
                }
                pos = end;
            }
            5 => {
                let end = pos.checked_add(4)?;
                if end > buf.len() {
                    return None;
                }
                out.extend_from_slice(&buf[field_start..end]);
                pos = end;
            }
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pb_varint_field(field: u32, mut val: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut tag = u64::from(field) << 3;
        while tag >= 0x80 {
            out.push((tag as u8 & 0x7F) | 0x80);
            tag >>= 7;
        }
        out.push(tag as u8);
        while val >= 0x80 {
            out.push((val as u8 & 0x7F) | 0x80);
            val >>= 7;
        }
        out.push(val as u8);
        out
    }

    fn pb_bytes_field(field: u32, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut tag = (u64::from(field) << 3) | 2;
        while tag >= 0x80 {
            out.push((tag as u8 & 0x7F) | 0x80);
            tag >>= 7;
        }
        out.push(tag as u8);
        let mut len = bytes.len() as u64;
        while len >= 0x80 {
            out.push((len as u8 & 0x7F) | 0x80);
            len >>= 7;
        }
        out.push(len as u8);
        out.extend_from_slice(bytes);
        out
    }

    fn make_user_step(prompt: &str, ts_secs: u64) -> Vec<u8> {
        let ts = pb_varint_field(1, ts_secs);
        let meta = pb_bytes_field(1, &ts);
        let user_input = pb_bytes_field(2, prompt.as_bytes());
        let mut step = pb_varint_field(1, STEP_TYPE_USER_INPUT as u64);
        step.extend(pb_bytes_field(5, &meta));
        step.extend(pb_bytes_field(19, &user_input));
        step
    }

    fn make_assistant_step(reply: &str, ts_secs: u64) -> Vec<u8> {
        let ts = pb_varint_field(1, ts_secs);
        let meta = pb_bytes_field(1, &ts);
        let resp = pb_bytes_field(1, reply.as_bytes());
        let mut step = pb_varint_field(1, STEP_TYPE_AGENT_RESPONSE as u64);
        step.extend(pb_bytes_field(5, &meta));
        step.extend(pb_bytes_field(20, &resp));
        step
    }

    #[test]
    fn lists_top_level_sessions_and_imports_turns_skipping_compaction() {
        let root = std::env::temp_dir().join(format!("waku-agy-session-test-{}", Uuid::new_v4()));
        let conv_dir = root.join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();

        let top_id = Uuid::new_v4().to_string();
        let child_id = Uuid::new_v4().to_string();

        for id in [&top_id, &child_id] {
            let conn = Connection::open(conv_dir.join(format!("{id}.db"))).unwrap();
            conn.execute_batch(
                "CREATE TABLE steps (
                    idx INTEGER PRIMARY KEY,
                    step_type INTEGER NOT NULL,
                    step_payload BLOB
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO steps (idx, step_type, step_payload) VALUES (0, ?1, ?2)",
                rusqlite::params![STEP_TYPE_USER_INPUT, make_user_step("Explain rebase", 1_700_000_010)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO steps (idx, step_type, step_payload) VALUES (1, ?1, ?2)",
                rusqlite::params![
                    STEP_TYPE_AGENT_RESPONSE,
                    make_assistant_step("Rebase replays commits.", 1_700_000_015)
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO steps (idx, step_type, step_payload) VALUES (2, ?1, ?2)",
                rusqlite::params![STEP_TYPE_USER_INPUT, make_user_step("/compact", 1_700_000_020)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO steps (idx, step_type, step_payload) VALUES (3, ?1, ?2)",
                rusqlite::params![STEP_TYPE_USER_INPUT, make_user_step("Follow up", 1_700_000_030)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO steps (idx, step_type, step_payload) VALUES (4, ?1, ?2)",
                rusqlite::params![
                    STEP_TYPE_AGENT_RESPONSE,
                    make_assistant_step("Second answer.", 1_700_000_035)
                ],
            )
            .unwrap();
        }

        let summary_conn = Connection::open(root.join("conversation_summaries.db")).unwrap();
        summary_conn
            .execute_batch(
                "CREATE TABLE conversation_summaries (
                    conversation_id TEXT PRIMARY KEY,
                    title TEXT NOT NULL DEFAULT '',
                    preview TEXT NOT NULL DEFAULT '',
                    step_count INTEGER NOT NULL DEFAULT 0,
                    last_modified_time TEXT NOT NULL,
                    last_user_input_time TEXT NOT NULL,
                    workspace_uris TEXT NOT NULL,
                    parent_conversation_id TEXT NOT NULL DEFAULT '',
                    nesting_depth INTEGER NOT NULL DEFAULT 0,
                    killed NUMERIC NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        summary_conn
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, 'Git Rebase Guide', 'Explain rebase', 5, '2026-11-01T10:00:35Z', '2026-11-01T10:00:10Z', '[\"file:///tmp/demo\"]', '', 0, 0)",
                [&top_id],
            )
            .unwrap();
        summary_conn
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, 'Subagent Run', 'child', 2, '2026-11-01T10:01:00Z', '2026-11-01T10:01:00Z', '[\"file:///tmp/demo\"]', ?2, 1, 0)",
                [&child_id, &top_id],
            )
            .unwrap();

        assert_eq!(
            session_title_in(&root, &top_id).as_deref(),
            Some("Git Rebase Guide")
        );

        let listed = list_provider_sessions_in(&root, 20).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].cursor.native_id(), top_id);
        assert_eq!(listed[0].title, "Git Rebase Guide");
        assert_eq!(listed[0].cwd, PathBuf::from("/tmp/demo"));

        let history = provider_session_history_in(&root, &top_id, 100).unwrap();
        assert_eq!(history.turns.len(), 2);
        assert_eq!(history.messages.len(), 4);
        assert_eq!(history.messages[0].content, "Explain rebase");
        assert_eq!(history.messages[1].content, "Rebase replays commits.");
        assert_eq!(history.messages[2].content, "Follow up");
        assert_eq!(history.messages[3].content, "Second answer.");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn forks_session_at_requested_turn_and_prunes_trailing_steps_and_metadata() {
        let root = std::env::temp_dir().join(format!("waku-agy-fork-test-{}", Uuid::new_v4()));
        let conv_dir = root.join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();

        let source_id = Uuid::new_v4().to_string();
        let source_traj_id = Uuid::new_v4().to_string();
        let conn = Connection::open(conv_dir.join(format!("{source_id}.db"))).unwrap();
        conn.execute_batch(
            "CREATE TABLE trajectory_meta (
                trajectory_id TEXT PRIMARY KEY,
                cascade_id TEXT,
                trajectory_type INTEGER,
                source INTEGER
            );
            CREATE TABLE trajectory_metadata_blob (
                id TEXT DEFAULT 'main' PRIMARY KEY,
                data BLOB
            );
            CREATE TABLE steps (
                idx INTEGER PRIMARY KEY,
                step_type INTEGER NOT NULL,
                step_payload BLOB
            );
            CREATE TABLE gen_metadata (
                idx INTEGER PRIMARY KEY,
                data BLOB,
                size INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE executor_metadata (
                idx INTEGER PRIMARY KEY,
                data BLOB
            );
            CREATE TABLE battle_mode_infos (
                idx INTEGER PRIMARY KEY,
                data BLOB
            );",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO trajectory_meta VALUES (?1, ?2, 4, 17)",
            [&source_traj_id, &source_id],
        )
        .unwrap();
        let mut meta_blob = pb_bytes_field(3, source_traj_id.as_bytes());
        meta_blob.extend(pb_bytes_field(6, source_id.as_bytes()));
        meta_blob.extend(pb_bytes_field(7, b"file:///tmp/demo"));
        conn.execute(
            "INSERT INTO trajectory_metadata_blob VALUES ('main', ?1)",
            [&meta_blob],
        )
        .unwrap();

        // Turn 1: idx 0, 1
        // Compaction: idx 2
        // Turn 2: idx 3, 4
        // Turn 3: idx 5, 6
        for (idx, step_type, payload) in [
            (0_i64, STEP_TYPE_USER_INPUT, make_user_step("Turn 1", 1_700_000_010)),
            (1, STEP_TYPE_AGENT_RESPONSE, make_assistant_step("Reply 1", 1_700_000_015)),
            (2, STEP_TYPE_USER_INPUT, make_user_step("/compact", 1_700_000_020)),
            (3, STEP_TYPE_USER_INPUT, make_user_step("Turn 2", 1_700_000_030)),
            (4, STEP_TYPE_AGENT_RESPONSE, make_assistant_step("Reply 2", 1_700_000_035)),
            (5, STEP_TYPE_USER_INPUT, make_user_step("Turn 3", 1_700_000_040)),
            (6, STEP_TYPE_AGENT_RESPONSE, make_assistant_step("Reply 3", 1_700_000_045)),
        ] {
            conn.execute(
                "INSERT INTO steps (idx, step_type, step_payload) VALUES (?1, ?2, ?3)",
                rusqlite::params![idx, step_type, payload],
            )
            .unwrap();
        }
        // gen_metadata rows referencing step indices [1], [4], [6]
        for (idx, step_idx) in [(0_i64, 1_u8), (1, 4), (2, 6)] {
            let gen_blob = pb_bytes_field(2, &[step_idx]);
            conn.execute(
                "INSERT INTO gen_metadata VALUES (?1, ?2, 10)",
                rusqlite::params![idx, gen_blob],
            )
            .unwrap();
        }
        // executor_metadata rows referencing step_index 1 and 5
        for (idx, step_idx) in [(0_i64, 1_u64), (1, 5)] {
            let exec_blob = pb_varint_field(3, step_idx);
            conn.execute(
                "INSERT INTO executor_metadata VALUES (?1, ?2)",
                rusqlite::params![idx, exec_blob],
            )
            .unwrap();
        }
        // Completed battles wholly before, across, and after each turn cutoff.
        // Field 6 is an end timestamp, not a step index.
        for (idx, start_idx, end_idx) in [
            (0_i64, 0_u64, 1_u64),
            (1, 2, 4),
            (2, 1, 3),
            (3, 4, 5),
            (4, 5, 6),
            (5, 1, 6),
        ] {
            let mut battle_blob = Vec::new();
            if start_idx != 0 {
                battle_blob.extend(pb_varint_field(3, start_idx));
            }
            battle_blob.extend(pb_varint_field(4, end_idx));
            battle_blob.extend(pb_bytes_field(6, &pb_varint_field(1, 1_700_000_045)));
            conn.execute(
                "INSERT INTO battle_mode_infos VALUES (?1, ?2)",
                rusqlite::params![idx, battle_blob],
            )
            .unwrap();
        }
        // A battle starting in discarded history has no end index yet.
        conn.execute(
            "INSERT INTO battle_mode_infos VALUES (6, ?1)",
            [pb_varint_field(3, 5)],
        )
        .unwrap();
        drop(conn);

        let summary_conn = Connection::open(root.join("conversation_summaries.db")).unwrap();
        summary_conn
            .execute_batch(
                "CREATE TABLE conversation_summaries (
                    conversation_id TEXT PRIMARY KEY,
                    title TEXT NOT NULL DEFAULT '',
                    preview TEXT NOT NULL DEFAULT '',
                    step_count INTEGER NOT NULL DEFAULT 0,
                    last_modified_time TEXT NOT NULL,
                    last_user_input_time TEXT NOT NULL,
                    workspace_uris TEXT NOT NULL,
                    parent_conversation_id TEXT NOT NULL DEFAULT '',
                    nesting_depth INTEGER NOT NULL DEFAULT 0,
                    killed NUMERIC NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        summary_conn
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, 'Original Task', 'Turn 1', 7, '2026-11-01T10:00:45Z', '2026-11-01T10:00:10Z', '[\"file:///tmp/demo\"]', '', 0, 0)",
                [&source_id],
            )
            .unwrap();
        drop(summary_conn);

        // Fork after turn 1 (should stop before idx 2 `/compact`)
        let fork1_id = fork_session_at_turn_in(&root, &source_id, 1, "Forked Turn 1").unwrap();
        assert_ne!(fork1_id, source_id);
        let fork1_history = provider_session_history_in(&root, &fork1_id, 100).unwrap();
        assert_eq!(fork1_history.turns.len(), 1);
        assert_eq!(fork1_history.messages.len(), 2);
        assert_eq!(fork1_history.messages[0].content, "Turn 1");
        assert_eq!(fork1_history.messages[1].content, "Reply 1");
        assert_eq!(
            session_title_in(&root, &fork1_id).as_deref(),
            Some("Forked Turn 1")
        );

        let fork1_conn = open_readonly(&conv_dir.join(format!("{fork1_id}.db"))).unwrap();
        let (traj_id, cascade_id): (String, String) = fork1_conn
            .query_row(
                "SELECT trajectory_id, cascade_id FROM trajectory_meta",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_ne!(traj_id, source_traj_id);
        assert_eq!(cascade_id, fork1_id);
        let blob: Vec<u8> = fork1_conn
            .query_row(
                "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pb_find_non_empty_string(&blob, 3).as_deref(), Some(traj_id.as_str()));
        assert_eq!(pb_find_non_empty_string(&blob, 6).as_deref(), Some(fork1_id.as_str()));
        let step_count: i64 = fork1_conn
            .query_row("SELECT COUNT(*) FROM steps", [], |r| r.get(0))
            .unwrap();
        assert_eq!(step_count, 2);
        let gen_count: i64 = fork1_conn
            .query_row("SELECT COUNT(*) FROM gen_metadata", [], |r| r.get(0))
            .unwrap();
        assert_eq!(gen_count, 1);
        let exec_count: i64 = fork1_conn
            .query_row("SELECT COUNT(*) FROM executor_metadata", [], |r| r.get(0))
            .unwrap();
        assert_eq!(exec_count, 1);
        let battle_indices = fork1_conn
            .prepare("SELECT idx FROM battle_mode_infos ORDER BY idx")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(battle_indices, vec![0]);
        drop(fork1_conn);

        // Fork after turn 2 (should retain `/compact` at idx 2 and Turn 2 at idx 3..=4, pruning Turn 3 at idx 5..=6)
        let fork2_id = fork_session_at_turn_in(&root, &source_id, 2, "Forked Turn 2").unwrap();
        let fork2_history = provider_session_history_in(&root, &fork2_id, 100).unwrap();
        assert_eq!(fork2_history.turns.len(), 2);
        assert_eq!(fork2_history.messages.len(), 4);
        assert_eq!(fork2_history.messages[2].content, "Turn 2");
        assert_eq!(fork2_history.messages[3].content, "Reply 2");
        let fork2_conn = open_readonly(&conv_dir.join(format!("{fork2_id}.db"))).unwrap();
        let battle_indices = fork2_conn
            .prepare("SELECT idx FROM battle_mode_infos ORDER BY idx")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(battle_indices, vec![0, 1, 2]);
        drop(fork2_conn);

        assert!(fork_session_at_turn_in(&root, &source_id, 0, "Invalid").is_err());
        assert!(fork_session_at_turn_in(&root, &source_id, 4, "Invalid").is_err());

        let _ = std::fs::remove_dir_all(root);
    }
}
