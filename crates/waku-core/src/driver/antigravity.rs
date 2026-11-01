//! Google Antigravity CLI (`agy`) streaming-input session driver.
//!
//! `agy --input-format stream-json --output-format stream-json` holds one
//! conversation open across turns: each user prompt is written as a single
//! NDJSON `{"event":"user","message":{"content":...}}` line on `stdin`, and
//! `stdout` answers with one session-wide `init` frame followed by per-turn
//! `step_update` frames and a terminal `result` frame. Closing `stdin` ends the
//! session cleanly with exit status `0`.
//!
//! Verified against live `agy` 1.4.x:
//! - Headless `stream-json` has no interactive stdio approval channel and
//!   rejects `control_request`/`control_response` frames with exit status `2`,
//!   so Waku maps [`RuntimeMode`] at launch (`--dangerously-skip-permissions`,
//!   `--mode auto`, `--mode accept-edits`, or default `request-review`) and
//!   restarts on option changes via `--conversation <id>`.
//! - `SIGINT` interrupts an active turn (`finished_reason: "interrupted"`),
//!   flushes the trajectory to SQLite, and exits; Waku drops the runtime on
//!   Stop and resumes `--conversation <id>` on the next prompt.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use crossbeam_channel::{Sender, unbounded};
use parking_lot::Mutex;
use serde_json::{Value, json};
use waku_protocol::compaction::{
    CompactionRequest, CompactionStatus, CompactionTrigger, ContextCompaction,
};

use super::{activity, support, title_refresh::NativeTitleRefresh};
use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{
    ActivityKind, DriverEvent, ExecutionPolicy, ProviderResumeCursor, RuntimeMode,
    unix_time_millis,
};

enum CommandMessage {
    Prompt(String),
    Compact(CompactionRequest),
    Shutdown,
}

fn user_message_payload(text: &str) -> Value {
    json!({
        "event": "user",
        "message": {
            "content": text,
        }
    })
}

fn write_line(writer: &mut impl Write, payload: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, payload)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn configure_mode_args(command: &mut Command, mode: RuntimeMode) {
    match mode {
        RuntimeMode::FullAccess => {
            command.arg("--dangerously-skip-permissions");
        }
        RuntimeMode::Auto => {
            command.args(["--mode", "auto"]);
        }
        RuntimeMode::AutoAcceptEdits => {
            command.args(["--mode", "accept-edits"]);
        }
        RuntimeMode::Ask => {}
    }
}

fn infer_context_window(model: Option<&str>) -> Option<u64> {
    let lower = model?.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    if lower.contains("gemini-") {
        Some(1_048_576)
    } else if lower.contains("claude-") {
        Some(200_000)
    } else if lower.contains("gpt-oss-") {
        Some(131_072)
    } else {
        None
    }
}

/// Context occupancy of a single `step_update` model invocation.
///
/// Unlike `result.usage`, which sums tokens cumulatively across every step and
/// turn of the session, an `agent_response` step's `usage` reports that single
/// model call: uncached `input_tokens` + `cache_read_tokens` + `output_tokens`.
fn step_context_tokens(usage: &Value) -> Option<u64> {
    let input = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cached = usage
        .get("cache_read_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let occupancy = input.saturating_add(cached).saturating_add(output);
    (occupancy > 0).then_some(occupancy)
}

fn start_antigravity_title_refresh(
    title_refresh: &NativeTitleRefresh,
    session_id: &str,
    events: &DriverEventSender,
) {
    let session_id = session_id.to_owned();
    title_refresh.start(
        "waku-agy-title",
        vec![Duration::from_secs(5), Duration::from_secs(10)],
        events.clone(),
        move || Ok(crate::antigravity_session::session_title(&session_id)),
    );
}

struct StreamState {
    cwd: PathBuf,
    session_id: Arc<Mutex<Option<String>>>,
    context_window: Option<u64>,
    streamed_text_this_turn: bool,
    last_auto_title: Option<String>,
    reported_error: Arc<AtomicBool>,
}

impl StreamState {
    fn new(
        cwd: PathBuf,
        session_id: Arc<Mutex<Option<String>>>,
        initial_model: Option<&str>,
        reported_error: Arc<AtomicBool>,
    ) -> Self {
        Self {
            cwd,
            session_id,
            context_window: infer_context_window(initial_model),
            streamed_text_this_turn: false,
            last_auto_title: None,
            reported_error,
        }
    }
}

pub struct AntigravityDriver {
    commands: Sender<CommandMessage>,
    current_options: SessionOptions,
    child_pid: u32,
    cancelled: Arc<AtomicBool>,
}

impl AntigravityDriver {
    pub fn start(options: DriverStartOptions, events: DriverEventSender) -> anyhow::Result<Self> {
        let DriverStartOptions {
            binary,
            cwd,
            mode,
            execution_policy,
            model,
            reasoning_effort,
            service_tier,
            context_window,
            agent_preset,
            computer_use_enabled: _,
            provider_cursor,
            delegation_session_path: _,
            delegation_registry_dir: _,
        } = options;

        if execution_policy == ExecutionPolicy::ReviewReadOnly {
            return Err(anyhow!(
                "Antigravity CLI cannot guarantee read-only review subtasks"
            ));
        }

        let resume_session_id = match provider_cursor {
            Some(ProviderResumeCursor::Antigravity { session_id })
                if !session_id.trim().is_empty() =>
            {
                Some(session_id)
            }
            Some(ProviderResumeCursor::Antigravity { .. }) | None => None,
            Some(cursor) => {
                return Err(anyhow!(
                    "cannot resume Antigravity CLI from a {} cursor",
                    cursor.provider().display_name()
                ));
            }
        };

        let current_options = SessionOptions {
            mode,
            model: model.clone(),
            reasoning_effort: reasoning_effort.clone(),
            service_tier,
            context_window,
        };

        let (wire_model, wire_effort) =
            crate::model_catalog::resolve_antigravity_model_and_effort(
                model.as_deref(),
                reasoning_effort.as_deref(),
            );

        let mut command = crate::command_env::command(&binary);
        command.args([
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--print-timeout",
            "0",
        ]);
        configure_mode_args(&mut command, mode);
        if let Some(model_slug) = &wire_model {
            command.args(["--model", model_slug]);
        }
        if let Some(effort) = &wire_effort {
            command.args(["--effort", effort]);
        }
        if let Some(preset) = agent_preset.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            command.args(["--agent", preset]);
        }
        if let Some(session_id) = &resume_session_id {
            command.args(["--conversation", session_id]);
        }
        command
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }

        let mut child = crate::command_env::spawn(&mut command)
            .with_context(|| format!("could not launch Antigravity CLI at {}", binary.display()))?;
        let child_pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("could not open Antigravity CLI stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("could not open Antigravity CLI stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("could not open Antigravity CLI stderr"))?;

        let session_id_slot = Arc::new(Mutex::new(resume_session_id.clone()));
        if let Some(session_id) = resume_session_id {
            let _ = events.send(DriverEvent::Connected {
                provider_cursor: Some(ProviderResumeCursor::Antigravity { session_id }),
            });
        }

        let turn_active = Arc::new(Mutex::new(false));
        let active_compaction: Arc<Mutex<Option<ContextCompaction>>> = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(AtomicBool::new(false));
        let reported_error = Arc::new(AtomicBool::new(false));
        let title_refresh = NativeTitleRefresh::default();
        let (command_tx, command_rx) = unbounded::<CommandMessage>();

        // Writer thread: owns ChildStdin; dropping it delivers EOF to `agy`.
        {
            let writer_events = events.clone();
            let writer_turn = turn_active.clone();
            let writer_compaction = active_compaction.clone();
            let writer_cancelled = cancelled.clone();
            let writer_session_id = session_id_slot.clone();
            let writer_title_refresh = title_refresh.clone();
            thread::Builder::new()
                .name("waku-agy-writer".into())
                .spawn(move || {
                    let mut stdin = stdin;
                    while let Ok(message) = command_rx.recv() {
                        let written = match message {
                            CommandMessage::Prompt(text) => {
                                writer_cancelled.store(false, Ordering::Release);
                                *writer_turn.lock() = true;
                                if let Some(session_id) = writer_session_id.lock().as_deref() {
                                    start_antigravity_title_refresh(
                                        &writer_title_refresh,
                                        session_id,
                                        &writer_events,
                                    );
                                }
                                let _ = writer_events.send(DriverEvent::TurnStarted);
                                write_line(&mut stdin, &user_message_payload(&text))
                            }
                            CommandMessage::Compact(request) => {
                                if request
                                    .instructions
                                    .as_deref()
                                    .is_some_and(|text| !text.trim().is_empty())
                                {
                                    let mut item = ContextCompaction::new(
                                        request.id,
                                        Some(CompactionTrigger::Manual),
                                        unix_time_millis(),
                                    );
                                    item.error = Some(
                                        "Antigravity CLI /compact does not accept focus instructions"
                                            .to_owned(),
                                    );
                                    item.finish(CompactionStatus::Failed, unix_time_millis());
                                    let _ =
                                        writer_events.send(DriverEvent::CompactionUpdated(item));
                                    continue;
                                }
                                writer_cancelled.store(false, Ordering::Release);
                                let item = ContextCompaction::new(
                                    request.id,
                                    Some(CompactionTrigger::Manual),
                                    unix_time_millis(),
                                );
                                *writer_compaction.lock() = Some(item.clone());
                                let _ = writer_events.send(DriverEvent::CompactionUpdated(item));
                                write_line(&mut stdin, &user_message_payload("/compact"))
                            }
                            CommandMessage::Shutdown => break,
                        };
                        if let Err(error) = written {
                            let _ = writer_events.send(DriverEvent::Error(tr!(
                                "errors.provider_transport_write",
                                provider = "Antigravity",
                                error = error
                            )));
                            if let Some(mut item) = writer_compaction.lock().take() {
                                item.error = Some(error.to_string());
                                item.finish(CompactionStatus::Failed, unix_time_millis());
                                let _ = writer_events.send(DriverEvent::CompactionUpdated(item));
                            }
                            if std::mem::take(&mut *writer_turn.lock()) {
                                let _ = writer_events.send(DriverEvent::TurnFinished {
                                    success: false,
                                    summary: Some(tr!(
                                        "errors.provider_receive_prompt",
                                        provider = "Antigravity"
                                    )),
                                });
                            }
                            break;
                        }
                    }
                })?;
        }

        // Stderr collector thread.
        let stderr_handle = thread::Builder::new()
            .name("waku-agy-stderr".into())
            .spawn(move || {
                let mut lines = Vec::new();
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if lines.len() >= 128 {
                        lines.remove(0);
                    }
                    lines.push(line);
                }
                lines
            })?;

        // Reader thread: parses NDJSON events from ChildStdout.
        let reader_handle = {
            let reader_events = events.clone();
            let reader_turn = turn_active.clone();
            let reader_compaction = active_compaction.clone();
            let reader_cancelled = cancelled.clone();
            let reader_reported_error = reported_error.clone();
            let reader_session_id = session_id_slot.clone();
            let reader_title_refresh = title_refresh;
            let initial_model = wire_model;
            thread::Builder::new()
                .name("waku-agy-reader".into())
                .spawn(move || {
                    let mut state = StreamState::new(
                        cwd,
                        reader_session_id,
                        initial_model.as_deref(),
                        reader_reported_error,
                    );
                    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                            continue;
                        };
                        handle_stream_event(
                            &value,
                            &mut state,
                            &reader_turn,
                            &reader_compaction,
                            &reader_cancelled,
                            Some(&reader_title_refresh),
                            Some(&reader_events),
                            &reader_events,
                        );
                    }
                })?
        };

        // Process reaper thread: blocks on `wait()` so no zombie outlives the runtime.
        let wait_cancelled = cancelled.clone();
        thread::Builder::new()
            .name("waku-agy-wait".into())
            .spawn(move || {
                let status = child.wait().ok();
                let _ = reader_handle.join();
                let stderr_lines = stderr_handle.join().unwrap_or_default();
                let was_cancelled = wait_cancelled.load(Ordering::Acquire);
                let stderr_error = support::provider_stderr_error(stderr_lines);

                if let Some(mut item) = active_compaction.lock().take() {
                    if was_cancelled {
                        item.finish(CompactionStatus::Interrupted, unix_time_millis());
                    } else {
                        item.error = stderr_error.clone().or_else(|| {
                            Some("Antigravity CLI exited during compaction".to_owned())
                        });
                        item.finish(CompactionStatus::Failed, unix_time_millis());
                    }
                    let _ = events.send(DriverEvent::CompactionUpdated(item));
                }

                if std::mem::take(&mut *turn_active.lock()) {
                    if !was_cancelled
                        && !reported_error.load(Ordering::Acquire)
                        && let Some(err) = &stderr_error
                    {
                        reported_error.store(true, Ordering::Release);
                        let _ = events.send(DriverEvent::Error(err.clone()));
                    }
                    let _ = events.send(DriverEvent::TurnFinished {
                        success: false,
                        summary: None,
                    });
                } else if status.is_some_and(|s| !s.success())
                    && !was_cancelled
                    && !reported_error.load(Ordering::Acquire)
                    && let Some(err) = stderr_error
                {
                    let _ = events.send(DriverEvent::Error(err));
                }

                let _ = events.send(DriverEvent::ProcessExited);
            })?;

        Ok(Self {
            commands: command_tx,
            current_options,
            child_pid,
            cancelled,
        })
    }
}

impl Drop for AntigravityDriver {
    fn drop(&mut self) {
        let _ = self.commands.send(CommandMessage::Shutdown);
    }
}

impl DriverControl for AntigravityDriver {
    fn compact(&self, request: CompactionRequest) {
        let _ = self.commands.send(CommandMessage::Compact(request));
    }

    fn prompt(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Prompt(prompt));
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        #[cfg(unix)]
        if self.child_pid > 0 {
            unsafe {
                if libc::kill(-(self.child_pid as i32), libc::SIGINT) != 0 {
                    libc::kill(self.child_pid as i32, libc::SIGINT);
                }
            }
        }
        let _ = self.commands.send(CommandMessage::Shutdown);
    }

    fn respond(&self, _request_id: String, _option_id: String) {}

    fn apply_options(&self, options: SessionOptions) -> bool {
        options == self.current_options
    }

    fn rollback(&self, _turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        anyhow::bail!("conversation rollback is not supported by Antigravity CLI")
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_stream_event(
    value: &Value,
    state: &mut StreamState,
    turn_active: &Mutex<bool>,
    active_compaction: &Mutex<Option<ContextCompaction>>,
    cancelled: &AtomicBool,
    title_refresh: Option<&NativeTitleRefresh>,
    title_sender: Option<&DriverEventSender>,
    events: &impl DriverEventSink,
) {
    match value.get("event").and_then(Value::as_str) {
        Some("init") => {
            if let Some(conv_id) = value
                .get("conversation_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                let changed = {
                    let mut guard = state.session_id.lock();
                    let changed = guard.as_deref() != Some(conv_id);
                    if changed {
                        *guard = Some(conv_id.to_owned());
                    }
                    changed
                };
                if changed {
                    let _ = events.send(DriverEvent::Connected {
                        provider_cursor: Some(ProviderResumeCursor::Antigravity {
                            session_id: conv_id.to_owned(),
                        }),
                    });
                }
                if *turn_active.lock()
                    && let (Some(refresh), Some(sender)) = (title_refresh, title_sender)
                {
                    start_antigravity_title_refresh(refresh, conv_id, sender);
                }
            }

            if let Some(model) = value.pointer("/init/model").and_then(Value::as_str)
                && let Some(window) = infer_context_window(Some(model))
            {
                state.context_window = Some(window);
                let _ = events.send(DriverEvent::UsageUpdated {
                    context_tokens: None,
                    context_window: Some(window),
                });
            }
        }
        Some("step_update") => {
            let Some(step) = value.get("step_update") else {
                return;
            };
            let step_type = step
                .get("step_type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match step_type {
                "agent_response" => {
                    if let Some(delta) = step.get("text_delta").and_then(Value::as_str)
                        && !delta.is_empty()
                    {
                        state.streamed_text_this_turn = true;
                        let _ = events.send(DriverEvent::TextDelta(delta.to_owned()));
                    }
                    if let Some(usage) = step.get("usage")
                        && let Some(tokens) = step_context_tokens(usage)
                    {
                        let _ = events.send(DriverEvent::UsageUpdated {
                            context_tokens: Some(tokens),
                            context_window: state.context_window,
                        });
                    }
                }
                "tool" => {
                    let item = build_tool_activity(step, &state.cwd);
                    let _ = events.send(DriverEvent::RichActivity(item));
                }
                _ => {
                    if let Some(subagent_info) =
                        step.get("subagent_info").filter(|val| !val.is_null())
                    {
                        let item = build_subagent_activity(step, subagent_info);
                        let _ = events.send(DriverEvent::RichActivity(item));
                    }
                }
            }
        }
        Some("result") => {
            let Some(result) = value.get("result") else {
                return;
            };
            if let Some(conv_id) = result
                .get("conversation_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                let mut guard = state.session_id.lock();
                if guard.as_deref() != Some(conv_id) {
                    *guard = Some(conv_id.to_owned());
                    let _ = events.send(DriverEvent::Connected {
                        provider_cursor: Some(ProviderResumeCursor::Antigravity {
                            session_id: conv_id.to_owned(),
                        }),
                    });
                }
            }

            let status = result
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("ERROR");
            let finished_reason = result.get("finished_reason").and_then(Value::as_str);
            let error_text = result
                .get("error")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|msg| !msg.is_empty());
            let interrupted = cancelled.load(Ordering::Acquire)
                || matches!(status, "INTERRUPTED" | "CANCELED")
                || finished_reason == Some("interrupted")
                || error_text == Some("interrupted");
            let success = status == "SUCCESS" && !interrupted;

            if let Some(mut compaction) = active_compaction.lock().take() {
                if success {
                    compaction.finish(CompactionStatus::Completed, unix_time_millis());
                } else if interrupted {
                    compaction.finish(CompactionStatus::Interrupted, unix_time_millis());
                } else {
                    compaction.error = error_text.map(str::to_owned);
                    compaction.finish(CompactionStatus::Failed, unix_time_millis());
                }
                let _ = events.send(DriverEvent::CompactionUpdated(compaction));
                state.streamed_text_this_turn = false;
                return;
            }

            if !*turn_active.lock() {
                if !success
                    && !interrupted
                    && let Some(err) = error_text
                {
                    state.reported_error.store(true, Ordering::Release);
                    let _ = events.send(DriverEvent::Error(err.to_owned()));
                }
                return;
            }

            if !state.streamed_text_this_turn
                && let Some(response) = result
                    .get("response")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            {
                let _ = events.send(DriverEvent::TextDelta(response.to_owned()));
            }
            state.streamed_text_this_turn = false;

            if let Some(session_id) = state.session_id.lock().clone()
                && let Some(title) = crate::antigravity_session::session_title(&session_id)
                && state.last_auto_title.as_deref() != Some(title.as_str())
            {
                state.last_auto_title = Some(title.clone());
                let _ = events.send(DriverEvent::AutoTitleUpdated(Some(title)));
            }

            if !success
                && !interrupted
                && let Some(err) = error_text
            {
                state.reported_error.store(true, Ordering::Release);
                let _ = events.send(DriverEvent::Error(err.to_owned()));
            }

            *turn_active.lock() = false;
            let _ = events.send(DriverEvent::TurnFinished {
                success,
                summary: None,
            });
        }
        _ => {}
    }
}

fn build_tool_activity(step: &Value, cwd: &Path) -> crate::model::ActivityItem {
    let step_index = step.get("step_index").and_then(Value::as_u64);
    let source_id = step_index.map(|idx| format!("agy-step-{idx}"));
    let state_str = step
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("ACTIVE");
    let complete = state_str == "DONE";

    let tool_info = step.get("tool_info");
    let tool_name = step
        .get("tool_name")
        .and_then(Value::as_str)
        .or_else(|| tool_info.and_then(|info| info.get("name")).and_then(Value::as_str))
        .unwrap_or("tool");
    let kind = support::classify_tool(tool_name);
    let parameters = tool_info
        .and_then(|info| info.get("parameters"))
        .filter(|val| !val.is_null());
    let error_obj = tool_info
        .and_then(|info| info.get("error"))
        .filter(|val| !val.is_null());
    let failed = error_obj.is_some();

    let output_value = if let Some(err) = error_obj {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .filter(|msg| !msg.trim().is_empty())
            .map(|msg| Value::String(msg.to_owned()))
            .unwrap_or_else(|| err.clone());
        Some(message)
    } else {
        tool_info
            .and_then(|info| info.get("output"))
            .filter(|val| !val.is_null())
            .cloned()
    };

    let title = tool_activity_title(tool_name, kind, parameters, cwd);
    activity::tool_activity(
        source_id,
        kind,
        title,
        parameters,
        output_value.as_ref(),
        None,
        failed,
        complete,
    )
}

fn build_subagent_activity(
    step: &Value,
    subagent_info: &Value,
) -> crate::model::ActivityItem {
    let step_index = step.get("step_index").and_then(Value::as_u64);
    let source_id = step_index.map(|idx| format!("agy-step-{idx}"));
    let complete = step.get("state").and_then(Value::as_str) == Some("DONE");
    let subagents = subagent_info
        .get("subagents")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let label = subagents
        .first()
        .and_then(|sub| {
            sub.get("role")
                .or_else(|| sub.get("type_name"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|role| {
            if subagents.len() > 1 {
                format!("Subagents: {role} (+{})", subagents.len() - 1)
            } else {
                format!("Subagent: {role}")
            }
        })
        .unwrap_or_else(|| "Subagent".to_owned());

    activity::tool_activity(
        source_id,
        ActivityKind::Tool,
        label,
        Some(subagent_info),
        None,
        None,
        false,
        complete,
    )
}

fn tool_activity_title(
    tool_name: &str,
    kind: ActivityKind,
    parameters: Option<&Value>,
    cwd: &Path,
) -> String {
    if let Some(title) = activity::input_title(parameters) {
        return title;
    }
    if let Some(summary) = parameters
        .and_then(|p| p.get("toolSummary"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return summary.to_owned();
    }
    match kind {
        ActivityKind::Command => parameters
            .and_then(|p| p.get("CommandLine").or_else(|| p.get("command")))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| tr!("activity.run_command")),
        ActivityKind::FileRead | ActivityKind::FileChange | ActivityKind::FileList => parameters
            .and_then(|p| {
                p.get("Path")
                    .or_else(|| p.get("TargetFile"))
                    .or_else(|| p.get("DirectoryPath"))
                    .or_else(|| p.get("SearchDirectory"))
                    .or_else(|| p.get("path"))
            })
            .and_then(Value::as_str)
            .map(|raw| display_workspace_path(raw, cwd))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| humanize_tool_name(tool_name)),
        ActivityKind::FileSearch | ActivityKind::Search => parameters
            .and_then(|p| {
                p.get("Query")
                    .or_else(|| p.get("Pattern"))
                    .or_else(|| p.get("Url"))
                    .or_else(|| p.get("query"))
                    .or_else(|| p.get("pattern"))
            })
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| humanize_tool_name(tool_name)),
        ActivityKind::Plan => tr!("activity.plan_updated"),
        _ => humanize_tool_name(tool_name),
    }
}

fn display_workspace_path(raw_path: &str, cwd: &Path) -> String {
    let trimmed = raw_path.trim();
    let path = Path::new(trimmed);
    if let Ok(rel) = path.strip_prefix(cwd) {
        let rel_str = rel.to_string_lossy();
        if rel_str.is_empty() {
            return ".".to_owned();
        }
        return rel_str.into_owned();
    }
    trimmed.to_owned()
}

fn humanize_tool_name(name: &str) -> String {
    let words: Vec<String> = name
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
        .enumerate()
        .map(|(idx, word)| {
            if idx == 0 {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            } else {
                word.to_owned()
            }
        })
        .collect();
    if words.is_empty() {
        "Tool".to_owned()
    } else {
        words.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::test_event_channel;

    #[test]
    fn normalizes_live_antigravity_stream_events_and_compaction() {
        let (tx, rx) = test_event_channel();
        let session_id = Arc::new(Mutex::new(None));
        let reported_error = Arc::new(AtomicBool::new(false));
        let mut state = StreamState::new(
            PathBuf::from("/work/demo"),
            session_id,
            Some("gemini-4-argon"),
            reported_error,
        );
        let turn_active = Mutex::new(true);
        let active_compaction = Mutex::new(None);
        let cancelled = AtomicBool::new(false);

        handle_stream_event(
            &json!({
                "event": "init",
                "conversation_id": "21edd5d2-c99a-4b77-9ff4-6205dceec207",
                "init": {"model": "gemini-4-argon-high", "cwd": "/work/demo"}
            }),
            &mut state,
            &turn_active,
            &active_compaction,
            &cancelled,
            None,
            None,
            &tx,
        );

        handle_stream_event(
            &json!({
                "event": "step_update",
                "step_update": {
                    "step_index": 2,
                    "state": "DONE",
                    "step_type": "tool",
                    "tool_name": "edit_file",
                    "tool_info": {
                        "name": "edit_file",
                        "parameters": {
                            "Path": "/work/demo/src/lib.rs",
                            "Target": "let old = 1;",
                            "Replacement": "let updated = 2;"
                        }
                    }
                }
            }),
            &mut state,
            &turn_active,
            &active_compaction,
            &cancelled,
            None,
            None,
            &tx,
        );

        handle_stream_event(
            &json!({
                "event": "step_update",
                "step_update": {
                    "step_index": 3,
                    "state": "DONE",
                    "step_type": "tool",
                    "tool_name": "run_command",
                    "tool_info": {
                        "name": "run_command",
                        "parameters": {
                            "CommandLine": "cargo check",
                            "toolSummary": "Verify workspace build"
                        },
                        "output": "Command exited with code 0.\n\nCommand output:\nFinished\n"
                    }
                }
            }),
            &mut state,
            &turn_active,
            &active_compaction,
            &cancelled,
            None,
            None,
            &tx,
        );

        handle_stream_event(
            &json!({
                "event": "step_update",
                "step_update": {
                    "step_index": 4,
                    "state": "ACTIVE",
                    "step_type": "agent_response",
                    "text_delta": "Done"
                }
            }),
            &mut state,
            &turn_active,
            &active_compaction,
            &cancelled,
            None,
            None,
            &tx,
        );

        handle_stream_event(
            &json!({
                "event": "step_update",
                "step_update": {
                    "step_index": 4,
                    "state": "DONE",
                    "step_type": "agent_response",
                    "text_delta": "\n",
                    "usage": {
                        "input_tokens": 300,
                        "cache_read_tokens": 7000,
                        "output_tokens": 50,
                        "total_tokens": 350
                    }
                }
            }),
            &mut state,
            &turn_active,
            &active_compaction,
            &cancelled,
            None,
            None,
            &tx,
        );

        handle_stream_event(
            &json!({
                "event": "result",
                "result": {
                    "conversation_id": "21edd5d2-c99a-4b77-9ff4-6205dceec207",
                    "status": "SUCCESS",
                    "response": "Done\n",
                    "num_turns": 1
                }
            }),
            &mut state,
            &turn_active,
            &active_compaction,
            &cancelled,
            None,
            None,
            &tx,
        );

        let collected: Vec<DriverEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(collected.iter().any(|e| matches!(
            e,
            DriverEvent::Connected {
                provider_cursor: Some(ProviderResumeCursor::Antigravity { session_id })
            } if session_id == "21edd5d2-c99a-4b77-9ff4-6205dceec207"
        )));
        let edit_activity = collected
            .iter()
            .find_map(|e| match e {
                DriverEvent::RichActivity(item) if item.kind == ActivityKind::FileChange => {
                    Some(item)
                }
                _ => None,
            })
            .expect("edit_file RichActivity");
        assert_eq!(edit_activity.title, "src/lib.rs");
        assert_eq!(edit_activity.file_changes.len(), 1);
        assert!(
            edit_activity.file_changes[0]
                .diff
                .as_deref()
                .is_some_and(|d| d.contains("-let old = 1;") && d.contains("+let updated = 2;"))
        );

        let cmd_activity = collected
            .iter()
            .find_map(|e| match e {
                DriverEvent::RichActivity(item) if item.kind == ActivityKind::Command => {
                    Some(item)
                }
                _ => None,
            })
            .expect("run_command RichActivity");
        assert_eq!(cmd_activity.arguments.as_deref(), Some("cargo check"));
        assert_eq!(
            cmd_activity.display_description.as_deref(),
            Some("Verify workspace build")
        );
        assert!(!cmd_activity.failed);
        assert!(cmd_activity.complete);

        let text: String = collected
            .iter()
            .filter_map(|e| match e {
                DriverEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Done\n");

        assert!(collected.iter().any(|e| matches!(
            e,
            DriverEvent::UsageUpdated {
                context_tokens: Some(7350),
                context_window: Some(1_048_576),
            }
        )));
        assert!(collected.iter().any(|e| matches!(
            e,
            DriverEvent::TurnFinished { success: true, .. }
        )));

        // Verify native manual compaction settles CompactionUpdated without TurnFinished.
        *active_compaction.lock() = Some(ContextCompaction::new(
            "cmp-1".to_owned(),
            Some(CompactionTrigger::Manual),
            100,
        ));
        handle_stream_event(
            &json!({
                "event": "result",
                "result": {
                    "conversation_id": "21edd5d2-c99a-4b77-9ff4-6205dceec207",
                    "status": "SUCCESS",
                    "response": ""
                }
            }),
            &mut state,
            &turn_active,
            &active_compaction,
            &cancelled,
            None,
            None,
            &tx,
        );
        let compaction_events: Vec<DriverEvent> =
            std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(compaction_events.iter().any(|e| matches!(
            e,
            DriverEvent::CompactionUpdated(c) if c.status == CompactionStatus::Completed
        )));
        assert!(
            !compaction_events
                .iter()
                .any(|e| matches!(e, DriverEvent::TurnFinished { .. }))
        );
    }

    #[test]
    #[ignore = "requires an installed, authenticated agy"]
    fn antigravity_live_smoke_against_real_cli() {
        let binary = crate::command_env::find_executable("agy").expect("agy is not installed");

        let (models, _) =
            crate::model_catalog::discover_catalog(crate::model::ProviderKind::Antigravity, &binary);
        assert!(!models.is_empty(), "agy catalog discovery returned no models");
        assert_eq!(
            models.iter().filter(|m| m.is_default).count(),
            1,
            "exactly one model should be marked default"
        );

        let plan_usage =
            crate::usage::fetch_antigravity_plan_usage(&binary, Some("gemini-4-argon"))
                .expect("agy /usage should succeed")
                .expect("agy /usage should return plan windows");
        assert!(!plan_usage.windows.is_empty());

        let workdir =
            std::env::temp_dir().join(format!("waku-agy-live-smoke-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workdir).unwrap();
        std::fs::write(workdir.join("note.txt"), "hello_from_waku\n").unwrap();

        let (tx, rx) = test_event_channel();
        let driver = AntigravityDriver::start(
            DriverStartOptions {
                binary: binary.clone(),
                cwd: workdir.clone(),
                mode: RuntimeMode::FullAccess,
                execution_policy: ExecutionPolicy::Interactive,
                model: Some("gemini-3.8-flash".to_owned()),
                reasoning_effort: Some("low".to_owned()),
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
                delegation_session_path: None,
                delegation_registry_dir: None,
            },
            tx,
        )
        .expect("start AntigravityDriver");

        driver.prompt(
            "Read note.txt and remember the codeword WAKU_AGY_77. Reply with OK.".to_owned(),
        );

        let mut session_id = None;
        let mut saw_read_activity = false;
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(60))
                .expect("turn 1 event");
            match event {
                DriverEvent::Connected {
                    provider_cursor: Some(ProviderResumeCursor::Antigravity { session_id: id }),
                } => {
                    session_id = Some(id);
                }
                DriverEvent::RichActivity(item) => {
                    if item.kind == ActivityKind::FileRead {
                        saw_read_activity = true;
                    }
                }
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "turn 1 should finish successfully");
                    break;
                }
                _ => {}
            }
        }
        let session_id = session_id.expect("init should report conversation_id");
        assert!(saw_read_activity, "expected FileRead RichActivity for note.txt");

        // Turn 2 in the same persistent process.
        driver.prompt("What codeword did I ask you to remember? Reply with ONLY the codeword.".to_owned());
        let mut turn2_text = String::new();
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(60))
                .expect("turn 2 event");
            match event {
                DriverEvent::TextDelta(delta) => turn2_text.push_str(&delta),
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "turn 2 should finish successfully");
                    break;
                }
                _ => {}
            }
        }
        assert!(
            turn2_text.contains("WAKU_AGY_77"),
            "turn 2 should retain context, got: {turn2_text:?}"
        );

        // Manual native compaction in the same process.
        driver.compact(CompactionRequest {
            id: "smoke-compact".to_owned(),
            instructions: None,
        });
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(60))
                .expect("compaction event");
            if let DriverEvent::CompactionUpdated(c) = event
                && c.status == CompactionStatus::Completed
            {
                break;
            }
        }

        // Drop driver -> stdin EOF -> ProcessExited.
        drop(driver);
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(15))
                .expect("ProcessExited after drop");
            if matches!(event, DriverEvent::ProcessExited) {
                break;
            }
        }

        // Verify SQLite session listing & cold history import on the real conversation!
        let listed = crate::antigravity_session::list_provider_sessions(20)
            .expect("list_provider_sessions");
        assert!(
            listed.iter().any(|s| s.cursor.native_id() == session_id),
            "newly created agy session should appear in list_provider_sessions"
        );
        let history = crate::antigravity_session::provider_session_history(&session_id, 50)
            .expect("provider_session_history");
        assert_eq!(history.turns.len(), 2, "compaction turn should be excluded");
        assert!(
            history
                .messages
                .iter()
                .any(|m| m.content.contains("WAKU_AGY_77")),
            "imported history should contain turn messages"
        );

        let _ = std::fs::remove_dir_all(workdir);
    }
}
