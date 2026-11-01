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
use chrono::{DateTime, NaiveDateTime};
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
}
