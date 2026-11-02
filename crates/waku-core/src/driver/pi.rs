//! Pi RPC transport.

use super::compaction::CompactionTracker;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use waku_protocol::capabilities::RuntimeCapabilities;
use waku_protocol::compaction::{
    CompactionRequest, CompactionStatus, CompactionTrigger, ContextCompaction,
};

use anyhow::{Context as _, anyhow};
use crossbeam_channel::{Sender, bounded, unbounded};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::activity;
use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{
    ActivityKind, DriverEvent, ExecutionPolicy, ProviderResumeCursor, ReportedCommand,
    unix_time_millis,
};

/// The tool allowlist a review subtask is launched with.
const REVIEW_READ_ONLY_TOOLS: &str = "read,grep,glob,lsp,web_search";

/// The arguments a review subtask is launched with. No `--approve`:
/// that would auto-approve exactly the writes the allowlist removes.
fn review_read_only_args() -> [&'static str; 4] {
    ["--mode", "rpc", "--tools", REVIEW_READ_ONLY_TOOLS]
}

const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// The handshake races the agent's own startup, so it needs more headroom than
/// a request against the already-running process. Pi loads extensions and
/// resources and, when model networking is on, refreshes its model catalog
/// before it reads stdin — it budgets 15 s for that refresh alone — and a
/// timed-out `get_state` there fails the whole session.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);

/// Only a possible standalone period waits for the completed message. Prose
/// streams immediately, and even malformed whitespace streams stay bounded.
const MAX_PENDING_PLACEHOLDER_BYTES: usize = 256;

/// Which dialect of the Pi RPC protocol a session speaks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PiFlavor {
    #[default]
    Pi,
}

impl PiFlavor {
    fn provider(self) -> crate::model::ProviderKind {
        crate::model::ProviderKind::Pi
    }

    fn display_name(self) -> &'static str {
        "Pi"
    }

    /// Pi has no permission system and only needs project-local files trusted;
    /// Waku runs these in Full access via `--approve`.
    fn full_access_arg(self) -> &'static str {
        "--approve"
    }

    /// The event that means the run is over and no more work is scheduled.
    fn settled_event(self) -> &'static str {
        "agent_settled"
    }

    fn session_info_event(self) -> &'static str {
        "session_info_changed"
    }

    fn session_info_title_field(self) -> &'static str {
        "name"
    }

    fn branch_messages_command(self) -> &'static str {
        "get_fork_messages"
    }

    fn branch_command(self) -> &'static str {
        "fork"
    }

    fn skips_version_check_by_env(self) -> bool {
        true
    }

    fn cursor(self, session_id: String, session_file: Option<PathBuf>) -> ProviderResumeCursor {
        ProviderResumeCursor::Pi {
            session_id,
            session_file,
        }
    }

    fn session_file_from_cursor(self, cursor: &ProviderResumeCursor) -> Option<&PathBuf> {
        match cursor {
            ProviderResumeCursor::Pi { session_file, .. } => session_file.as_ref(),
            _ => None,
        }
    }

    fn owns_cursor(self, cursor: &ProviderResumeCursor) -> bool {
        matches!(cursor, ProviderResumeCursor::Pi { .. })
    }
}

enum CommandMessage {
    Compact(CompactionRequest),
    RefreshContextUsage,
    Prompt(String),
    Steer(String),
    Cancel,
    CancelExtensionRequest(String),
    Options(SessionOptions),
    Rollback {
        turns: usize,
        response: Sender<Result<ProviderResumeCursor, String>>,
    },
    Fork {
        turns_to_remove: usize,
        response: Sender<Result<ProviderResumeCursor, String>>,
    },
    Shutdown,
}

enum PendingResponse {
    Compact(ContextCompaction),
    Request(Sender<Result<Value, String>>),
    Prompt,
}

type PendingResponses = Arc<Mutex<HashMap<String, PendingResponse>>>;

fn interrupt_pending_compactions(pending: &PendingResponses, events: &impl DriverEventSink) {
    for entry in pending.lock().values_mut() {
        if let PendingResponse::Compact(item) = entry
            && item.status == CompactionStatus::Running
        {
            item.finish(CompactionStatus::Interrupted, unix_time_millis());
            let _ = events.send(DriverEvent::CompactionUpdated(item.clone()));
        }
    }
}

pub struct PiDriver {
    flavor: PiFlavor,
    commands: Sender<CommandMessage>,
}

/// Give the parent agent Waku's delegation tools.
///
/// Pi cannot inherit an MCP server the way Codex does, so the tools arrive as
/// an extension that spawns `waku-daemon mcp-agent-tools` itself; the session
/// registration file tells that bridge whose turn this is. The extension marks
/// both tools `loadMode: "essential"`, so Oh My Pi keeps their full schemas at
/// top level without needing `--tools`. Without the extension the task still
/// runs, so a missing asset is reported and skipped rather than failing the
/// launch.
fn configure_pi_delegation_command(
    command: &mut std::process::Command,
    flavor: PiFlavor,
    session_path: Option<&Path>,
    extension: impl FnOnce() -> anyhow::Result<PathBuf>,
) {
    let Some(session_path) = session_path else {
        return;
    };
    match extension() {
        Ok(extension) => {
            let daemon = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("waku-daemon"));
            command
                .arg("--extension")
                .arg(extension)
                .env("WAKU_AGENT_TOOLS_SERVER", daemon)
                .env("WAKU_DELEGATION_SESSION_PATH", session_path);
        }
        Err(error) => eprintln!(
            "{} will not be able to delegate: {error:#}",
            flavor.display_name()
        ),
    }
}

impl PiDriver {
    pub fn start(
        options: DriverStartOptions,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
        let flavor = PiFlavor::Pi;
        let DriverStartOptions {
            binary,
            cwd,
            mode,
            execution_policy,
            model,
            reasoning_effort,
            service_tier: _,
            context_window: _,
            agent_preset: _,
            provider_cursor,
            delegation_session_path,
            delegation_registry_dir: _,
        } = options;
        // A review subtask runs unattended, so it never gets the full-access
        // launch: the tool allowlist below is what makes it read-only, and it
        // is enforced by the CLI refusing to expose anything else.
        let review_read_only = execution_policy == ExecutionPolicy::ReviewReadOnly;
        if !review_read_only {
            flavor.provider().validate_access_mode(mode)?;
        }
        let resume_session_file = match provider_cursor {
            Some(cursor) if flavor.owns_cursor(&cursor) => {
                let Some(session_file) = flavor.session_file_from_cursor(&cursor).cloned() else {
                    return Err(anyhow!(
                        "cannot resume {} because its native session file is missing",
                        flavor.display_name()
                    ));
                };
                Some(session_file)
            }
            Some(cursor) => {
                return Err(anyhow!(
                    "cannot resume {} from a {} cursor",
                    flavor.display_name(),
                    cursor.provider().display_name()
                ));
            }
            None => None,
        };
        if let Some(model) = model.as_deref() {
            parse_model_slug(model)?;
        }

        let mut command = crate::command_env::command(&binary);
        if review_read_only {
            // Only read-capable tools exist in this session. Shells, editors,
            // notebooks and subagents are absent rather than denied, and the
            // delegation extension's tools are not in the allowlist, so a
            // subtask cannot recurse.
            command.args(review_read_only_args());
        } else {
            command.args(["--mode", "rpc", flavor.full_access_arg()]);
        }
        if flavor.skips_version_check_by_env() {
            command.env("PI_SKIP_VERSION_CHECK", "1");
        }
        configure_pi_delegation_command(
            &mut command,
            flavor,
            delegation_session_path.as_deref(),
            crate::delegation::pi_extension_path,
        );
        let command = command
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::command_env::spawn(command)
            .with_context(|| format!("failed to start `{} --mode rpc`", binary.display()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("{} stdin unavailable", flavor.display_name()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("{} stdout unavailable", flavor.display_name()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("{} stderr unavailable", flavor.display_name()))?;

        let (commands, command_rx) = unbounded();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        let reader_commands = commands.clone();
        let reader_events = events.clone();
        let reader_thread =
            thread::Builder::new()
                .name("waku-pi-reader".into())
                .spawn(move || {
                    let mut stream_state = PiStreamState::default();
                    for line in BufReader::new(stdout).lines() {
                        match line {
                            Ok(line) if !line.trim().is_empty() => {
                                match serde_json::from_str::<Value>(&line) {
                                    Ok(value) => handle_pi_message(
                                        flavor,
                                        value,
                                        &reader_pending,
                                        &reader_commands,
                                        &reader_events,
                                        &mut stream_state,
                                    ),
                                    Err(error) => {
                                        let _ = reader_events.send(DriverEvent::Error(tr!(
                                            "errors.provider_invalid_json",
                                            provider = flavor.display_name(),
                                            error = error
                                        )));
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                let _ = reader_events.send(DriverEvent::Error(tr!(
                                    "errors.provider_transport_read",
                                    provider = flavor.display_name(),
                                    error = error
                                )));
                                break;
                            }
                        }
                    }
                    flush_pending_message_text(&reader_events, &mut stream_state);
                    // Unblock anything waiting on an RPC reply immediately; the
                    // process thread owns the `ProcessExited` announcement so a
                    // non-zero exit can be reported before the runtime is torn down.
                    fail_pending(
                        &reader_pending,
                        &format!("{} RPC process exited", flavor.display_name()),
                    );
                })?;

        let writer_pending = pending;
        let writer_events = events.clone();
        thread::Builder::new()
            .name("waku-pi-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                let mut next_request_id = 0_u64;
                let initialize = (|| -> Result<Value, String> {
                    let _ = send_request_with_timeout(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({"type": "get_state"}),
                        INITIALIZE_TIMEOUT,
                    )?;
                    if let Some(session_file) = resume_session_file {
                        let response = send_request_with_timeout(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({
                                "type": "switch_session",
                                "sessionPath": session_file
                            }),
                            INITIALIZE_TIMEOUT,
                        )?;
                        if response.pointer("/data/cancelled").and_then(Value::as_bool)
                            == Some(true)
                        {
                            return Err(format!(
                                "{} session switch was cancelled",
                                flavor.display_name()
                            ));
                        }
                    }
                    if let Some(model) = model.as_deref() {
                        let (provider, model_id) =
                            parse_model_slug(model).map_err(|error| error.to_string())?;
                        let _ = send_request_with_timeout(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({
                                "type": "set_model",
                                "provider": provider,
                                "modelId": model_id
                            }),
                            INITIALIZE_TIMEOUT,
                        )?;
                    }
                    if let Some(level) = reasoning_effort.as_deref() {
                        let _ = send_request_with_timeout(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({"type": "set_thinking_level", "level": level}),
                            INITIALIZE_TIMEOUT,
                        )?;
                    }
                    send_request_with_timeout(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({"type": "get_state"}),
                        INITIALIZE_TIMEOUT,
                    )
                })();

                let state = match initialize {
                    Ok(state) => state,
                    Err(error) => {
                        let _ = writer_events.send(DriverEvent::Error(tr!(
                            "errors.initialize_provider",
                            provider = flavor.display_name(),
                            error = error
                        )));
                        let _ = writer_events.send(DriverEvent::TurnFinished {
                            success: false,
                            summary: Some(tr!(
                                "errors.provider_initialize_session",
                                provider = flavor.display_name()
                            )),
                        });
                        return;
                    }
                };
                let Some(mut cursor) = cursor_from_state(flavor, &state) else {
                    let _ = writer_events.send(DriverEvent::Error(tr!(
                        "errors.provider_no_session_id",
                        provider = flavor.display_name()
                    )));
                    let _ = writer_events.send(DriverEvent::TurnFinished {
                        success: false,
                        summary: Some(tr!(
                            "errors.provider_initialize_session",
                            provider = flavor.display_name()
                        )),
                    });
                    return;
                };
                let initial_usage = send_request(
                    &mut stdin,
                    &writer_pending,
                    &mut next_request_id,
                    json!({"type": "get_session_stats"}),
                )
                .ok()
                .and_then(|stats| pi_context_usage(&state, Some(&stats)))
                .or_else(|| pi_context_usage(&state, None));
                let _ = writer_events.send(DriverEvent::Connected {
                    provider_cursor: Some(cursor.clone()),
                });
                if let Some((context_tokens, context_window)) = initial_usage {
                    let _ = writer_events.send(DriverEvent::UsageUpdated {
                        context_tokens,
                        context_window,
                    });
                }
                if let Some(title) = state
                    .pointer("/data/sessionName")
                    .and_then(Value::as_str)
                    .filter(|title| !title.trim().is_empty())
                {
                    let title = Some(title.to_owned());
                    let _ =
                        writer_events.send(DriverEvent::AutoTitleUpdated(title));
                }

                // Pi reports extension commands, prompts and skills on request.
                if let Ok(catalog) = send_request(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({"type": "get_commands"}),
                    )
                {
                    publish_commands(
                        crate::slash_command_catalog::parse_pi_commands(&catalog),
                        &writer_events,
                    );
                }

                // Both flavors expose setters for these, so changing either is
                // an RPC on the live session rather than a restart.
                let mut current_model = model;
                let mut current_effort = reasoning_effort;
                while let Ok(message) = command_rx.recv() {
                    match message {
                        CommandMessage::RefreshContextUsage => {
                            // Compaction can finish without a model call. Pi
                            // persists tokensAfter but omits it from the RPC
                            // result; fetch its rebased occupancy on this worker.
                            if let Ok(stats) = send_request(
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                json!({"type": "get_session_stats"}),
                            ) && let Some((context_tokens, context_window)) =
                                pi_context_usage(&Value::Null, Some(&stats))
                            {
                                let _ = writer_events.send(DriverEvent::UsageUpdated {
                                    context_tokens,
                                    context_window,
                                });
                            }
                        }
                        CommandMessage::Compact(request) => {
                            next_request_id += 1;
                            let rpc_id = format!("waku-{}", next_request_id);
                            let item = ContextCompaction::new(request.id, Some(CompactionTrigger::Manual), unix_time_millis());
                            writer_pending.lock().insert(rpc_id.clone(), PendingResponse::Compact(item.clone()));
                            let _ = writer_events.send(DriverEvent::CompactionUpdated(item.clone()));
                            if let Err(error) = write_json_line(&mut stdin, &json!({"id": rpc_id, "type": "compact", "customInstructions": request.instructions})) {
                                writer_pending.lock().remove(&rpc_id);
                                let mut item = item;
                                item.error = Some(error.to_string());
                                item.finish(CompactionStatus::Failed, unix_time_millis());
                                let _ = writer_events.send(DriverEvent::CompactionUpdated(item));
                            }
                        }
                        CommandMessage::Prompt(prompt) => {
                            let result = send_prompt(
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                &prompt,
                            );
                            if let Err(error) = result {
                                let _ = writer_events.send(DriverEvent::Error(tr!(
                                    "errors.provider_rejected_prompt_detail",
                                    provider = flavor.display_name(),
                                    error = error
                                )));
                                let _ = writer_events.send(DriverEvent::TurnFinished {
                                    success: false,
                                    summary: Some(tr!(
                                        "errors.provider_rejected_prompt",
                                        provider = flavor.display_name()
                                    )),
                                });
                            }
                        }
                        CommandMessage::Steer(prompt) => {
                            let result = send_request(
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                json!({"type": "steer", "message": prompt}),
                            );
                            match result {
                                Ok(_) => {
                                    let _ = writer_events
                                        .send(DriverEvent::SteerAccepted { message: prompt });
                                }
                                Err(error) => {
                                    let _ = writer_events.send(DriverEvent::SteerRejected {
                                        message: prompt,
                                        reason: error,
                                    });
                                }
                            }
                        }
                        CommandMessage::Cancel => {
                            interrupt_pending_compactions(&writer_pending, &writer_events);
                            if let Err(error) = send_request(
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                json!({"type": "abort"}),
                            ) {
                                let _ = writer_events.send(DriverEvent::Error(tr!(
                                    "errors.stop_provider",
                                    provider = flavor.display_name(),
                                    error = error
                                )));
                            }
                        }
                        CommandMessage::Options(options) => {
                            if options.model != current_model {
                                match options.model.as_deref().map(parse_model_slug).transpose() {
                                    Ok(Some((provider, model_id))) => {
                                        match send_request(
                                            &mut stdin,
                                            &writer_pending,
                                            &mut next_request_id,
                                            json!({
                                                "type": "set_model",
                                                "provider": provider,
                                                "modelId": model_id
                                            }),
                                        ) {
                                            Ok(response) => {
                                                if let Some(window) = response
                                                    .pointer("/data/contextWindow")
                                                    .and_then(Value::as_u64)
                                                    .filter(|window| *window > 0)
                                                {
                                                    let _ = writer_events.send(
                                                        DriverEvent::UsageUpdated {
                                                            context_tokens: None,
                                                            context_window: Some(window),
                                                        },
                                                    );
                                                }
                                            }
                                            Err(error) => {
                                                let _ =
                                                    writer_events.send(DriverEvent::Error(tr!(
                                                        "errors.switch_provider_model",
                                                        provider = flavor.display_name(),
                                                        error = error
                                                    )));
                                            }
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(error) => {
                                        let _ = writer_events
                                            .send(DriverEvent::Error(error.to_string()));
                                    }
                                }
                                current_model = options.model;
                            }
                            if options.reasoning_effort != current_effort {
                                if let Some(level) = options.reasoning_effort.as_deref()
                                    && let Err(error) = send_request(
                                        &mut stdin,
                                        &writer_pending,
                                        &mut next_request_id,
                                        json!({"type": "set_thinking_level", "level": level}),
                                    )
                                {
                                    let _ = writer_events.send(DriverEvent::Error(tr!(
                                        "errors.change_provider_thinking",
                                        provider = flavor.display_name(),
                                        error = error
                                    )));
                                }
                                current_effort = options.reasoning_effort;
                            }
                        }
                        CommandMessage::CancelExtensionRequest(id) => {
                            if write_json_line(
                                &mut stdin,
                                &json!({
                                    "type": "extension_ui_response",
                                    "id": id,
                                    "cancelled": true
                                }),
                            )
                            .is_err()
                            {
                                break;
                            }
                        }
                        CommandMessage::Rollback { turns, response } => {
                            let result = fork_pi_session(
                                flavor,
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                &cursor,
                                turns,
                                false,
                            );
                            if let Ok(next_cursor) = &result {
                                cursor = next_cursor.clone();
                            }
                            let _ = response.send(result);
                        }
                        CommandMessage::Fork {
                            turns_to_remove,
                            response,
                        } => {
                            let result = fork_pi_session(
                                flavor,
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                &cursor,
                                turns_to_remove,
                                true,
                            );
                            let _ = response.send(result);
                        }
                        CommandMessage::Shutdown => break,
                    }
                }
            })?;

        let last_visible_stderr = Arc::new(Mutex::new(None::<String>));
        let stderr_last_error = last_visible_stderr.clone();
        let stderr_events = events.clone();
        let stderr_thread =
            thread::Builder::new()
                .name("waku-pi-stderr".into())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        if line.to_ascii_lowercase().contains("error") {
                            let error = format!("{}: {}", flavor.display_name(), line.trim());
                            *stderr_last_error.lock() = Some(error.clone());
                            let _ = stderr_events.send(DriverEvent::Error(error));
                        }
                    }
                })?;

        // Nothing signals or kills the agent process: it exits when the writer
        // thread drops its stdin. Something still has to reap it, or every
        // session that ever ran leaves a zombie behind for the life of the app.
        thread::Builder::new()
            .name("waku-pi-process".into())
            .spawn(move || {
                let status = child.wait();
                let _ = reader_thread.join();
                let _ = stderr_thread.join();
                match status {
                    Ok(status) if !status.success() && last_visible_stderr.lock().is_none() => {
                        let _ = events.send(DriverEvent::Error(tr!(
                            "errors.provider_rpc_exited",
                            provider = flavor.display_name(),
                            status = status
                        )));
                    }
                    Err(error) => {
                        let _ = events.send(DriverEvent::Error(tr!(
                            "errors.read_provider_exit_status",
                            provider = format!("{} RPC", flavor.display_name()),
                            error = error
                        )));
                    }
                    _ => {}
                }
                let _ = events.send(DriverEvent::ProcessExited);
            })?;

        Ok(Self { flavor, commands })
    }
}

impl DriverControl for PiDriver {
    fn provider(&self) -> crate::model::ProviderKind {
        self.flavor.provider()
    }

    fn compact(&self, request: CompactionRequest) {
        let _ = self.commands.send(CommandMessage::Compact(request));
    }

    fn prompt(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Prompt(prompt));
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities { steer: true }
    }

    fn steer(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Steer(prompt));
    }

    fn cancel(&self) {
        let _ = self.commands.send(CommandMessage::Cancel);
    }

    fn respond(&self, _request_id: String, _option_id: String) {}

    fn apply_options(&self, options: SessionOptions) -> bool {
        // Both flavors have setters for the model and thinking level, so those
        // apply to the live session. Neither exposes one for permissions — and
        // Waku only runs them with Full access anyway, so a mode change asks
        // for a fresh start, which is where that is reported.
        if !self
            .flavor
            .provider()
            .capabilities()
            .supports_access_mode(options.mode)
        {
            return false;
        }
        self.commands.send(CommandMessage::Options(options)).is_ok()
    }

    fn rollback(&self, turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        if turns == 0 {
            return Ok(None);
        }
        let (response_tx, response_rx) = bounded(1);
        self.commands
            .send(CommandMessage::Rollback {
                turns,
                response: response_tx,
            })
            .with_context(|| {
                format!(
                    "{} driver stopped before rollback",
                    self.flavor.display_name()
                )
            })?;
        response_rx
            .recv_timeout(Duration::from_secs(60))
            .with_context(|| {
                format!(
                    "timed out waiting for {} conversation rollback",
                    self.flavor.display_name()
                )
            })?
            .map(Some)
            .map_err(anyhow::Error::msg)
    }

    fn fork(&self, turns_to_remove: usize) -> anyhow::Result<ProviderResumeCursor> {
        let (response_tx, response_rx) = bounded(1);
        self.commands
            .send(CommandMessage::Fork {
                turns_to_remove,
                response: response_tx,
            })
            .with_context(|| {
                format!(
                    "{} driver stopped before forking",
                    self.flavor.display_name()
                )
            })?;
        response_rx
            .recv_timeout(Duration::from_secs(60))
            .with_context(|| {
                format!(
                    "timed out waiting for {} conversation fork",
                    self.flavor.display_name()
                )
            })?
            .map_err(anyhow::Error::msg)
    }
}

impl Drop for PiDriver {
    fn drop(&mut self) {
        let _ = self.commands.send(CommandMessage::Shutdown);
    }
}

fn send_request(
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    request: Value,
) -> Result<Value, String> {
    send_request_with_timeout(stdin, pending, next_request_id, request, RPC_TIMEOUT)
}

fn send_request_with_timeout(
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    mut request: Value,
    timeout: Duration,
) -> Result<Value, String> {
    *next_request_id += 1;
    let id = format!("waku-{}", next_request_id);
    request["id"] = Value::String(id.clone());
    let (response_tx, response_rx) = bounded(1);
    pending
        .lock()
        .insert(id.clone(), PendingResponse::Request(response_tx));
    if let Err(error) = write_json_line(stdin, &request) {
        pending.lock().remove(&id);
        return Err(format!("transport write failed: {error}"));
    }
    match response_rx.recv_timeout(timeout) {
        Ok(response) => response,
        Err(_) => {
            pending.lock().remove(&id);
            Err(format!(
                "{} timed out after {}s",
                request["type"].as_str().unwrap_or("request"),
                timeout.as_secs()
            ))
        }
    }
}

fn send_prompt(
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    prompt: &str,
) -> Result<(), String> {
    *next_request_id += 1;
    let id = format!("waku-{}", next_request_id);
    {
        let mut pending = pending.lock();
        // A response from an older prompt cannot settle the next turn.
        pending.retain(|_, response| {
            matches!(
                response,
                PendingResponse::Request(_) | PendingResponse::Compact(_)
            )
        });
        pending.insert(id.clone(), PendingResponse::Prompt);
    }
    // Built-ins can hold the prompt response until compaction or another
    // command finishes. Do not apply the short control-RPC timeout or block
    // the writer from sending abort while waiting for that response.
    if let Err(error) = write_json_line(
        stdin,
        &json!({"id": id, "type": "prompt", "message": prompt}),
    ) {
        pending.lock().remove(&id);
        return Err(format!("transport write failed: {error}"));
    }
    Ok(())
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn fail_pending(pending: &PendingResponses, message: &str) {
    for (_, response) in pending.lock().drain() {
        if let PendingResponse::Request(response) = response {
            let _ = response.send(Err(message.to_owned()));
        }
    }
}

fn parse_model_slug(model: &str) -> anyhow::Result<(&str, &str)> {
    let Some((provider, model_id)) = model.trim().split_once('/') else {
        return Err(anyhow!(
            "models must use provider/model format; received `{model}`"
        ));
    };
    if provider.is_empty() || model_id.is_empty() {
        return Err(anyhow!(
            "models must use provider/model format; received `{model}`"
        ));
    }
    Ok((provider, model_id))
}

fn cursor_from_state(flavor: PiFlavor, response: &Value) -> Option<ProviderResumeCursor> {
    let session_id = response
        .pointer("/data/sessionId")
        .and_then(Value::as_str)?;
    let session_file = response
        .pointer("/data/sessionFile")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    Some(flavor.cursor(session_id.to_owned(), session_file))
}

/// Pi already computes context occupancy for its own footer. Prefer that
/// native value when session stats are available, and use the active model in
/// `get_state` for the window before the first assistant message arrives.
fn pi_context_usage(state: &Value, stats: Option<&Value>) -> Option<(Option<u64>, Option<u64>)> {
    let context = stats.and_then(|stats| stats.pointer("/data/contextUsage"));
    let tokens = context
        .and_then(|context| context.get("tokens"))
        .and_then(Value::as_u64);
    let window = context
        .and_then(|context| context.get("contextWindow"))
        .and_then(Value::as_u64)
        .or_else(|| {
            state
                .pointer("/data/model/contextWindow")
                .and_then(Value::as_u64)
        })
        .filter(|window| *window > 0);
    (tokens.is_some() || window.is_some()).then_some((tokens, window))
}

/// Pi's providers normally fill `totalTokens`, but Pi itself deliberately
/// falls back to the four component counters when a provider leaves it zero.
/// Keep Waku's meter aligned with that provider-native calculation.
fn pi_message_context_tokens(message: &Value) -> Option<u64> {
    let usage = message.get("usage")?;
    usage
        .get("totalTokens")
        .and_then(Value::as_u64)
        .filter(|tokens| *tokens > 0)
        .or_else(|| {
            let total = ["input", "output", "cacheRead", "cacheWrite"]
                .into_iter()
                .filter_map(|field| usage.get(field).and_then(Value::as_u64))
                .fold(0_u64, u64::saturating_add);
            (total > 0).then_some(total)
        })
}

fn fork_pi_session(
    flavor: PiFlavor,
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    original_cursor: &ProviderResumeCursor,
    turns_to_remove: usize,
    restore_original: bool,
) -> Result<ProviderResumeCursor, String> {
    let name = flavor.display_name();
    let messages = send_request(
        stdin,
        pending,
        next_request_id,
        json!({"type": flavor.branch_messages_command()}),
    )?;
    let messages = messages
        .pointer("/data/messages")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{name} returned an invalid fork-message list"))?;

    let request = pi_fork_request(flavor, messages, turns_to_remove)?;
    let fork = send_request(stdin, pending, next_request_id, request)?;
    if fork.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
        return Err(format!("{name} session fork was cancelled"));
    }
    let fork_state = send_request(
        stdin,
        pending,
        next_request_id,
        json!({"type": "get_state"}),
    )?;
    let fork_cursor = cursor_from_state(flavor, &fork_state)
        .ok_or_else(|| format!("{name} did not report the forked session cursor"))?;

    if restore_original {
        let session_file = flavor
            .session_file_from_cursor(original_cursor)
            .ok_or_else(|| format!("{name}'s original session file is unavailable"))?;
        let switched = send_request(
            stdin,
            pending,
            next_request_id,
            json!({
                "type": "switch_session",
                "sessionPath": session_file
            }),
        )?;
        if switched.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
            return Err(format!(
                "{name} could not return to the source session after forking"
            ));
        }
        let restored_state = send_request(
            stdin,
            pending,
            next_request_id,
            json!({"type": "get_state"}),
        )?;
        let restored_cursor = cursor_from_state(flavor, &restored_state)
            .ok_or_else(|| format!("{name} did not report the restored source session"))?;
        if restored_cursor.native_id() != original_cursor.native_id() {
            return Err(format!(
                "{name} returned to the wrong source session after forking"
            ));
        }
    }

    Ok(fork_cursor)
}

fn pi_fork_request(
    flavor: PiFlavor,
    messages: &[Value],
    turns_to_remove: usize,
) -> Result<Value, String> {
    let name = flavor.display_name();
    if turns_to_remove > messages.len() {
        return Err(format!(
            "{name} has only {} native turns, but Waku needs to remove {turns_to_remove}",
            messages.len()
        ));
    }
    let retained_turns = messages.len() - turns_to_remove;
    if turns_to_remove == 0 {
        // Only reachable for Pi; Oh My Pi copies out of process instead.
        Ok(json!({"type": "clone"}))
    } else {
        let entry_id = messages
            .get(retained_turns)
            .and_then(|message| message.get("entryId"))
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{name} returned a fork message without an entry ID"))?;
        Ok(json!({"type": flavor.branch_command(), "entryId": entry_id}))
    }
}

#[derive(Clone)]
struct PiTrackedTool {
    kind: ActivityKind,
    title: String,
    tool_name: Option<String>,
    saw_end: bool,
    settled_async: Option<(Value, bool)>,
}

#[derive(Default)]
struct PiStreamState {
    compaction: CompactionTracker,
    run_started: bool,
    message_saw_text: bool,
    message_saw_reasoning: bool,
    message_is_deepseek: bool,
    message_pending_text: String,
    reasoning_blocks: HashMap<u64, (String, bool)>,
    failed: bool,
    tools: HashMap<String, PiTrackedTool>,
}

impl PiStreamState {
    fn reset_turn(&mut self) {
        *self = Self::default();
    }

    fn reasoning_block(
        &mut self,
        index: u64,
        events: &impl DriverEventSink,
    ) -> &mut (String, bool) {
        self.reasoning_block_at(index, unix_time_millis(), events)
    }

    fn reasoning_block_at(
        &mut self,
        index: u64,
        timestamp_ms: u64,
        events: &impl DriverEventSink,
    ) -> &mut (String, bool) {
        self.reasoning_blocks.entry(index).or_insert_with(|| {
            let id = format!("pi-reasoning-{}", uuid::Uuid::new_v4());
            let _ = events.send(DriverEvent::ReasoningStarted {
                id: id.clone(),
                timestamp_ms,
            });
            (id, false)
        })
    }

    fn finish_reasoning_block(
        &mut self,
        index: u64,
        content: Option<String>,
        events: &impl DriverEventSink,
    ) {
        let (id, complete) = self.reasoning_block_at(index, 0, events);
        if !*complete {
            *complete = true;
            let _ = events.send(DriverEvent::ReasoningCompleted {
                id: id.clone(),
                timestamp_ms: unix_time_millis(),
                content,
            });
        }
    }
}

fn handle_pi_message(
    flavor: PiFlavor,
    value: Value,
    pending: &PendingResponses,
    commands: &Sender<CommandMessage>,
    events: &impl DriverEventSink,
    state: &mut PiStreamState,
) {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type == "response" {
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            return;
        };
        let compact = {
            let mut pending = pending.lock();
            if matches!(pending.get(id), Some(PendingResponse::Compact(_))) {
                match pending.remove(id) {
                    Some(PendingResponse::Compact(item)) => Some(item),
                    _ => None,
                }
            } else {
                None
            }
        };
        if let Some(item) = compact {
            let cancelled = item.status == CompactionStatus::Interrupted;
            let same_operation = state
                .compaction
                .current
                .as_ref()
                .is_some_and(|current| current.id == item.id);
            let mut operation = CompactionTracker {
                current: Some(if same_operation && !cancelled {
                    state.compaction.current.as_ref().unwrap().clone()
                } else {
                    item
                }),
            };
            let success = value.get("success").and_then(Value::as_bool) == Some(true);
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned);
            operation.finish(
                if cancelled {
                    CompactionStatus::Interrupted
                } else if success {
                    CompactionStatus::Completed
                } else {
                    CompactionStatus::Failed
                },
                value.get("data"),
                if cancelled { None } else { error },
                events,
            );
            if success && !cancelled {
                let _ = commands.send(CommandMessage::RefreshContextUsage);
            }
            if same_operation
                || state
                    .compaction
                    .current
                    .as_ref()
                    .is_none_or(|current| current.status != CompactionStatus::Running)
            {
                state.compaction = operation;
            }
            return;
        }
        let prompt_response = matches!(pending.lock().get(id), Some(PendingResponse::Prompt));
        if prompt_response {
            let success = value.get("success").and_then(Value::as_bool) == Some(true);
            let local_only =
                value.pointer("/data/agentInvoked").and_then(Value::as_bool) == Some(false);
            if success && !local_only {
                // Pi may acknowledge a prompt before reporting that an
                // extension handled it locally. Retain the id for that second
                // response; normal agent completion retires it below.
                return;
            }
            pending.lock().remove(id);
            if !state.run_started {
                let _ = events.send(DriverEvent::TurnStarted);
            }
            let error = (!success).then(|| {
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{} RPC command failed", flavor.display_name()))
            });
            if let Some(error) = error.as_ref() {
                let _ = events.send(DriverEvent::Error(error.clone()));
            }
            let _ = events.send(DriverEvent::TurnFinished {
                success,
                summary: error,
            });
            state.reset_turn();
            return;
        }
        let Some(PendingResponse::Request(response)) = pending.lock().remove(id) else {
            return;
        };
        if value.get("success").and_then(Value::as_bool) == Some(true) {
            let _ = response.send(Ok(value));
        } else {
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{} RPC command failed", flavor.display_name()));
            let _ = response.send(Err(error));
        }
        return;
    }


    if event_type == flavor.session_info_event() {
        let title = value
            .get(flavor.session_info_title_field())
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty());
        let title = title.map(str::to_owned);
        let _ = events.send(DriverEvent::AutoTitleUpdated(title));
        return;
    }

    if event_type == flavor.settled_event() {
        if value.get("isTerminal").and_then(Value::as_bool) == Some(false) {
            return;
        }
        flush_pending_message_text(events, state);
        if state.run_started {
            pending.lock().retain(|_, response| {
                matches!(
                    response,
                    PendingResponse::Request(_) | PendingResponse::Compact(_)
                )
            });
            let success = !state.failed;
            let _ = events.send(DriverEvent::TurnFinished {
                success,
                summary: (!success).then(|| {
                    tr!(
                        "errors.provider_complete_turn",
                        provider = flavor.display_name()
                    )
                }),
            });
        }
        state.reset_turn();
        return;
    }

    match event_type {
        "compaction_start" | "auto_compaction_start" | "manual_compaction_start" => {
            let manual = pending.lock().values().find_map(|entry| match entry {
                PendingResponse::Compact(item) => Some(item.clone()),
                _ => None,
            });
            if let Some(item) = manual {
                state.compaction.seed(item);
            }
            let trigger = match value.get("reason").and_then(Value::as_str) {
                Some("manual") => Some(CompactionTrigger::Manual),
                Some("threshold" | "overflow") => Some(CompactionTrigger::Auto),
                _ if event_type == "auto_compaction_start" => Some(CompactionTrigger::Auto),
                _ if event_type == "manual_compaction_start" => Some(CompactionTrigger::Manual),
                _ => None,
            };
            state.compaction.start(None, trigger, events);
        }
        "compaction_end" | "auto_compaction_end" | "manual_compaction_end" => {
            let manual = pending.lock().values().find_map(|entry| match entry {
                PendingResponse::Compact(item) => Some(item.status),
                _ => None,
            });
            let status = if manual == Some(CompactionStatus::Interrupted)
                || value.get("aborted").and_then(Value::as_bool) == Some(true)
            {
                CompactionStatus::Interrupted
            } else if value.get("result").is_some_and(|result| !result.is_null()) {
                CompactionStatus::Completed
            } else {
                CompactionStatus::Failed
            };
            state.compaction.finish(
                status,
                value.get("result"),
                value
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                events,
            );
            // A manual RPC response refreshes once it settles. Automatic and
            // provider-initiated compactions have no command response to do so.
            if status == CompactionStatus::Completed && manual.is_none() {
                let _ = commands.send(CommandMessage::RefreshContextUsage);
            }
        }
        "command_output" => {
            if let Some(text) = value
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                let was_idle = !state.run_started;
                if was_idle {
                    let _ = events.send(DriverEvent::TurnStarted);
                }
                // Each command_output is a complete output block, unlike
                // assistant deltas. Keep successive reports separated.
                let _ = events.send(DriverEvent::TextDelta(format!("{text}\n\n")));
                if was_idle
                    && !pending
                        .lock()
                        .values()
                        .any(|response| matches!(response, PendingResponse::Prompt))
                {
                    let _ = events.send(DriverEvent::TurnFinished {
                        success: true,
                        summary: None,
                    });
                } else if was_idle {
                    state.run_started = true;
                }
            }
        }
        "agent_start" | "turn_start" => {
            if !state.run_started {
                state.run_started = true;
                state.failed = false;
                let _ = events.send(DriverEvent::TurnStarted);
            }
        }
        "message_start" => {
            if value.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                flush_pending_message_text(events, state);
                state.message_saw_text = false;
                state.message_saw_reasoning = false;
                state.reasoning_blocks.clear();
                state.message_is_deepseek = value
                    .get("message")
                    .is_some_and(crate::pi_session::is_deepseek_message);
            }
        }
        "message_update" => {
            let update = value.get("assistantMessageEvent").unwrap_or(&Value::Null);
            state.message_is_deepseek |= value
                .get("message")
                .or_else(|| update.get("partial"))
                .is_some_and(crate::pi_session::is_deepseek_message);
            match update.get("type").and_then(Value::as_str) {
                Some("thinking_start") => {
                    if let Some(index) = update.get("contentIndex").and_then(Value::as_u64) {
                        state.reasoning_block(index, events);
                    }
                }
                Some("text_delta") => {
                    if let Some(delta) = update
                        .get("delta")
                        .and_then(Value::as_str)
                        .filter(|delta| !delta.is_empty())
                    {
                        let may_be_placeholder = state.message_is_deepseek
                            && (!state.message_saw_text || !state.message_pending_text.is_empty());
                        state.message_saw_text = true;
                        if may_be_placeholder {
                            state.message_pending_text.push_str(delta);
                            let text = state.message_pending_text.trim();
                            if state.message_pending_text.len() > MAX_PENDING_PLACEHOLDER_BYTES
                                || (!text.is_empty() && text != ".")
                            {
                                flush_pending_message_text(events, state);
                            }
                        } else {
                            let _ = events.send(DriverEvent::TextDelta(delta.to_owned()));
                        }
                    }
                }
                Some("thinking_delta") => {
                    if let Some(delta) = update
                        .get("delta")
                        .and_then(Value::as_str)
                        .filter(|delta| !delta.is_empty())
                    {
                        flush_pending_message_text(events, state);
                        state.message_saw_reasoning = true;
                        if let Some(index) = update.get("contentIndex").and_then(Value::as_u64) {
                            let (id, complete) = state.reasoning_block_at(index, 0, events);
                            if !*complete {
                                let _ = events.send(DriverEvent::ReasoningBlockDelta {
                                    id: id.clone(),
                                    delta: delta.to_owned(),
                                });
                            }
                        } else {
                            let _ = events.send(DriverEvent::ReasoningDelta(delta.to_owned()));
                        }
                    }
                }
                Some("thinking_end") => {
                    if let Some(index) = update.get("contentIndex").and_then(Value::as_u64) {
                        flush_pending_message_text(events, state);
                        state.finish_reasoning_block(
                            index,
                            update
                                .get("content")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            events,
                        );
                    }
                }
                Some("error") => {
                    flush_pending_message_text(events, state);
                    state.failed = true;
                    let _ = events.send(DriverEvent::Error(pi_error_message(flavor, update)));
                }
                _ => {}
            }
        }
        "message_end" => {
            if value.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                // This is the context the next call starts from, not the
                // cumulative billed total for the whole session.
                if let Some(tokens) = value.get("message").and_then(pi_message_context_tokens) {
                    let _ = events.send(DriverEvent::UsageUpdated {
                        context_tokens: Some(tokens),
                        context_window: None,
                    });
                }
                emit_completed_message_fallback(value.get("message"), events, state);
            }
        }
        "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
            // Speculative tools may run before message_end. Leave a possible
            // filler period pending until the completed text confirms it;
            // a later delta may still extend it into ordinary prose.
            let id = value
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let raw_tool_name = value.get("toolName").and_then(Value::as_str);
            let xdev_call = resolve_pi_xdev_tool(&value);
            let tracked = id.as_ref().and_then(|id| state.tools.get(id)).cloned();
            let resolved_tool_name = tracked
                .as_ref()
                .and_then(|tool| tool.tool_name.clone())
                .or_else(|| xdev_call.as_ref().map(|call| call.tool_name.clone()))
                .or_else(|| raw_tool_name.map(str::to_owned));
            let (kind, mut title) = tracked
                .as_ref()
                .map(|tool| (tool.kind, tool.title.clone()))
                .or_else(|| {
                    xdev_call
                        .as_ref()
                        .map(|call| (call.kind, call.title.clone()))
                })
                .unwrap_or_else(|| {
                    resolved_tool_name
                        .as_deref()
                        .map(|tool_name| (classify_tool(tool_name), tool_title(tool_name)))
                        .unwrap_or_else(|| (ActivityKind::Tool, tr!("activity.tool")))
                });
            let effective_args = xdev_call
                .as_ref()
                .and_then(|call| call.arguments.as_ref())
                .or_else(|| value.get("args"));
            if (event_type == "tool_execution_start" || tracked.is_none())
                && let Some(input_title) = activity::input_title(effective_args)
            {
                title = input_title;
            }
            if let Some(id) = id.as_ref() {
                if event_type == "tool_execution_start"
                    || (tracked.is_none() && event_type == "tool_execution_update")
                {
                    state.tools.insert(
                        id.clone(),
                        PiTrackedTool {
                            kind,
                            title: title.clone(),
                            tool_name: resolved_tool_name.clone(),
                            saw_end: false,
                            settled_async: None,
                        },
                    );
                }
            }
            let arguments = (event_type == "tool_execution_start")
                .then_some(effective_args)
                .flatten();
            let mut output = match event_type {
                "tool_execution_update" => value.get("partialResult").cloned(),
                "tool_execution_end" => value.get("result").cloned(),
                _ => None,
            };
            let mut complete = event_type == "tool_execution_end";
            let mut failed = value.get("isError").and_then(Value::as_bool) == Some(true);
            if !failed {
                failed = value
                    .pointer("/partialResult/isError")
                    .or_else(|| value.pointer("/result/isError"))
                    .and_then(Value::as_bool)
                    == Some(true);
            }
            let async_state = value
                .pointer("/partialResult/details/async/state")
                .or_else(|| value.pointer("/result/details/async/state"))
                .and_then(Value::as_str);
            match async_state {
                Some("running") => {
                    if event_type == "tool_execution_end"
                        && let Some((settled_output, settled_failed)) = tracked
                            .as_ref()
                            .and_then(|tool| tool.settled_async.as_ref())
                    {
                        output = Some(settled_output.clone());
                        failed = *settled_failed;
                        complete = true;
                    } else {
                        complete = false;
                        if event_type == "tool_execution_end"
                            && let Some(id) = id.as_ref()
                            && let Some(entry) = state.tools.get_mut(id)
                        {
                            entry.saw_end = true;
                        }
                    }
                }
                Some("completed") => {
                    complete = true;
                    if event_type == "tool_execution_update"
                        && !tracked.as_ref().is_some_and(|tool| tool.saw_end)
                        && let Some(id) = id.as_ref()
                        && let Some(entry) = state.tools.get_mut(id)
                    {
                        entry.settled_async = output.clone().map(|out| (out, failed));
                    }
                }
                Some("failed" | "cancelled") => {
                    complete = true;
                    failed = true;
                    if event_type == "tool_execution_update"
                        && !tracked.as_ref().is_some_and(|tool| tool.saw_end)
                        && let Some(id) = id.as_ref()
                        && let Some(entry) = state.tools.get_mut(id)
                    {
                        entry.settled_async = output.clone().map(|out| (out, true));
                    }
                }
                _ => {}
            }
            let mut item = activity::tool_activity(
                id.clone(),
                kind,
                title,
                arguments,
                output.as_ref(),
                output.as_ref(),
                failed,
                complete,
            )
            .with_tool_name(resolved_tool_name.as_deref());
            if async_state == Some("running") && !complete {
                item.complete = false;
            }
            let _ = events.send(DriverEvent::RichActivity(item));
            if let Some(id) = id {
                let finish_tracking = (event_type == "tool_execution_end" && complete)
                    || (event_type == "tool_execution_update"
                        && matches!(async_state, Some("completed" | "failed" | "cancelled"))
                        && tracked.as_ref().is_some_and(|tool| tool.saw_end));
                if finish_tracking {
                    state.tools.remove(&id);
                }
            }
        }
        "auto_retry_end" => {
            if value.get("success").and_then(Value::as_bool) == Some(true) {
                state.failed = false;
            } else {
                state.failed = true;
                let message = value
                    .get("finalError")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!("{} exhausted its automatic retries", flavor.display_name())
                    });
                let _ = events.send(DriverEvent::Error(message));
            }
        }
        "extension_ui_request" => {
            let method = value.get("method").and_then(Value::as_str);
            let id = value.get("id").and_then(Value::as_str);
            if matches!(method, Some("select" | "confirm" | "input" | "editor"))
                && let Some(id) = id
            {
                let _ = commands.send(CommandMessage::CancelExtensionRequest(id.to_owned()));
            }
        }
        "extension_error" => {
            let _ = events.send(DriverEvent::Error(pi_error_message(flavor, &value)));
        }
        _ => {}
    }
}

fn flush_pending_message_text(events: &impl DriverEventSink, state: &mut PiStreamState) {
    if !state.message_pending_text.is_empty() {
        let _ = events.send(DriverEvent::TextDelta(std::mem::take(
            &mut state.message_pending_text,
        )));
    }
}

fn publish_commands(
    commands: Vec<waku_protocol::composer::SlashCommand>,
    events: &impl DriverEventSink,
) {
    let commands = commands
        .into_iter()
        .map(|command| ReportedCommand {
            name: command.name,
            description: command.description,
        })
        .collect();
    let _ = events.send(DriverEvent::AvailableCommands(commands));
}

fn emit_completed_message_fallback(
    message: Option<&Value>,
    events: &impl DriverEventSink,
    state: &mut PiStreamState,
) {
    let placeholder = message.is_some_and(crate::pi_session::is_deepseek_tool_placeholder);
    if placeholder {
        state.message_pending_text.clear();
    } else {
        flush_pending_message_text(events, state);
    }
    let Some(content) = message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    let emit_text = !state.message_saw_text && !placeholder;
    let emit_reasoning = !state.message_saw_reasoning;
    for (index, block) in content.iter().enumerate() {
        match block.get("type").and_then(Value::as_str) {
            Some("text") if emit_text => {
                if let Some(text) = block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    state.message_saw_text = true;
                    let _ = events.send(DriverEvent::TextDelta(text.to_owned()));
                }
            }
            Some("thinking") if !state.reasoning_blocks.is_empty() || emit_reasoning => {
                // Snapshot-only blocks have no observed start. Preserve their
                // boundaries and final text without inventing a one-second duration.
                state.reasoning_block_at(index as u64, 0, events);
                state.finish_reasoning_block(
                    index as u64,
                    block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    events,
                );
            }
            _ => {}
        }
    }
}

fn pi_error_message(flavor: PiFlavor, value: &Value) -> String {
    value
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| value.get("errorMessage").and_then(Value::as_str))
        .or_else(|| value.get("reason").and_then(Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            tr!(
                "errors.provider_reported_error",
                provider = flavor.display_name()
            )
        })
}

struct PiXdevCall {
    kind: ActivityKind,
    title: String,
    tool_name: String,
    arguments: Option<Value>,
}

/// Unwraps Oh My Pi's `xd://<tool>` virtual device transport (`read xd://...`
/// for help/catalog and `write xd://<tool>` with JSON `content` for execution)
/// into the underlying tool identity and arguments.
fn resolve_pi_xdev_tool(value: &Value) -> Option<PiXdevCall> {
    if let Some(args) = value.get("args")
        && let Some(raw_path) = args.get("path").and_then(Value::as_str)
        && let Some((device, has_query)) = crate::model::parse_xdev_uri(raw_path)
    {
        if !device.is_empty() && !has_query && args.get("content").is_some() {
            let arguments = args
                .get("content")
                .and_then(Value::as_str)
                .and_then(|content| serde_json::from_str::<Value>(content.trim()).ok());
            return Some(PiXdevCall {
                kind: classify_tool(device),
                title: tool_title(device),
                tool_name: device.to_owned(),
                arguments,
            });
        }
        return Some(PiXdevCall {
            kind: ActivityKind::Tool,
            title: if device.is_empty() {
                "xd://".to_owned()
            } else {
                tool_title(device)
            },
            tool_name: if device.is_empty() {
                "xdev".to_owned()
            } else {
                device.to_owned()
            },
            arguments: None,
        });
    }

    let xdev = value
        .pointer("/partialResult/details/xdev")
        .or_else(|| value.pointer("/result/details/xdev"))?;
    let device = xdev
        .get("tool")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|device| !device.is_empty())?;
    let mode = xdev.get("mode").and_then(Value::as_str);
    if mode == Some("execute") && device != "*" {
        Some(PiXdevCall {
            kind: classify_tool(device),
            title: tool_title(device),
            tool_name: device.to_owned(),
            arguments: xdev.get("args").cloned(),
        })
    } else {
        Some(PiXdevCall {
            kind: ActivityKind::Tool,
            title: if device == "*" {
                "xd://".to_owned()
            } else {
                tool_title(device)
            },
            tool_name: if device == "*" {
                "xdev".to_owned()
            } else {
                device.to_owned()
            },
            arguments: None,
        })
    }
}

fn classify_tool(name: &str) -> ActivityKind {
    ActivityKind::from_tool_name(name)
}

fn tool_title(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "bash" => tr!("activity.run_command"),
        "edit" => tr!("activity.edit_file"),
        "write" => tr!("activity.write_file"),
        "read" => tr!("activity.read_file"),
        "grep" => tr!("activity.search_files"),
        "find" => tr!("activity.find_files"),
        "ls" => tr!("activity.list_files"),
        _ => name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RuntimeMode;
    use crossbeam_channel::TryRecvError;

    #[test]
    fn native_compaction_response_is_metadata_and_never_an_assistant_turn() {
        let (pending, commands, command_rx, mut state) = harness();
        let (events, received) = unbounded();
        let item = ContextCompaction::new("manual", Some(CompactionTrigger::Manual), 1000);
        pending
            .lock()
            .insert("rpc-compact".into(), PendingResponse::Compact(item));
        for frame in [
            json!({"type":"compaction_start","reason":"manual"}),
            json!({"type":"compaction_end","reason":"manual","result":{"summary":"Keep the public API stable","tokensBefore":120000,"estimatedTokensAfter":16000},"aborted":false}),
            json!({"type":"response","id":"rpc-compact","command":"compact","success":true,"data":{"summary":"Keep the public API stable","tokensBefore":120000,"estimatedTokensAfter":16000}}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        let seen = received.try_iter().collect::<Vec<_>>();
        assert!(
            seen.iter()
                .all(|event| matches!(event, DriverEvent::CompactionUpdated(_)))
        );
        let record = state.compaction.current.unwrap();
        assert_eq!(record.id, "manual");
        assert_eq!(record.status, CompactionStatus::Completed);
        assert_eq!(record.after_tokens, Some(16000));
        assert!(record.after_tokens_estimated);
        assert_eq!(
            record.summary.as_deref(),
            Some("Keep the public API stable")
        );
        assert!(matches!(
            command_rx.try_recv(),
            Ok(CommandMessage::RefreshContextUsage)
        ));
        assert!(command_rx.is_empty());
    }

    #[test]
    fn compaction_events_refresh_usage_without_generating_an_assistant_turn() {
        for manual in [false, true] {
            let (pending, commands, command_rx, mut state) = harness();
            let (events, received) = unbounded();
            if manual {
                pending.lock().insert(
                    "rpc-compact".into(),
                    PendingResponse::Compact(ContextCompaction::new(
                        "manual",
                        Some(CompactionTrigger::Manual),
                        1000,
                    )),
                );
            }
            // Compaction returns its instruction envelope, without the
            // tokensAfter count it writes into the native session file.
            let result = json!({
                "summary": "Resume prior conversation. Earlier turns archived under HISTORY below, oldest→newest.\n\nHISTORY\n===================",
                "shortSummary": "Archived 10,120 chars of history onto 0 snapcompact frames (+10,120 chars as text)",
                "firstKeptEntryId": "kept",
                "tokensBefore": 30113,
                "preserveData": {"snapcompact": {"frames": [], "text": "archived history"}}
            });
            for frame in [
                json!({"type": if manual { "manual_compaction_start" } else { "auto_compaction_start" }}),
                json!({"type": if manual { "manual_compaction_end" } else { "auto_compaction_end" }, "result": result, "aborted": false}),
            ] {
                handle_pi_message(
                    PiFlavor::Pi,
                    frame,
                    &pending,
                    &commands,
                    &events,
                    &mut state,
                );
            }
            if manual {
                assert!(
                    command_rx.is_empty(),
                    "refresh once the RPC response settles"
                );
                handle_pi_message(
                    PiFlavor::Pi,
                    json!({"type":"response", "id":"rpc-compact", "command":"compact", "success":true, "data":result}),
                    &pending,
                    &commands,
                    &events,
                    &mut state,
                );
            }
            let item = state.compaction.current.unwrap();
            assert_eq!(item.status, CompactionStatus::Completed);
            assert_eq!(
                item.trigger,
                Some(if manual {
                    CompactionTrigger::Manual
                } else {
                    CompactionTrigger::Auto
                })
            );
            assert_eq!(item.before_tokens, Some(30113));
            assert_eq!(item.after_tokens, None);
            assert!(!item.summary_for_display().unwrap().contains("HISTORY"));
            assert!(
                received
                    .try_iter()
                    .all(|event| matches!(event, DriverEvent::CompactionUpdated(_)))
            );
            assert!(matches!(
                command_rx.try_recv(),
                Ok(CommandMessage::RefreshContextUsage)
            ));
            assert!(command_rx.is_empty());
        }
    }

    #[test]
    fn cancelled_manual_compaction_response_preserves_interrupted_status() {
        let (pending, commands, command_rx, mut state) = harness();
        let (events, received) = unbounded();
        let item = ContextCompaction::new("manual", Some(CompactionTrigger::Manual), 1);
        state.compaction.seed(item.clone());
        pending
            .lock()
            .insert("rpc-compact".into(), PendingResponse::Compact(item));
        interrupt_pending_compactions(&pending, &events);
        handle_pi_message(
            PiFlavor::Pi,
            json!({"type":"manual_compaction_end", "result":{"summary":"A late result"}, "aborted":false}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        handle_pi_message(
            PiFlavor::Pi,
            json!({"type":"response","id":"rpc-compact","command":"compact","success":false,"error":"Compaction aborted"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        let records = received.try_iter().collect::<Vec<_>>();
        assert!(
            records
                .iter()
                .all(|event| matches!(event, DriverEvent::CompactionUpdated(item)
            if item.status == CompactionStatus::Interrupted && item.error.is_none()))
        );
        assert_eq!(
            state.compaction.current.as_ref().unwrap().status,
            CompactionStatus::Interrupted
        );
        assert!(command_rx.is_empty());
    }

    #[test]
    fn local_slash_command_output_and_completion_do_not_need_an_agent_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        send_prompt(&mut Vec::new(), &pending, &mut 0, "/context").unwrap();
        for frame in [
            json!({"type": "response", "id": "waku-1", "command": "prompt", "success": true}),
            json!({"type": "command_output", "text": "Available commands\n"}),
            json!({"type": "command_output", "text": "/compact"}),
            json!({"type": "response", "id": "waku-1", "command": "prompt", "success": true, "data": {"agentInvoked": false}}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::TextDelta(text) if text == "Available commands\n\n\n")
        );
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::TextDelta(text) if text == "/compact\n\n")
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(event_rx.try_recv().is_err());
        assert!(pending.lock().is_empty());
    }

    #[test]
    fn asynchronous_prompt_errors_settle_only_the_current_prompt() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        let mut next = 0;
        let mut wire = Vec::new();
        send_prompt(&mut wire, &pending, &mut next, "/old").unwrap();
        send_prompt(&mut wire, &pending, &mut next, "/new").unwrap();
        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "response", "id": "waku-1", "success": false, "error": "stale"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(event_rx.try_recv().is_err());
        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "response", "id": "waku-2", "success": false, "error": "command failed"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::Error(error) if error == "command failed")
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: false, .. }
        ));
        assert!(pending.lock().is_empty());
        assert_eq!(String::from_utf8(wire).unwrap().lines().count(), 2);
    }
    fn harness() -> (
        PendingResponses,
        Sender<CommandMessage>,
        crossbeam_channel::Receiver<CommandMessage>,
        PiStreamState,
    ) {
        let (commands, receiver) = unbounded();
        (
            Arc::new(Mutex::new(HashMap::new())),
            commands,
            receiver,
            PiStreamState::default(),
        )
    }

    /// Drives the installed Pi RPC through one real provider turn. Ignored by
    /// default because it needs the CLI, credentials, and network access.
    #[test]
    #[ignore = "requires an installed, authenticated pi"]
    fn pi_context_usage_against_the_real_rpc() {
        let binary = crate::command_env::find_executable("pi").expect("pi is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = PiDriver::start(
            DriverStartOptions {
                binary,
                cwd: std::env::temp_dir(),
                mode: RuntimeMode::FullAccess,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                execution_policy: Default::default(),
                agent_preset: None,
                provider_cursor: None,
                delegation_session_path: None,
                delegation_registry_dir: None,
            },
            events,
        )
        .expect("the Pi RPC session should start");

        let mut connected = false;
        let mut context_tokens = None;
        let mut context_window = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(30)) {
            match event {
                DriverEvent::Connected { .. } => {
                    connected = true;
                    break;
                }
                DriverEvent::Error(error) => panic!("Pi failed to initialize: {error}"),
                _ => {}
            }
        }
        assert!(connected, "Pi never reported its native session");

        driver.prompt("Reply with exactly: OK. Do not use any tools.".into());
        let mut finished = false;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(180)) {
            match event {
                DriverEvent::UsageUpdated {
                    context_tokens: tokens,
                    context_window: window,
                } => {
                    context_tokens = tokens.or(context_tokens);
                    context_window = window.or(context_window);
                }
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "Pi should finish the probe turn");
                    finished = true;
                    break;
                }
                DriverEvent::Error(error) => panic!("Pi reported: {error}"),
                _ => {}
            }
        }

        assert!(finished, "Pi never settled the probe turn");
        assert!(context_tokens.is_some_and(|tokens| tokens > 0));
        assert!(context_window.is_some_and(|window| window > 0));
    }

    #[test]
    fn model_and_thinking_changes_reach_the_running_session_but_mode_changes_do_not() {
        let (commands, command_rx) = unbounded();
        let driver = PiDriver {
            flavor: PiFlavor::Pi,
            commands,
        };
        let options = |mode| SessionOptions {
            mode,
            model: Some("anthropic/claude-opus-5".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            service_tier: None,
            context_window: None,
        };

        assert!(driver.apply_options(options(RuntimeMode::FullAccess)));
        assert!(matches!(
            command_rx.try_recv(),
            Ok(CommandMessage::Options(_))
        ));

        // Pi has no permission setter and only runs with Full access.
        assert!(!driver.apply_options(options(RuntimeMode::Ask)));
        assert!(command_rx.try_recv().is_err());
    }

    #[test]
    fn pi_fork_selects_the_first_removed_user_turn_or_clones_the_tip() {
        let messages = [
            json!({"entryId": "turn-1"}),
            json!({"entryId": "turn-2"}),
            json!({"entryId": "turn-3"}),
        ];
        assert_eq!(
            pi_fork_request(PiFlavor::Pi, &messages, 0).unwrap(),
            json!({"type": "clone"})
        );
        assert_eq!(
            pi_fork_request(PiFlavor::Pi, &messages, 2).unwrap(),
            json!({"type": "fork", "entryId": "turn-2"})
        );
        assert_eq!(
            pi_fork_request(PiFlavor::Pi, &messages, 3).unwrap(),
            json!({"type": "fork", "entryId": "turn-1"})
        );
        assert!(pi_fork_request(PiFlavor::Pi, &messages, 4).is_err());
    }

    #[test]
    fn streams_pi_text_reasoning_tools_and_settles_once() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "thinking_delta", "delta": "checking"}
            }),
            json!({
                "type": "tool_execution_start",
                "toolCallId": "tool-1",
                "toolName": "read",
                "args": {"path": "src/main.rs", "title": "Inspect Pi source"}
            }),
            json!({
                "type": "tool_execution_end",
                "toolCallId": "tool-1",
                "toolName": "read",
                "result": {"content": "..."},
                "isError": false
            }),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "text_delta", "delta": "done"}
            }),
            json!({"type": "agent_end", "willRetry": false}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                value,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::ReasoningDelta(value) if value == "checking"
        ));
        let DriverEvent::RichActivity(started) = event_rx.recv().unwrap() else {
            panic!("expected a rich Pi tool activity");
        };
        assert_eq!(started.title, "Inspect Pi source");
        assert_eq!(started.kind, ActivityKind::FileRead);
        assert_eq!(started.display_target.as_deref(), Some("src/main.rs"));
        assert!(
            started
                .arguments
                .as_deref()
                .is_some_and(|arguments| arguments.contains("src/main.rs"))
        );
        assert!(!started.complete);
        let DriverEvent::RichActivity(completed) = event_rx.recv().unwrap() else {
            panic!("expected a completed rich Pi tool activity");
        };
        assert!(
            completed
                .output
                .as_deref()
                .is_some_and(|output| output.contains("..."))
        );
        assert!(completed.complete);
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TextDelta(value) if value == "done"
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn pi_parked_and_pre_return_async_tool_updates_preserve_title_and_settlement() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();

        // Case 1: call parks on `tool_execution_end` with `async.state = running`
        // and settles on a later `tool_execution_update`.
        for value in [
            json!({"type": "turn_start"}),
            json!({
                "type": "tool_execution_start",
                "toolCallId": "tool-parked",
                "toolName": "bash",
                "args": {"command": "cargo test", "title": "Run tests"}
            }),
            json!({
                "type": "tool_execution_end",
                "toolCallId": "tool-parked",
                "toolName": "bash",
                "isError": false,
                "result": {
                    "content": [{"type": "text", "text": "Exit code: 0\nBackgrounded as job bg_1"}],
                    "details": {
                        "async": {"state": "running", "jobId": "bg_1", "type": "bash"}
                    }
                }
            }),
            json!({
                "type": "tool_execution_update",
                "toolCallId": "tool-parked",
                "toolName": "bash",
                "partialResult": {
                    "content": [{"type": "text", "text": "FAILED\nWall time: 44.30 seconds\nCommand exited with code 101"}],
                    "details": {
                        "async": {"state": "failed", "jobId": "bg_1", "type": "bash"}
                    }
                }
            }),
            // Case 2: async job settles in `tool_execution_update` before
            // `tool_execution_end` returns its initial `running` handle.
            json!({
                "type": "tool_execution_start",
                "toolCallId": "tool-fast",
                "toolName": "bash",
                "args": {"command": "cargo check", "title": "Check workspace"}
            }),
            json!({
                "type": "tool_execution_update",
                "toolCallId": "tool-fast",
                "toolName": "bash",
                "partialResult": {
                    "content": [{"type": "text", "text": "error\nCommand exited with code 1"}],
                    "details": {
                        "async": {"state": "failed", "jobId": "bg_2", "type": "bash"}
                    }
                }
            }),
            json!({
                "type": "tool_execution_end",
                "toolCallId": "tool-fast",
                "toolName": "bash",
                "isError": false,
                "result": {
                    "content": [{"type": "text", "text": "Backgrounded as job bg_2"}],
                    "details": {
                        "async": {"state": "running", "jobId": "bg_2", "type": "bash"}
                    }
                }
            }),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                value,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        let DriverEvent::RichActivity(started) = event_rx.recv().unwrap() else {
            panic!("expected started activity");
        };
        assert!(!started.complete);
        assert_eq!(started.title, "Run tests");

        let DriverEvent::RichActivity(parked) = event_rx.recv().unwrap() else {
            panic!("expected parked activity");
        };
        assert!(!parked.complete);
        assert_eq!(parked.title, "Run tests");

        let DriverEvent::RichActivity(settled) = event_rx.recv().unwrap() else {
            panic!("expected settled activity");
        };
        assert!(settled.complete);
        assert!(settled.failed);
        assert_eq!(settled.title, "Run tests");

        let DriverEvent::RichActivity(fast_started) = event_rx.recv().unwrap() else {
            panic!("expected fast started activity");
        };
        assert!(!fast_started.complete);
        assert_eq!(fast_started.title, "Check workspace");

        let DriverEvent::RichActivity(fast_update) = event_rx.recv().unwrap() else {
            panic!("expected fast settled update");
        };
        assert!(fast_update.complete);
        assert!(fast_update.failed);
        assert_eq!(fast_update.title, "Check workspace");

        let DriverEvent::RichActivity(fast_end) = event_rx.recv().unwrap() else {
            panic!("expected fast end activity");
        };
        assert!(fast_end.complete);
        assert!(fast_end.failed);
        assert_eq!(fast_end.title, "Check workspace");
        assert!(
            fast_end
                .output
                .as_deref()
                .is_some_and(|text| text.contains("Command exited with code 1"))
        );
    }

    #[test]
    fn session_name_changes_are_forwarded_as_automatic_titles() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();

        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "session_info_changed", "name": "Named by Pi"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Named by Pi"
        ));

        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "session_info_changed", "name": null}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::AutoTitleUpdated(None)
        ));
    }

    #[test]
    fn tool_only_intermediate_message_does_not_emit_empty_text() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "toolCall", "id": "tool-1", "name": "read"}]
                }
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn deepseek_tool_periods_do_not_split_reasoning_and_tool_activity() {
        // Shape recorded from a DeepSeek Flash message in Oh My Pi's native
        // session: thinking, a literal period, then toolCall, stopReason toolUse.
        let flavor = PiFlavor::Pi;
        for streamed in [false, true] {
                let (pending, commands, _command_rx, mut state) = harness();
                let (events, event_rx) = unbounded();
                let message = json!({
                    "role": "assistant", "provider": "deepseek", "model": "deepseek-flash",
                    "stopReason": "toolUse",
                    "content": [
                        {"type": "thinking", "thinking": "Inspect the project"},
                        {"type": "text", "text": " \n.\n"},
                        {"type": "toolCall", "id": "tool-1", "name": "read", "arguments": {"path": "README.md"}}
                    ]
                });
                let mut frames = vec![json!({"type": "message_start", "message": {
                    "role": "assistant", "provider": "deepseek", "model": "deepseek-flash", "content": []
                }})];
                if streamed {
                    frames.push(json!({"type": "message_update", "assistantMessageEvent": {
                        "type": "thinking_delta", "delta": "Inspect the project"
                    }}));
                    for delta in [" \n", ".", "\n"] {
                        frames.push(json!({"type": "message_update", "assistantMessageEvent": {
                            "type": "text_delta", "delta": delta
                        }}));
                    }
                }
                frames.extend([
                    json!({"type": "message_end", "message": message}),
                    json!({"type": "tool_execution_start", "toolCallId": "tool-1", "toolName": "read", "args": {"path": "README.md"}}),
                    json!({"type": "tool_execution_end", "toolCallId": "tool-1", "toolName": "read", "result": {"content": "Project instructions"}}),
                    json!({"type": "message_start", "message": {"role": "assistant", "provider": "deepseek"}}),
                    json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "Done."}}),
                    json!({"type": "message_end", "message": {"role": "assistant", "provider": "deepseek", "stopReason": "stop", "content": [{"type": "text", "text": "Done."}]}}),
                ]);
                for frame in frames {
                    handle_pi_message(flavor, frame, &pending, &commands, &events, &mut state);
                }
                let emitted = event_rx.try_iter().collect::<Vec<_>>();
                let tools = if streamed {
                    assert!(
                        matches!(&emitted[0], DriverEvent::ReasoningDelta(text) if text == "Inspect the project")
                    );
                    &emitted[1..]
                } else {
                    assert!(matches!(
                        &emitted[0],
                        DriverEvent::ReasoningStarted {
                            timestamp_ms: 0,
                            ..
                        }
                    ));
                    assert!(
                        matches!(&emitted[1], DriverEvent::ReasoningCompleted { content: Some(text), .. } if text == "Inspect the project")
                    );
                    &emitted[2..]
                };
                assert!(matches!(tools, [
                    DriverEvent::RichActivity(started),
                    DriverEvent::RichActivity(finished),
                    DriverEvent::TextDelta(text),
                ] if !started.complete && finished.complete && text == "Done."));
            }
    }

    #[test]
    fn deepseek_period_buffer_preserves_real_text_and_other_models() {
        for (provider, model, deltas, text, tool_use) in [
            (
                "deepseek",
                "deepseek-flash",
                vec![".", "gitignore"],
                ".gitignore",
                true,
            ),
            ("deepseek", "deepseek-flash", vec!["."], ".", false),
            (
                "deepseek",
                "deepseek-flash",
                vec![".", ".", "."],
                "...",
                true,
            ),
            ("other", "other-model", vec!["."], ".", true),
        ] {
            let (pending, commands, _command_rx, mut state) = harness();
            let (events, event_rx) = unbounded();
            handle_pi_message(
                PiFlavor::Pi,
                json!({"type": "message_start", "message": {"role": "assistant", "provider": provider, "model": model}}),
                &pending,
                &commands,
                &events,
                &mut state,
            );
            for delta in deltas {
                handle_pi_message(
                    PiFlavor::Pi,
                    json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": delta}}),
                    &pending,
                    &commands,
                    &events,
                    &mut state,
                );
            }
            if text != "." || provider != "deepseek" {
                assert!(
                    !event_rx.is_empty(),
                    "ordinary text must stream before message_end"
                );
            } else {
                assert!(
                    event_rx.is_empty(),
                    "a possible filler period must not flash in the transcript"
                );
            }
            let mut content = vec![json!({"type": "text", "text": text})];
            if tool_use {
                content.push(json!({"type": "toolCall", "id": "tool-1", "name": "read"}));
            }
            handle_pi_message(
                PiFlavor::Pi,
                json!({"type": "message_end", "message": {
                    "role": "assistant", "provider": provider, "model": model,
                    "stopReason": if tool_use { "toolUse" } else { "stop" }, "content": content
                }}),
                &pending,
                &commands,
                &events,
                &mut state,
            );
            let actual = event_rx
                .try_iter()
                .map(|event| match event {
                    DriverEvent::TextDelta(text) => text,
                    _ => panic!("unexpected event"),
                })
                .collect::<String>();
            assert_eq!(actual, text);
        }
    }

    #[test]
    fn speculative_deepseek_tools_wait_for_the_completed_text() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for frame in [
            json!({"type": "message_start", "message": {"role": "assistant", "provider": "deepseek"}}),
            json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "."}}),
            json!({"type": "tool_execution_start", "toolCallId": "tool-1", "toolName": "read"}),
            json!({"type": "message_end", "message": {
                "role": "assistant", "provider": "deepseek", "stopReason": "toolUse",
                "content": [{"type": "text", "text": "."}, {"type": "toolCall", "id": "tool-1", "name": "read"}]
            }}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::RichActivity(_)
        ));
        assert!(event_rx.is_empty());

        for frame in [
            json!({"type": "message_start", "message": {"role": "assistant", "provider": "deepseek"}}),
            json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "."}}),
            json!({"type": "tool_execution_start", "toolCallId": "tool-2", "toolName": "read"}),
            json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "gitignore"}}),
            json!({"type": "message_end", "message": {
                "role": "assistant", "provider": "deepseek", "stopReason": "toolUse",
                "content": [{"type": "text", "text": ".gitignore"}, {"type": "toolCall", "id": "tool-2", "name": "read"}]
            }}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::RichActivity(_)
        ));
        assert!(
            matches!(event_rx.try_recv().unwrap(), DriverEvent::TextDelta(text) if text == ".gitignore")
        );
        assert!(event_rx.is_empty());
    }

    #[test]
    fn completed_message_is_used_when_deltas_were_not_streamed() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "reason"},
                        {"type": "text", "text": "answer"}
                    ]
                }
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        let DriverEvent::ReasoningStarted {
            id,
            timestamp_ms: 0,
        } = event_rx.recv().unwrap()
        else {
            panic!("snapshot-only reasoning has an unknown start time");
        };
        assert!(matches!(event_rx.recv().unwrap(),
            DriverEvent::ReasoningCompleted { id: finished, content: Some(value), .. }
            if finished == id && value == "reason"));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TextDelta(value) if value == "answer"
        ));
    }

    #[test]
    fn indexed_thinking_preserves_native_blocks_and_authoritative_completions() {
        let flavor = PiFlavor::Pi;
            let (pending, commands, _command_rx, mut state) = harness();
            let (events, received) = unbounded();
            let first = "**Checking settings definition**\n\n**Checking default-path references**";
            let second = "**Implementing daemon monitoring**\n\n**Initializing daemon state**";
            for value in [
                json!({"type":"message_start","message":{"role":"assistant"}}),
                json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_start","contentIndex":0}}),
                json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"**Checking settings definition**"}}),
                json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_end","contentIndex":0,"content":first}}),
                json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_start","contentIndex":1}}),
                json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":1,"delta":second}}),
                json!({"type":"message_end","message":{"role":"assistant","content":[
                    {"type":"thinking","thinking":first},{"type":"thinking","thinking":second}
                ]}}),
            ] {
                handle_pi_message(flavor, value, &pending, &commands, &events, &mut state);
            }
            let emitted = received.try_iter().collect::<Vec<_>>();
            let [
                DriverEvent::ReasoningStarted {
                    id: a,
                    timestamp_ms: start,
                },
                DriverEvent::ReasoningBlockDelta { id: a_delta, .. },
                DriverEvent::ReasoningCompleted {
                    id: a_end,
                    timestamp_ms: end,
                    content: Some(a_text),
                },
                DriverEvent::ReasoningStarted { id: b, .. },
                DriverEvent::ReasoningBlockDelta { id: b_delta, .. },
                DriverEvent::ReasoningCompleted {
                    id: b_end,
                    content: Some(b_text),
                    ..
                },
            ] = &emitted[..]
            else {
                panic!("unexpected lifecycle: {emitted:?}")
            };
            assert_eq!(a, a_delta);
            assert_eq!(a, a_end);
            assert_ne!(a, b);
            assert_eq!(b, b_delta);
            assert_eq!(b, b_end);
            assert!(*start > 0 && end >= start);
            assert_eq!(a_text, first);
            assert_eq!(b_text, second);

            // contentIndex starts over in the next assistant message.
            for value in [
                json!({"type":"message_start","message":{"role":"assistant"}}),
                json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_start","contentIndex":0}}),
            ] {
                handle_pi_message(flavor, value, &pending, &commands, &events, &mut state);
            }
            assert!(
                matches!(received.recv().unwrap(), DriverEvent::ReasoningStarted { id, .. } if &id != a && &id != b)
            );
    }

    #[test]
    fn context_usage_uses_pi_components_when_total_is_zero() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [],
                    "usage": {
                        "input": 33,
                        "output": 27,
                        "cacheRead": 5888,
                        "cacheWrite": 4,
                        "totalTokens": 0
                    }
                }
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::UsageUpdated {
                context_tokens: Some(5952),
                context_window: None
            }
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn session_stats_supply_pi_context_tokens_and_window() {
        let state = json!({"data": {"model": {"contextWindow": 200_000}}});
        let stats = json!({
            "data": {
                "contextUsage": {
                    "tokens": 6109,
                    "contextWindow": 1_000_000,
                    "percent": 0.6109
                }
            }
        });

        assert_eq!(
            pi_context_usage(&state, Some(&stats)),
            Some((Some(6109), Some(1_000_000)))
        );
        assert_eq!(pi_context_usage(&state, None), Some((None, Some(200_000))));
    }

    #[test]
    fn recoverable_tool_error_does_not_fail_the_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({
                "type": "tool_execution_end",
                "toolCallId": "tool-1",
                "toolName": "read",
                "result": {"error": "missing"},
                "isError": true
            }),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                value,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        let DriverEvent::RichActivity(completed) = event_rx.recv().unwrap() else {
            panic!("expected a completed rich Pi tool activity");
        };
        assert!(completed.failed);
        assert!(
            completed
                .output
                .as_deref()
                .is_some_and(|output| output.contains("missing"))
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
    }

    #[test]
    fn successful_auto_retry_recovers_the_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "error", "error": "temporary"}
            }),
            json!({"type": "auto_retry_end", "success": true}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                value,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::Error(_)));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
    }
}

#[cfg(test)]
mod delegation_tests {
    use super::*;

    fn command() -> std::process::Command {
        std::process::Command::new("pi")
    }

    fn args(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn env(command: &std::process::Command, key: &str) -> Option<String> {
        command
            .get_envs()
            .find(|(name, _)| *name == key)
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned())
    }

    #[test]
    fn a_session_with_targets_gets_the_delegation_extension() {
        let mut command = command();
        configure_pi_delegation_command(
            &mut command,
            PiFlavor::Pi,
            Some(Path::new("/tmp/registration.json")),
            || Ok(PathBuf::from("/bundle/delegation/pi-extension.ts")),
        );

        let args = args(&command);
        let flag = args.iter().position(|arg| arg == "--extension").unwrap();
        assert_eq!(args[flag + 1], "/bundle/delegation/pi-extension.ts");
        assert!(!args.iter().any(|arg| arg == "--tools"));
        assert_eq!(
            env(&command, "WAKU_DELEGATION_SESSION_PATH").as_deref(),
            Some("/tmp/registration.json")
        );
        // The bridge executable must be the daemon that owns this task, never
        // an arbitrary path from the model.
        assert_eq!(
            env(&command, "WAKU_AGENT_TOOLS_SERVER").as_deref(),
            std::env::current_exe()
                .ok()
                .as_deref()
                .map(|path| path.to_string_lossy())
                .as_deref()
        );
    }

    #[test]
    fn a_session_without_targets_spawns_no_extension() {
        let mut command = command();
        configure_pi_delegation_command(&mut command, PiFlavor::Pi, None, || {
            panic!("the extension must not be probed without a registration")
        });
        assert!(args(&command).is_empty());
        assert!(env(&command, "WAKU_AGENT_TOOLS_SERVER").is_none());
    }

    /// The subtask launch must not carry the full-access flag: `--approve` /
    /// `--yolo` would auto-approve the writes the allowlist exists to prevent.
    #[test]
    fn a_review_subtask_is_launched_with_a_read_only_tool_allowlist() {
        let _flavor = PiFlavor::Pi;
        let mut command = command();
        command.args(review_read_only_args());
        let args = args(&command);
        let tools = args
            .iter()
            .position(|arg| arg == "--tools")
            .map(|index| args[index + 1].clone())
            .expect("a review subtask must carry a tool allowlist");
        for denied in ["bash", "edit", "write", "python", "notebook", "task"] {
            assert!(!tools.split(',').any(|tool| tool == denied), "{denied}");
        }
        for allowed in ["read", "grep", "glob"] {
            assert!(tools.split(',').any(|tool| tool == allowed), "{allowed}");
        }
        assert!(!args.iter().any(|arg| arg == "--approve" || arg == "--yolo"));
    }

    #[test]
    fn a_missing_extension_leaves_the_task_runnable() {
        let mut command = command();
        // A build without the asset still launches the agent; it simply cannot
        // delegate, which is what the eprintln! records.
        configure_pi_delegation_command(
            &mut command,
            PiFlavor::Pi,
            Some(Path::new("/tmp/registration.json")),
            || anyhow::bail!("asset missing"),
        );
        assert!(args(&command).is_empty());
    }
}
