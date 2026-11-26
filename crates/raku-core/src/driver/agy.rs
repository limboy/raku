//! Antigravity CLI (`agy acp`) stdio JSON-RPC transport.

use std::cell::Cell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use base64::Engine as _;
use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};
use parking_lot::Mutex;
use raku_protocol::capabilities::RuntimeCapabilities;
use raku_protocol::context_inspection::ContextDetail;
use serde_json::{Value, json};

use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{
    ActivityItem, ActivityKind, DriverEvent, PermissionOption, PlanStep, PlanStepStatus,
    ProviderKind, ProviderResumeCursor, ReportedCommand, RuntimeMode, unix_time_millis,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const CONFIG_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const EFFORT_ORDER: &[&str] = &["low", "medium", "high", "xhigh", "max"];

enum CommandMessage {
    Prompt(String),
    Cancel,
    Respond {
        request_id: String,
        option_id: String,
    },
    RespondPermissionRaw {
        raw_id: Value,
        result: Value,
    },
    RespondErrorRaw {
        raw_id: Value,
        code: i64,
        message: String,
    },
    Options(SessionOptions),
    ContextBreakdown(ContextDetail),
    Shutdown,
}

#[derive(Clone, Copy)]
enum RpcErrorKey {
    Initialize,
    OpenSession,
    SwitchModel,
    ChangeThinking,
}

impl RpcErrorKey {
    fn format(self, message: &str) -> String {
        let provider = ProviderKind::Antigravity.display_name();
        match self {
            Self::Initialize => {
                tr!(
                    "errors.initialize_provider",
                    provider = provider,
                    error = message
                )
            }
            Self::OpenSession => {
                tr!(
                    "errors.open_provider_session",
                    provider = provider,
                    error = message
                )
            }
            Self::SwitchModel => {
                tr!(
                    "errors.switch_provider_model",
                    provider = provider,
                    error = message
                )
            }
            Self::ChangeThinking => {
                tr!(
                    "errors.change_provider_thinking",
                    provider = provider,
                    error = message
                )
            }
        }
    }
}

#[derive(Clone, Debug)]
struct AcpPermissionOption {
    option_id: String,
    name: String,
    kind: String,
}

#[derive(Default)]
struct AcpToolCallState {
    tool_call_id: String,
    name: String,
    title: String,
    kind: String,
    status: Option<String>,
    args: Option<Value>,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
    content: Option<Value>,
}

#[derive(Default)]
struct AgyRuntimeState {
    session_id: Option<String>,
    thought_levels: Vec<String>,
    current_model: Option<String>,
    current_effort: Option<String>,
    plan_mode: bool,
    base_mode: RuntimeMode,
    active_prompt_id: Option<u64>,
    pending_rpc: HashMap<u64, Sender<Value>>,
    pending_permissions: HashMap<String, (Value, Vec<AcpPermissionOption>)>,
    last_usage: Option<(u64, u64)>,
    last_turn_usage: Option<Value>,
}

#[derive(Default)]
struct AgyStreamState {
    active_reasoning_id: Option<String>,
    /// The turn produced agy's plan artifact (`ArtifactMetadata.RequestFeedback`),
    /// the one signal that a plan-mode turn proposed a plan. Ordinary text in
    /// plan mode, such as a clarifying question, is not a plan.
    saw_plan_artifact: bool,
    tool_calls: HashMap<String, AcpToolCallState>,
}

impl AgyStreamState {
    fn open_reasoning(&mut self, events: &impl DriverEventSink) -> String {
        if let Some(id) = &self.active_reasoning_id {
            return id.clone();
        }
        let id = format!("agy-reasoning-{}", uuid::Uuid::new_v4());
        self.active_reasoning_id = Some(id.clone());
        let _ = events.send(DriverEvent::ReasoningStarted {
            id: id.clone(),
            timestamp_ms: unix_time_millis(),
        });
        id
    }

    fn append_reasoning(&mut self, text: &str, events: &impl DriverEventSink) {
        if text.is_empty() {
            return;
        }
        let id = self.open_reasoning(events);
        let _ = events.send(DriverEvent::ReasoningBlockDelta {
            id,
            delta: text.to_owned(),
        });
    }

    fn close_reasoning(&mut self, events: &impl DriverEventSink) {
        if let Some(id) = self.active_reasoning_id.take() {
            let _ = events.send(DriverEvent::ReasoningCompleted {
                id,
                timestamp_ms: unix_time_millis(),
                content: None,
            });
        }
    }

    fn reset_turn(&mut self, events: &impl DriverEventSink) {
        self.close_reasoning(events);
        self.saw_plan_artifact = false;
        self.tool_calls.clear();
    }
}

pub struct AgyDriver {
    commands: Sender<CommandMessage>,
    shutdown: Sender<()>,
    mode: RuntimeMode,
}

impl AgyDriver {
    pub fn start(options: DriverStartOptions, events: DriverEventSender) -> anyhow::Result<Self> {
        let DriverStartOptions {
            binary,
            cwd,
            mode,
            plan_mode,
            model,
            reasoning_effort,
            service_tier: _,
            context_window: _,
            agent_preset: _,
            provider_cursor,
            read_only,
            subagent,
        } = options;

        let caps = ProviderKind::Antigravity.capabilities();
        if read_only && !caps.side_chat {
            return Err(anyhow!(tr!(
                "errors.provider_side_chat_unsupported",
                provider = ProviderKind::Antigravity.display_name()
            )));
        }
        if subagent.child && !caps.subagent_target {
            return Err(anyhow!(tr!(
                "errors.provider_subagent_unsupported",
                provider = ProviderKind::Antigravity.display_name()
            )));
        }

        let resume_session_id = match provider_cursor {
            Some(ProviderResumeCursor::Antigravity { session_id }) => Some(session_id),
            Some(cursor) => {
                return Err(anyhow!(
                    "cannot resume Antigravity from a {} cursor",
                    cursor.provider().display_name()
                ));
            }
            None => None,
        };

        let instructions = super::system_instructions(read_only, &subagent);
        let mut command = crate::command_env::command(&binary);
        command.arg("acp");
        let manifest = super::context_inspection::launch_manifest(
            &command,
            instructions,
            Vec::new(),
            Vec::new(),
        );
        let command = command
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::command_env::spawn(command)
            .with_context(|| format!("failed to start `{}` acp", binary.display()))?;
        let _ = events.send(DriverEvent::LaunchManifest(Box::new(manifest)));

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Antigravity stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Antigravity stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Antigravity stderr unavailable"))?;

        let (commands, command_rx) = unbounded::<CommandMessage>();
        let (shutdown, shutdown_rx) = bounded::<()>(1);
        // Reader-originated replies (permission auto-answers, method-not-found,
        // auth shutdown) travel on their own channel so the writer can answer
        // them while it blocks on a handshake or config RPC.
        let (reader_commands, urgent_rx) = unbounded::<CommandMessage>();
        let state = Arc::new(Mutex::new(AgyRuntimeState {
            base_mode: mode,
            plan_mode,
            ..Default::default()
        }));
        let suppress_replay = Arc::new(AtomicBool::new(false));

        let reader_state = state.clone();
        let reader_suppress_replay = suppress_replay.clone();
        let reader_events = events.clone();
        let subagent_child = subagent.child;
        let reader_thread = thread::Builder::new()
            .name("raku-agy-reader".into())
            .spawn(move || {
                let mut stream = AgyStreamState::default();
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(line) if !line.trim().is_empty() => {
                            match serde_json::from_str::<Value>(&line) {
                                Ok(value) => handle_acp_message(
                                    value,
                                    &reader_state,
                                    &reader_suppress_replay,
                                    &reader_commands,
                                    &reader_events,
                                    &mut stream,
                                    mode,
                                    subagent_child,
                                ),
                                Err(error) => {
                                    let _ = reader_events.send(DriverEvent::Error(tr!(
                                        "errors.provider_invalid_json",
                                        provider = ProviderKind::Antigravity.display_name(),
                                        error = error
                                    )));
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let _ = reader_events.send(DriverEvent::Error(tr!(
                                "errors.provider_transport_read",
                                provider = ProviderKind::Antigravity.display_name(),
                                error = error
                            )));
                            break;
                        }
                    }
                }
                // Drop every pending RPC sender so an in-flight handshake or
                // config call wakes immediately on stdout EOF instead of
                // sleeping out its timeout after the child has exited.
                reader_state.lock().pending_rpc.clear();
                stream.reset_turn(&reader_events);
            })?;

        let writer_state = state.clone();
        let writer_suppress_replay = suppress_replay;
        let writer_events = events.clone();
        let cwd_string = cwd.display().to_string();
        let instructions_owned = instructions.to_owned();
        thread::Builder::new()
            .name("raku-agy-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                let mut next_request_id: u64 = 0;
                let signals = WriterSignals {
                    shutdown: shutdown_rx,
                    urgent: urgent_rx,
                    stop: Cell::new(false),
                };

                if !run_handshake(
                    &mut stdin,
                    &mut next_request_id,
                    &writer_state,
                    &writer_suppress_replay,
                    &signals,
                    &writer_events,
                    &cwd_string,
                    &instructions_owned,
                    resume_session_id.as_deref(),
                    model.as_deref(),
                    reasoning_effort.as_deref(),
                    mode,
                    plan_mode,
                ) {
                    drop(stdin);
                    return;
                }

                let never = crossbeam_channel::never::<CommandMessage>();
                let mut urgent_open = true;
                loop {
                    if signals.stop.get() {
                        break;
                    }
                    let urgent = if urgent_open { &signals.urgent } else { &never };
                    let command = select! {
                        recv(command_rx) -> msg => match msg {
                            Ok(command) => command,
                            Err(_) => break,
                        },
                        recv(urgent) -> msg => match msg {
                            Ok(command) => command,
                            // The reader exited; keep serving driver commands
                            // until the driver drops.
                            Err(_) => {
                                urgent_open = false;
                                continue;
                            }
                        },
                    };
                    match command {
                        CommandMessage::Prompt(text) => {
                            let Some(session_id) = writer_state.lock().session_id.clone() else {
                                let _ = writer_events.send(DriverEvent::Error(tr!(
                                    "errors.provider_initialize_session",
                                    provider = ProviderKind::Antigravity.display_name()
                                )));
                                continue;
                            };
                            next_request_id += 1;
                            let request_id = next_request_id;
                            writer_state.lock().active_prompt_id = Some(request_id);
                            let _ = writer_events.send(DriverEvent::TurnStarted);
                            let blocks = build_prompt_blocks(&text, &cwd);
                            let message = json!({
                                "jsonrpc": "2.0",
                                "id": request_id,
                                "method": "session/prompt",
                                "params": {
                                    "sessionId": session_id,
                                    "prompt": blocks,
                                }
                            });
                            if let Err(error) = write_json_line(&mut stdin, &message) {
                                writer_state.lock().active_prompt_id = None;
                                let _ = writer_events.send(DriverEvent::Error(tr!(
                                    "errors.provider_transport_write",
                                    provider = ProviderKind::Antigravity.display_name(),
                                    error = error
                                )));
                                let _ = writer_events.send(DriverEvent::TurnFinished {
                                    success: false,
                                    summary: Some(tr!(
                                        "errors.provider_rejected_prompt",
                                        provider = ProviderKind::Antigravity.display_name()
                                    )),
                                });
                                break;
                            }
                        }
                        CommandMessage::Cancel => {
                            let (session_id, cancelled_perms) = {
                                let mut guard = writer_state.lock();
                                let drained = guard.pending_permissions.drain().collect::<Vec<_>>();
                                (guard.session_id.clone(), drained)
                            };
                            // ACP order: `session/cancel` first, then answer every
                            // open permission request with `cancelled`. Answering
                            // first reads as a refused tool, and the agent may carry
                            // on with the turn before the cancel arrives.
                            if let Some(session_id) = session_id {
                                let notif = json!({
                                    "jsonrpc": "2.0",
                                    "method": "session/cancel",
                                    "params": { "sessionId": session_id }
                                });
                                if let Err(error) = write_json_line(&mut stdin, &notif) {
                                    let _ = writer_events.send(DriverEvent::Error(tr!(
                                        "errors.stop_provider",
                                        provider = ProviderKind::Antigravity.display_name(),
                                        error = error
                                    )));
                                }
                            }
                            for (_, (raw_id, _)) in cancelled_perms {
                                let reply = json!({
                                    "jsonrpc": "2.0",
                                    "id": raw_id,
                                    "result": { "outcome": { "outcome": "cancelled" } }
                                });
                                let _ = write_json_line(&mut stdin, &reply);
                            }
                        }
                        CommandMessage::Respond {
                            request_id,
                            option_id,
                        } => {
                            let entry = writer_state.lock().pending_permissions.remove(&request_id);
                            let Some((raw_id, _)) = entry else {
                                continue;
                            };
                            let reply = json!({
                                "jsonrpc": "2.0",
                                "id": raw_id,
                                "result": {
                                    "outcome": {
                                        "outcome": "selected",
                                        "optionId": option_id,
                                    }
                                }
                            });
                            if let Err(error) = write_json_line(&mut stdin, &reply) {
                                let _ = writer_events.send(DriverEvent::Error(tr!(
                                    "errors.answer_provider_permission",
                                    provider = ProviderKind::Antigravity.display_name(),
                                    error = error
                                )));
                                break;
                            }
                        }
                        command @ CommandMessage::RespondPermissionRaw { .. } => {
                            if let Err(error) = write_raw_reply(&mut stdin, command) {
                                let _ = writer_events.send(DriverEvent::Error(tr!(
                                    "errors.answer_provider_permission",
                                    provider = ProviderKind::Antigravity.display_name(),
                                    error = error
                                )));
                                break;
                            }
                        }
                        command @ CommandMessage::RespondErrorRaw { .. } => {
                            if write_raw_reply(&mut stdin, command).is_err() {
                                break;
                            }
                        }
                        CommandMessage::Options(options) => {
                            apply_session_options_rpc(
                                &mut stdin,
                                &mut next_request_id,
                                &writer_state,
                                &signals,
                                &writer_events,
                                &options,
                            );
                        }
                        CommandMessage::ContextBreakdown(detail) => {
                            let (model, last_usage, turn_usage) = {
                                let guard = writer_state.lock();
                                (
                                    guard.current_model.clone(),
                                    guard.last_usage,
                                    guard.last_turn_usage.clone(),
                                )
                            };
                            match super::context_inspection::agy_breakdown(
                                detail,
                                model.as_deref(),
                                last_usage,
                                turn_usage.as_ref(),
                            ) {
                                Some(breakdown) => {
                                    let _ = writer_events.send(
                                        DriverEvent::ContextBreakdownUpdated(Box::new(breakdown)),
                                    );
                                }
                                None => {
                                    let _ =
                                        writer_events.send(DriverEvent::ContextBreakdownFailed(
                                            tr!("context_inspector.fetch_failed"),
                                        ));
                                }
                            }
                        }
                        CommandMessage::Shutdown => break,
                    }
                }
                drop(stdin);
            })?;

        let stderr_lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let stderr_capture = stderr_lines.clone();
        let stderr_thread = thread::Builder::new()
            .name("raku-agy-stderr".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut guard = stderr_capture.lock();
                    if guard.len() < 128 {
                        guard.push(line);
                    }
                }
            })?;

        thread::Builder::new()
            .name("raku-agy-process".into())
            .spawn(move || {
                let status = child.wait();
                let _ = reader_thread.join();
                let _ = stderr_thread.join();
                match status {
                    Ok(status) if !status.success() => {
                        let lines = stderr_lines.lock().clone();
                        let message =
                            super::support::provider_stderr_error(lines).unwrap_or_else(|| {
                                tr!(
                                    "errors.provider_exited",
                                    provider = ProviderKind::Antigravity.display_name(),
                                    status = status
                                )
                            });
                        let _ = events.send(DriverEvent::Error(message));
                    }
                    Err(error) => {
                        let _ = events.send(DriverEvent::Error(tr!(
                            "errors.read_provider_exit_status",
                            provider = ProviderKind::Antigravity.display_name(),
                            error = error
                        )));
                    }
                    _ => {}
                }
                let _ = events.send(DriverEvent::ProcessExited);
            })?;

        Ok(Self {
            commands,
            shutdown,
            mode,
        })
    }
}

impl Drop for AgyDriver {
    fn drop(&mut self) {
        let _ = self.shutdown.try_send(());
        let _ = self.commands.send(CommandMessage::Shutdown);
    }
}

impl DriverControl for AgyDriver {
    fn provider(&self) -> ProviderKind {
        ProviderKind::Antigravity
    }

    fn prompt(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Prompt(prompt));
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities { steer: false }
    }

    fn cancel(&self) {
        let _ = self.commands.send(CommandMessage::Cancel);
    }

    fn respond(&self, request_id: String, option_id: String) {
        let _ = self.commands.send(CommandMessage::Respond {
            request_id,
            option_id,
        });
    }

    fn context_breakdown(&self, detail: ContextDetail) {
        let _ = self.commands.send(CommandMessage::ContextBreakdown(detail));
    }

    fn apply_options(&self, options: SessionOptions) -> bool {
        if options.mode != self.mode {
            return false;
        }
        let _ = self.commands.send(CommandMessage::Options(options));
        true
    }

    fn rollback(&self, _turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        anyhow::bail!(tr!(
            "errors.provider_turn_branching_unsupported",
            provider = ProviderKind::Antigravity.display_name()
        ))
    }
}

/// What a blocking writer-side RPC must keep listening to while it waits.
struct WriterSignals {
    /// Fires (or disconnects) when the driver is dropped.
    shutdown: Receiver<()>,
    /// Reader-originated commands: raw replies the server is waiting on, and
    /// `Shutdown` after an auth failure.
    urgent: Receiver<CommandMessage>,
    /// Set when a blocking RPC consumed an urgent `Shutdown`, so the writer
    /// loop stops once that RPC returns.
    stop: Cell<bool>,
}

/// Writes a reader-originated reply to a server-to-client request. Other
/// commands are ignored; only the reader sends these two variants.
fn write_raw_reply(stdin: &mut impl Write, command: CommandMessage) -> std::io::Result<()> {
    let reply = match command {
        CommandMessage::RespondPermissionRaw { raw_id, result } => json!({
            "jsonrpc": "2.0",
            "id": raw_id,
            "result": result,
        }),
        CommandMessage::RespondErrorRaw {
            raw_id,
            code,
            message,
        } => json!({
            "jsonrpc": "2.0",
            "id": raw_id,
            "error": { "code": code, "message": message },
        }),
        _ => return Ok(()),
    };
    write_json_line(stdin, &reply)
}

enum RpcCallError {
    /// Reader exited (stdout EOF) or driver was dropped; the reaper thread
    /// owns exit diagnostics.
    Disconnected,
    Failed(String),
}

#[allow(clippy::too_many_arguments)]
fn run_handshake(
    stdin: &mut impl Write,
    next_request_id: &mut u64,
    state: &Mutex<AgyRuntimeState>,
    suppress_replay: &AtomicBool,
    signals: &WriterSignals,
    events: &impl DriverEventSink,
    cwd: &str,
    instructions: &str,
    resume_session_id: Option<&str>,
    requested_model: Option<&str>,
    requested_effort: Option<&str>,
    mode: RuntimeMode,
    plan_mode: bool,
) -> bool {
    let init_params = json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false,
        },
        "clientInfo": {
            "name": "raku",
            "version": env!("CARGO_PKG_VERSION"),
        }
    });
    let init_resp = match call_rpc_sync(
        stdin,
        next_request_id,
        state,
        signals,
        "initialize",
        init_params,
        HANDSHAKE_TIMEOUT,
    ) {
        Ok(resp) => resp,
        Err(RpcCallError::Disconnected) => return false,
        Err(RpcCallError::Failed(error)) => {
            let _ = events.send(DriverEvent::Error(tr!(
                "errors.initialize_provider",
                provider = ProviderKind::Antigravity.display_name(),
                error = error
            )));
            return false;
        }
    };
    if let Some(error) = init_resp.get("error") {
        emit_rpc_error(error, RpcErrorKey::Initialize, events);
        return false;
    }

    // Every open carries Raku's instructions. A fresh `agy acp` process
    // rebuilds the system prompt per session, so a resume or load without the
    // append would drop the run-block and follow-up conventions.
    let meta = json!({
        "antigravity": {
            "systemPrompt": {
                "append": instructions,
            }
        }
    });
    let session_resp = if let Some(session_id) = resume_session_id {
        let resume_params = json!({
            "sessionId": session_id,
            "cwd": cwd,
            "mcpServers": [],
            "_meta": meta,
        });
        match call_rpc_sync(
            stdin,
            next_request_id,
            state,
            signals,
            "session/resume",
            resume_params,
            HANDSHAKE_TIMEOUT,
        ) {
            Ok(resp) if resp.get("error").is_none() => resp,
            Ok(resp) if resp.get("error").is_some_and(is_auth_error) => {
                let _ = events.send(DriverEvent::Error(tr!("errors.agy_auth_required")));
                return false;
            }
            Err(RpcCallError::Disconnected) => return false,
            _ => {
                suppress_replay.store(true, Ordering::Release);
                let load_params = json!({
                    "sessionId": session_id,
                    "cwd": cwd,
                    "mcpServers": [],
                    "_meta": meta,
                });
                let loaded = call_rpc_sync(
                    stdin,
                    next_request_id,
                    state,
                    signals,
                    "session/load",
                    load_params,
                    HANDSHAKE_TIMEOUT,
                );
                suppress_replay.store(false, Ordering::Release);
                match loaded {
                    Ok(resp) => resp,
                    Err(RpcCallError::Disconnected) => return false,
                    Err(RpcCallError::Failed(error)) => {
                        let _ = events.send(DriverEvent::Error(tr!(
                            "errors.open_provider_session",
                            provider = ProviderKind::Antigravity.display_name(),
                            error = error
                        )));
                        return false;
                    }
                }
            }
        }
    } else {
        let new_params = json!({
            "cwd": cwd,
            "mcpServers": [],
            "_meta": meta,
        });
        match call_rpc_sync(
            stdin,
            next_request_id,
            state,
            signals,
            "session/new",
            new_params,
            HANDSHAKE_TIMEOUT,
        ) {
            Ok(resp) => resp,
            Err(RpcCallError::Disconnected) => return false,
            Err(RpcCallError::Failed(error)) => {
                let _ = events.send(DriverEvent::Error(tr!(
                    "errors.open_provider_session",
                    provider = ProviderKind::Antigravity.display_name(),
                    error = error
                )));
                return false;
            }
        }
    };

    if let Some(error) = session_resp.get("error") {
        if is_auth_error(error) {
            let _ = events.send(DriverEvent::Error(tr!("errors.agy_auth_required")));
        } else {
            emit_rpc_error(error, RpcErrorKey::OpenSession, events);
        }
        return false;
    }

    let result = session_resp.get("result").unwrap_or(&Value::Null);
    let session_id = result
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .or_else(|| resume_session_id.map(str::to_owned));
    let Some(session_id) = session_id else {
        let _ = events.send(DriverEvent::Error(tr!(
            "errors.provider_no_session_id",
            provider = ProviderKind::Antigravity.display_name()
        )));
        return false;
    };

    #[cfg(not(test))]
    crate::model_catalog::reconcile_agy_session_models(result);

    let extracted = extract_config_state(result);
    {
        let mut guard = state.lock();
        guard.session_id = Some(session_id.clone());
        apply_extracted_config(&mut guard, extracted);
        guard.base_mode = mode;
        guard.plan_mode = plan_mode;
    }

    // Apply requested model -> clamped effort -> effective mode. A saved id
    // the session no longer offers (retired, or an effort-suffixed id the
    // catalog has since collapsed) must not make the thread unopenable: map
    // it to its base model, or stay on the session's own model.
    let offered_models = offered_model_ids(result);
    let resolved_model =
        requested_model.and_then(|model| resolve_requested_model(model, &offered_models));
    let requested_effort = requested_effort.or(resolved_model.and_then(|(_, effort)| effort));
    if let Some((requested_model, _)) = resolved_model
        && state.lock().current_model.as_deref() != Some(requested_model)
        && !set_config_option_sync(
            stdin,
            next_request_id,
            state,
            signals,
            events,
            &session_id,
            "model",
            requested_model,
            RpcErrorKey::SwitchModel,
            true,
        )
    {
        return false;
    }

    let target_effort = {
        let guard = state.lock();
        clamp_thought_level(
            requested_effort,
            &guard.thought_levels,
            guard.current_effort.as_deref(),
        )
    };
    if let Some(effort) = target_effort
        && state.lock().current_effort.as_deref() != Some(effort.as_str())
    {
        // A rejected clamped effort falls back to the model's own default
        // without poisoning the connecting turn with a pre-connect error.
        let _ = set_config_option_sync(
            stdin,
            next_request_id,
            state,
            signals,
            events,
            &session_id,
            "thought_level",
            &effort,
            RpcErrorKey::ChangeThinking,
            false,
        );
    }

    // A resumed or loaded session keeps the mode it was last in, so never
    // assume `default`: set the mode unless the response already reports it.
    // A failure here aborts the start rather than leaving the session in a
    // looser mode than the user chose.
    let target_mode = effective_acp_mode(mode, plan_mode);
    if extract_current_mode(result).as_deref() != Some(target_mode)
        && !set_config_option_sync(
            stdin,
            next_request_id,
            state,
            signals,
            events,
            &session_id,
            "mode",
            target_mode,
            RpcErrorKey::OpenSession,
            true,
        )
    {
        return false;
    }

    let _ = events.send(DriverEvent::Connected {
        provider_cursor: Some(ProviderResumeCursor::Antigravity { session_id }),
    });
    true
}

fn apply_session_options_rpc(
    stdin: &mut impl Write,
    next_request_id: &mut u64,
    state: &Mutex<AgyRuntimeState>,
    signals: &WriterSignals,
    events: &impl DriverEventSink,
    options: &SessionOptions,
) {
    let (session_id, prev_model, prev_effort, prev_plan_mode, base_mode) = {
        let guard = state.lock();
        let Some(session_id) = guard.session_id.clone() else {
            return;
        };
        (
            session_id,
            guard.current_model.clone(),
            guard.current_effort.clone(),
            guard.plan_mode,
            guard.base_mode,
        )
    };

    let model_changed = options.model.is_some() && options.model != prev_model;
    if model_changed && let Some(model) = options.model.as_deref() {
        set_config_option_sync(
            stdin,
            next_request_id,
            state,
            signals,
            events,
            &session_id,
            "model",
            model,
            RpcErrorKey::SwitchModel,
            true,
        );
    }

    let target_effort = {
        let guard = state.lock();
        clamp_thought_level(
            options.reasoning_effort.as_deref(),
            &guard.thought_levels,
            guard.current_effort.as_deref(),
        )
    };
    if let Some(effort) = target_effort
        && (model_changed || prev_effort.as_deref() != Some(effort.as_str()))
        && state.lock().current_effort.as_deref() != Some(effort.as_str())
    {
        set_config_option_sync(
            stdin,
            next_request_id,
            state,
            signals,
            events,
            &session_id,
            "thought_level",
            &effort,
            RpcErrorKey::ChangeThinking,
            true,
        );
    }

    if options.plan_mode != prev_plan_mode {
        let target_mode = effective_acp_mode(base_mode, options.plan_mode);
        if set_config_option_sync(
            stdin,
            next_request_id,
            state,
            signals,
            events,
            &session_id,
            "mode",
            target_mode,
            RpcErrorKey::OpenSession,
            true,
        ) {
            state.lock().plan_mode = options.plan_mode;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn set_config_option_sync(
    stdin: &mut impl Write,
    next_request_id: &mut u64,
    state: &Mutex<AgyRuntimeState>,
    signals: &WriterSignals,
    events: &impl DriverEventSink,
    session_id: &str,
    config_id: &str,
    value: &str,
    error_key: RpcErrorKey,
    emit_error: bool,
) -> bool {
    let params = json!({
        "sessionId": session_id,
        "configId": config_id,
        "value": value,
    });
    let resp = match call_rpc_sync(
        stdin,
        next_request_id,
        state,
        signals,
        "session/set_config_option",
        params,
        CONFIG_TIMEOUT,
    ) {
        Ok(resp) => resp,
        Err(RpcCallError::Disconnected) => return false,
        Err(RpcCallError::Failed(error)) => {
            if emit_error {
                let _ = events.send(DriverEvent::Error(error_key.format(&error)));
            }
            return false;
        }
    };
    if let Some(error) = resp.get("error") {
        if emit_error {
            if is_auth_error(error) {
                let _ = events.send(DriverEvent::Error(tr!("errors.agy_auth_required")));
            } else {
                emit_rpc_error(error, error_key, events);
            }
        }
        return false;
    }
    if let Some(result) = resp.get("result") {
        let extracted = extract_config_state(result);
        let mut guard = state.lock();
        apply_extracted_config(&mut guard, extracted);
    }
    true
}

fn call_rpc_sync(
    stdin: &mut impl Write,
    next_request_id: &mut u64,
    state: &Mutex<AgyRuntimeState>,
    signals: &WriterSignals,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, RpcCallError> {
    *next_request_id += 1;
    let id = *next_request_id;
    let (tx, rx) = bounded(1);
    state.lock().pending_rpc.insert(id, tx);
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    if let Err(error) = write_json_line(stdin, &payload) {
        state.lock().pending_rpc.remove(&id);
        return Err(RpcCallError::Failed(error.to_string()));
    }
    let deadline = Instant::now() + timeout;
    loop {
        select! {
            recv(rx) -> msg => return msg.map_err(|_| RpcCallError::Disconnected),
            recv(signals.shutdown) -> _ => {
                state.lock().pending_rpc.remove(&id);
                return Err(RpcCallError::Disconnected);
            }
            // The server may block this RPC on a request of its own (a
            // permission or an unsupported callback); answer it now rather
            // than after the RPC times out.
            recv(signals.urgent) -> msg => match msg {
                Ok(CommandMessage::Shutdown) => {
                    signals.stop.set(true);
                    state.lock().pending_rpc.remove(&id);
                    return Err(RpcCallError::Disconnected);
                }
                Ok(command) => {
                    if write_raw_reply(stdin, command).is_err() {
                        state.lock().pending_rpc.remove(&id);
                        return Err(RpcCallError::Disconnected);
                    }
                }
                // The reader exited; only a response it already delivered
                // can still arrive.
                Err(_) => {
                    state.lock().pending_rpc.remove(&id);
                    return rx.try_recv().map_err(|_| RpcCallError::Disconnected);
                }
            },
            default(deadline.saturating_duration_since(Instant::now())) => {
                state.lock().pending_rpc.remove(&id);
                return Err(RpcCallError::Failed(format!("{method} timed out")));
            }
        }
    }
}

fn emit_rpc_error(error: &Value, key: RpcErrorKey, events: &impl DriverEventSink) {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    let _ = events.send(DriverEvent::Error(key.format(message)));
}

#[allow(clippy::too_many_arguments)]
fn handle_acp_message(
    value: Value,
    state: &Mutex<AgyRuntimeState>,
    suppress_replay: &AtomicBool,
    commands: &Sender<CommandMessage>,
    events: &impl DriverEventSink,
    stream: &mut AgyStreamState,
    mode: RuntimeMode,
    subagent_child: bool,
) {
    let method = value.get("method").and_then(Value::as_str);
    let id = value.get("id");

    match (id, method) {
        // Response to a Raku JSON-RPC request.
        (Some(id), None) => {
            let Some(num_id) = id.as_u64() else {
                return;
            };
            let waiter = state.lock().pending_rpc.remove(&num_id);
            if let Some(waiter) = waiter {
                // Clear replay suppression before unblocking `session/load` so
                // no subsequent live notifications are dropped.
                if suppress_replay.load(Ordering::Acquire) {
                    suppress_replay.store(false, Ordering::Release);
                }
                let _ = waiter.send(value);
                return;
            }
            let is_active_prompt = {
                let mut guard = state.lock();
                if guard.active_prompt_id == Some(num_id) {
                    guard.active_prompt_id = None;
                    true
                } else {
                    false
                }
            };
            if is_active_prompt {
                handle_prompt_response(&value, state, commands, events, stream);
            }
        }
        // Server -> client JSON-RPC request (`session/request_permission`).
        (Some(raw_id), Some(method)) => {
            if method == "session/request_permission" {
                handle_permission_request(
                    raw_id.clone(),
                    value.get("params").unwrap_or(&Value::Null),
                    state,
                    commands,
                    events,
                    mode,
                    subagent_child,
                );
            } else {
                // Every JSON-RPC request carrying an `id` requires a reply so
                // the ACP server never blocks indefinitely on an unsupported
                // client callback.
                let _ = commands.send(CommandMessage::RespondErrorRaw {
                    raw_id: raw_id.clone(),
                    code: -32601,
                    message: format!("Method not found: {method}"),
                });
            }
        }
        // JSON-RPC notification (`session/update`).
        (None, Some(method)) => {
            if method == "session/update" {
                handle_session_update(
                    value.pointer("/params/update").unwrap_or(&Value::Null),
                    state,
                    suppress_replay,
                    events,
                    stream,
                );
            }
        }
        (None, None) => {}
    }
}

fn handle_prompt_response(
    value: &Value,
    state: &Mutex<AgyRuntimeState>,
    commands: &Sender<CommandMessage>,
    events: &impl DriverEventSink,
    stream: &mut AgyStreamState,
) {
    stream.close_reasoning(events);
    let plan_mode = state.lock().plan_mode;
    let saw_plan = stream.saw_plan_artifact;
    stream.reset_turn(events);

    if let Some(error) = value.get("error") {
        if is_auth_error(error) {
            let _ = events.send(DriverEvent::Error(tr!("errors.agy_auth_required")));
            let _ = commands.send(CommandMessage::Shutdown);
            let _ = events.send(DriverEvent::TurnFinished {
                success: false,
                summary: None,
            });
            return;
        }
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Antigravity turn failed");
        let _ = events.send(DriverEvent::Error(tr!(
            "errors.provider_rejected_prompt_detail",
            provider = ProviderKind::Antigravity.display_name(),
            error = message
        )));
        let _ = events.send(DriverEvent::TurnFinished {
            success: false,
            summary: None,
        });
        return;
    }

    if let Some(usage) = value
        .pointer("/result/usage")
        .filter(|usage| !usage.is_null())
    {
        state.lock().last_turn_usage = Some(usage.clone());
    }

    let stop_reason = value
        .pointer("/result/stopReason")
        .and_then(Value::as_str)
        .unwrap_or("");
    if stop_reason == "end_turn" {
        if plan_mode && saw_plan {
            let _ = events.send(DriverEvent::PlanProposed { request_id: None });
        }
        let _ = events.send(DriverEvent::TurnFinished {
            success: true,
            summary: None,
        });
    } else {
        let summary = if stop_reason.is_empty() {
            tr!(
                "errors.provider_complete_turn",
                provider = ProviderKind::Antigravity.display_name()
            )
        } else {
            tr!("session.agent_stopped_reason", reason = stop_reason)
        };
        let _ = events.send(DriverEvent::TurnFinished {
            success: false,
            summary: Some(summary),
        });
    }
}

fn handle_permission_request(
    raw_id: Value,
    params: &Value,
    state: &Mutex<AgyRuntimeState>,
    commands: &Sender<CommandMessage>,
    events: &impl DriverEventSink,
    mode: RuntimeMode,
    subagent_child: bool,
) {
    let options: Vec<AcpPermissionOption> = params
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|opt| {
            let option_id = opt.get("optionId").and_then(Value::as_str)?.to_owned();
            let name = opt
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&option_id)
                .to_owned();
            let kind = opt
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Some(AcpPermissionOption {
                option_id,
                name,
                kind,
            })
        })
        .collect();

    if subagent_child {
        let reject_id = options
            .iter()
            .find(|opt| matches!(opt.kind.as_str(), "reject_once" | "reject_always"))
            .map(|opt| opt.option_id.as_str());
        let result = match reject_id {
            Some(option_id) => {
                json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
            }
            None => json!({ "outcome": { "outcome": "cancelled" } }),
        };
        let _ = commands.send(CommandMessage::RespondPermissionRaw { raw_id, result });
        return;
    }

    if mode == RuntimeMode::FullAccess {
        let allow_id = options
            .iter()
            .find(|opt| opt.kind == "allow_always")
            .or_else(|| options.iter().find(|opt| opt.kind == "allow_once"))
            .or_else(|| options.first())
            .map(|opt| opt.option_id.as_str());
        let result = match allow_id {
            Some(option_id) => {
                json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
            }
            None => json!({ "outcome": { "outcome": "cancelled" } }),
        };
        let _ = commands.send(CommandMessage::RespondPermissionRaw { raw_id, result });
        return;
    }

    let request_key = match &raw_id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let tool_call = params.get("toolCall").unwrap_or(&Value::Null);
    let tool_name = tool_call
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Tool");
    let title = tool_call
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| tool_name.to_owned());
    let detail = tool_call
        .get("rawInput")
        .or_else(|| tool_call.get("args"))
        .and_then(super::activity::format_json)
        .unwrap_or_default();

    let driver_options: Vec<PermissionOption> = options
        .iter()
        .map(|opt| PermissionOption {
            id: opt.option_id.clone(),
            label: opt.name.clone(),
            allow: matches!(opt.kind.as_str(), "allow_once" | "allow_always"),
        })
        .collect();

    state
        .lock()
        .pending_permissions
        .insert(request_key.clone(), (raw_id, options));

    let _ = events.send(DriverEvent::Permission {
        request_id: request_key,
        title,
        detail,
        options: driver_options,
    });
}

fn handle_session_update(
    update: &Value,
    state: &Mutex<AgyRuntimeState>,
    suppress_replay: &AtomicBool,
    events: &impl DriverEventSink,
    stream: &mut AgyStreamState,
) {
    let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
        return;
    };
    if suppress_replay.load(Ordering::Acquire) && is_replay_update_kind(kind) {
        return;
    }

    match kind {
        "available_commands_update" => {
            let commands = update
                .get("availableCommands")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|cmd| {
                    let name = cmd
                        .get("name")
                        .and_then(Value::as_str)?
                        .trim()
                        .trim_start_matches('/')
                        .to_owned();
                    if name.is_empty() {
                        return None;
                    }
                    let description = cmd
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_owned();
                    Some(ReportedCommand { name, description })
                })
                .collect();
            let _ = events.send(DriverEvent::AvailableCommands(commands));
        }
        "usage_update" => {
            let used = update.get("used").and_then(Value::as_u64);
            let size = update.get("size").and_then(Value::as_u64);
            if let (Some(used), Some(size)) = (used, size) {
                state.lock().last_usage = Some((used, size));
            }
            if used.is_some() || size.is_some() {
                let _ = events.send(DriverEvent::UsageUpdated {
                    context_tokens: used,
                    context_window: size,
                });
            }
        }
        "session_info_update" => {
            if let Some(title) = update
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|title| !title.is_empty())
            {
                let _ = events.send(DriverEvent::AutoTitleUpdated(Some(title.to_owned())));
            }
        }
        "config_option_update" => {
            let extracted = extract_config_state(update);
            let mut guard = state.lock();
            apply_extracted_config(&mut guard, extracted);
        }
        "current_mode_update" => {
            if let Some(mode_id) = update.get("currentModeId").and_then(Value::as_str) {
                state.lock().plan_mode = mode_id == "plan";
            }
        }
        "agent_thought_chunk" => {
            if let Some(text) = extract_chunk_text(update) {
                stream.append_reasoning(text, events);
            }
        }
        "agent_message_chunk" => {
            stream.close_reasoning(events);
            if let Some(text) = extract_chunk_text(update)
                && !text.is_empty()
            {
                let _ = events.send(DriverEvent::TextDelta(text.to_owned()));
            }
        }
        "tool_call" | "tool_call_update" => {
            stream.close_reasoning(events);
            let Some(tool_call_id) = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            else {
                return;
            };
            let entry = stream
                .tool_calls
                .entry(tool_call_id.to_owned())
                .or_insert_with(|| AcpToolCallState {
                    tool_call_id: tool_call_id.to_owned(),
                    ..Default::default()
                });
            if let Some(name) = update.get("name").and_then(Value::as_str)
                && !name.is_empty()
            {
                entry.name = name.to_owned();
            }
            if let Some(title) = update.get("title").and_then(Value::as_str)
                && !title.trim().is_empty()
            {
                entry.title = title.trim().to_owned();
            }
            if let Some(acp_kind) = update.get("kind").and_then(Value::as_str)
                && !acp_kind.is_empty()
            {
                entry.kind = acp_kind.to_owned();
            }
            if let Some(status) = update.get("status").and_then(Value::as_str) {
                entry.status = Some(status.to_owned());
            }
            if let Some(args) = update.get("args")
                && !args.is_null()
            {
                if args
                    .pointer("/ArtifactMetadata/RequestFeedback")
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    stream.saw_plan_artifact = true;
                }
                entry.args = Some(args.clone());
            }
            if let Some(raw_input) = update.get("rawInput")
                && !raw_input.is_null()
            {
                if raw_input
                    .pointer("/ArtifactMetadata/RequestFeedback")
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    stream.saw_plan_artifact = true;
                }
                entry.raw_input = Some(raw_input.clone());
            }
            if let Some(raw_output) = update.get("rawOutput")
                && !raw_output.is_null()
            {
                entry.raw_output = Some(raw_output.clone());
            }
            if let Some(content) = update.get("content")
                && !content.is_null()
            {
                entry.content = Some(content.clone());
            }
            let item = normalize_tool_call(entry);
            let _ = events.send(DriverEvent::RichActivity(item));
        }
        "plan" => {
            stream.close_reasoning(events);
            if let Some(item) = normalize_plan_update(update) {
                let _ = events.send(DriverEvent::RichActivity(item));
            }
        }
        _ => {}
    }
}

fn is_replay_update_kind(kind: &str) -> bool {
    matches!(
        kind,
        "user_message_chunk"
            | "agent_message_chunk"
            | "agent_thought_chunk"
            | "tool_call"
            | "tool_call_update"
            | "plan"
    )
}

fn extract_chunk_text(update: &Value) -> Option<&str> {
    let content = update.get("content")?;
    if let Some(text) = content.get("text").and_then(Value::as_str) {
        return Some(text);
    }
    content.as_str()
}

fn normalize_tool_call(state: &AcpToolCallState) -> ActivityItem {
    let by_name = if state.name.is_empty() {
        ActivityKind::Tool
    } else {
        super::support::classify_tool(&state.name)
    };
    let kind = if by_name != ActivityKind::Tool {
        by_name
    } else {
        match state.kind.as_str() {
            "execute" => ActivityKind::Command,
            "edit" => ActivityKind::FileChange,
            "read" => ActivityKind::FileRead,
            "search" => ActivityKind::FileSearch,
            "fetch" => ActivityKind::Search,
            "think" => ActivityKind::Reasoning,
            _ => ActivityKind::Tool,
        }
    };

    let raw_args = state.raw_input.as_ref().or(state.args.as_ref());
    let title = super::activity::input_title(raw_args)
        .or_else(|| (!state.title.is_empty()).then(|| state.title.clone()))
        .or_else(|| (!state.name.is_empty()).then(|| state.name.clone()))
        .unwrap_or_else(|| "Tool".to_owned());

    let unwrapped_output =
        unwrap_acp_output(kind, state.raw_output.as_ref(), state.content.as_ref());
    let exit_code_failed = state
        .raw_output
        .as_ref()
        .and_then(|out| out.get("exit_code").and_then(Value::as_i64))
        .is_some_and(|code| code != 0);
    let failed = matches!(state.status.as_deref(), Some("failed" | "error")) || exit_code_failed;
    let complete = matches!(
        state.status.as_deref(),
        Some("completed" | "failed" | "error" | "cancelled")
    );

    super::activity::tool_activity(
        Some(state.tool_call_id.clone()),
        kind,
        title,
        raw_args,
        unwrapped_output.as_ref(),
        state.content.as_ref(),
        failed,
        complete,
    )
    .with_tool_name((!state.name.is_empty()).then_some(state.name.as_str()))
}

fn unwrap_acp_output(
    kind: ActivityKind,
    raw_output: Option<&Value>,
    content: Option<&Value>,
) -> Option<Value> {
    let normalize = |value: Value| -> Value {
        if kind == ActivityKind::FileRead {
            normalize_agy_read_value(value)
        } else {
            value
        }
    };
    if let Some(raw_output) = raw_output {
        if let Some(text) = raw_output.get("output").and_then(Value::as_str) {
            return Some(normalize(Value::String(text.to_owned())));
        }
        return Some(normalize(raw_output.clone()));
    }
    let content = content?;
    if let Some(items) = content.as_array() {
        let unwrapped: Vec<Value> = items
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(Value::as_str) == Some("content") {
                    item.get("content").cloned()
                } else {
                    Some(item.clone())
                }
            })
            .collect();
        return Some(normalize(Value::Array(unwrapped)));
    }
    Some(normalize(content.clone()))
}

/// Strips Antigravity's `Created At` / `Completed At` / `Showing lines` header
/// and converts `<line>: <code>` gutters into Raku's `<line>\t<code>` form so
/// `read_range` and the syntax-highlighted file preview recognize them.
fn normalize_agy_read_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(normalize_agy_read_text(&text)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| {
                    if item.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(text) = item.get("text").and_then(Value::as_str)
                    {
                        let mut clone = item.clone();
                        clone["text"] = Value::String(normalize_agy_read_text(text));
                        clone
                    } else {
                        item
                    }
                })
                .collect(),
        ),
        other => other,
    }
}

fn normalize_agy_read_text(text: &str) -> String {
    let mut lines = text.lines().peekable();
    if lines
        .peek()
        .is_some_and(|line| line.starts_with("Created At: "))
    {
        lines.next();
        if lines
            .peek()
            .is_some_and(|line| line.starts_with("Completed At: "))
        {
            lines.next();
        }
    }
    let had_showing_header = lines
        .peek()
        .is_some_and(|line| line.starts_with("Showing lines "));
    if had_showing_header {
        lines.next();
    }
    let remaining: Vec<&str> = lines.collect();
    if remaining.is_empty() {
        return text.to_owned();
    }
    fn parse_colon_gutter(line: &str) -> Option<(u32, &str)> {
        let trimmed = line.trim_start_matches(' ');
        let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || digits > 7 {
            return None;
        }
        let rest = trimmed.get(digits..)?;
        let code = rest
            .strip_prefix(": ")
            .or_else(|| (rest == ":").then_some(""))?;
        Some((trimmed.get(..digits)?.parse().ok()?, code))
    }
    let non_empty = remaining.iter().filter(|l| !l.trim().is_empty()).count();
    let numbered = remaining
        .iter()
        .filter(|l| !l.trim().is_empty() && parse_colon_gutter(l).is_some())
        .count();
    if non_empty == 0 || (!had_showing_header && numbered * 4 < non_empty * 3) {
        return text.to_owned();
    }
    remaining
        .into_iter()
        .map(|line| match parse_colon_gutter(line) {
            Some((num, code)) => format!("{num}\t{code}"),
            None => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_plan_update(update: &Value) -> Option<ActivityItem> {
    let entries = update.get("entries").and_then(Value::as_array)?;
    let steps: Vec<PlanStep> = entries
        .iter()
        .filter_map(|entry| {
            let text = ["content", "step", "subject", "title", "text"]
                .iter()
                .find_map(|key| entry.get(key).and_then(Value::as_str))?
                .trim();
            (!text.is_empty()).then(|| PlanStep {
                text: text.to_owned(),
                status: PlanStepStatus::from_wire(entry.get("status").and_then(Value::as_str)),
            })
        })
        .collect();
    if steps.is_empty() {
        return None;
    }
    Some(
        ActivityItem::new(
            Some("agy-plan".into()),
            ActivityKind::Plan,
            tr!("activity.plan"),
            None,
            true,
        )
        .with_plan_steps(steps),
    )
}

/// The session's current ACP mode from a `session/new`, `session/resume` or
/// `session/load` result: `modes.currentModeId`, else the `mode` config option.
fn extract_current_mode(value: &Value) -> Option<String> {
    let from_modes = value
        .pointer("/modes/currentModeId")
        .and_then(Value::as_str);
    let from_config = || {
        value
            .get("configOptions")
            .and_then(Value::as_array)?
            .iter()
            .find(|entry| {
                entry.get("id").and_then(Value::as_str) == Some("mode")
                    || entry.get("category").and_then(Value::as_str) == Some("mode")
            })?
            .get("currentValue")
            .and_then(Value::as_str)
    };
    from_modes
        .or_else(from_config)
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .map(str::to_owned)
}

struct ExtractedConfig {
    current_model: Option<String>,
    has_thought_level: bool,
    current_effort: Option<String>,
    thought_levels: Vec<String>,
}

fn apply_extracted_config(state: &mut AgyRuntimeState, cfg: ExtractedConfig) {
    let saw_model = cfg.current_model.is_some();
    if let Some(model) = cfg.current_model {
        state.current_model = Some(model);
    }
    if cfg.has_thought_level {
        state.current_effort = cfg.current_effort;
        state.thought_levels = cfg.thought_levels;
    } else if saw_model {
        // Standalone models (e.g. `gpt-oss-120b-medium`) omit `thought_level`
        // from `configOptions` altogether; clear stale state from a prior model.
        state.current_effort = None;
        state.thought_levels.clear();
    }
}

fn extract_config_state(value: &Value) -> ExtractedConfig {
    let options = value
        .get("configOptions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut current_model = None;
    let mut has_thought_level = false;
    let mut current_effort = None;
    let mut thought_levels = Vec::new();
    for entry in options {
        let id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
        let category = entry.get("category").and_then(Value::as_str).unwrap_or(id);
        let current = entry
            .get("currentValue")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned);
        if category == "model" || id == "model" {
            current_model = current;
        } else if category == "thought_level" || id == "thought_level" {
            has_thought_level = true;
            current_effort = current;
            thought_levels = entry
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|opt| {
                    opt.get("value")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                })
                .collect();
        }
    }
    ExtractedConfig {
        current_model,
        has_thought_level,
        current_effort,
        thought_levels,
    }
}

/// The `model` option values a session response advertises, or empty when it
/// lists none.
fn offered_model_ids(value: &Value) -> Vec<String> {
    value
        .get("configOptions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|entry| {
            entry.get("category").and_then(Value::as_str) == Some("model")
                || entry.get("id").and_then(Value::as_str) == Some("model")
        })
        .and_then(|entry| entry.get("options"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| option.get("value").and_then(Value::as_str))
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .collect()
}

/// The model to switch to for `requested`, plus the effort an effort-suffixed
/// id carried. `None` means the session does not offer it at all, so the
/// session keeps its own model. An empty `offered` list cannot be checked, so
/// the request goes through as is.
fn resolve_requested_model<'a>(
    requested: &'a str,
    offered: &[String],
) -> Option<(&'a str, Option<&'a str>)> {
    if offered.is_empty() || offered.iter().any(|id| id == requested) {
        return Some((requested, None));
    }
    let (stem, effort) = crate::model_catalog::split_agy_effort_suffix(requested)?;
    offered
        .iter()
        .any(|id| id == stem)
        .then_some((stem, Some(effort)))
}

fn clamp_thought_level(
    requested: Option<&str>,
    supported: &[String],
    fallback: Option<&str>,
) -> Option<String> {
    if supported.is_empty() {
        return None;
    }
    if let Some(req) = requested {
        if supported.iter().any(|item| item == req) {
            return Some(req.to_owned());
        }
        if let Some(pos) = EFFORT_ORDER.iter().position(|level| *level == req) {
            for candidate in EFFORT_ORDER[..pos].iter().rev() {
                if supported.iter().any(|item| item == candidate) {
                    return Some((*candidate).to_owned());
                }
            }
        }
    }
    if let Some(fb) = fallback
        && supported.iter().any(|item| item == fb)
    {
        return Some(fb.to_owned());
    }
    supported.first().cloned()
}

fn acp_mode_for(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::Ask => "default",
        RuntimeMode::AutoAcceptEdits => "accept-edits",
        RuntimeMode::Auto | RuntimeMode::FullAccess => "auto",
    }
}

fn effective_acp_mode(base_mode: RuntimeMode, plan_mode: bool) -> &'static str {
    if plan_mode {
        "plan"
    } else {
        acp_mode_for(base_mode)
    }
}

fn is_auth_error(error: &Value) -> bool {
    if error.get("code").and_then(Value::as_i64) == Some(-32000) {
        return true;
    }
    let msg = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    msg.contains("authentication required")
        || msg.contains("not authenticated")
        || msg.contains("unauthenticated")
        || msg.contains("login required")
}

/// Most whitespace-separated words one `@` mention may span. Attachment
/// mentions are paths, which can contain spaces (`/tmp/no project.png`).
const MAX_MENTION_WORDS: usize = 16;

/// The text block plus one image block per `@` mention that names a readable
/// image. Mentions are project-relative when the file is inside the project
/// (`dropped_file_mention`), so relative paths resolve against the session's
/// `cwd`, never the daemon's working directory.
fn build_prompt_blocks(text: &str, cwd: &Path) -> Vec<Value> {
    let mut blocks = vec![json!({ "type": "text", "text": text })];
    let mut seen_paths = std::collections::HashSet::new();
    for (path, mime) in image_mentions(text, cwd) {
        if !seen_paths.insert(path.clone()) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            blocks.push(json!({
                "type": "image",
                "data": encoded,
                "mimeType": mime,
            }));
        }
    }
    blocks
}

/// Every `@` mention at a word start that resolves to an image file within
/// the size limit. A mention may run across spaces up to the next mention;
/// the shortest span naming an existing image wins.
fn image_mentions(text: &str, cwd: &Path) -> Vec<(std::path::PathBuf, &'static str)> {
    let starts = text
        .char_indices()
        .filter(|&(index, ch)| {
            ch == '@'
                && text[..index]
                    .chars()
                    .next_back()
                    .is_none_or(char::is_whitespace)
        })
        .map(|(index, _)| index + 1)
        .collect::<Vec<_>>();

    let mut found = Vec::new();
    for (position, &start) in starts.iter().enumerate() {
        // A mention never reaches into the next one.
        let limit = starts.get(position + 1).map_or(text.len(), |next| next - 1);
        let rest = &text[start..limit];
        let ends = rest
            .char_indices()
            .filter(|&(_, ch)| ch.is_whitespace())
            .map(|(index, _)| index)
            .chain(std::iter::once(rest.len()))
            .take(MAX_MENTION_WORDS);
        for end in ends {
            let candidate = rest[..end].trim_end();
            if candidate.is_empty() {
                continue;
            }
            let raw = Path::new(candidate);
            let Some(mime) = image_mime_for_path(raw) else {
                continue;
            };
            let path = if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                cwd.join(raw)
            };
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.is_file() && meta.len() > 0 && meta.len() <= MAX_IMAGE_BYTES {
                found.push((path, mime));
                break;
            }
        }
    }
    found
}

fn image_mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::test_event_channel;

    fn samples() -> Value {
        serde_json::from_str(include_str!("fixtures/agy_acp_samples.json"))
            .expect("agy fixture json must parse")
    }

    #[test]
    fn reads_current_mode_from_modes_or_config_options() {
        assert_eq!(
            extract_current_mode(&json!({ "modes": { "currentModeId": "auto" } })).as_deref(),
            Some("auto")
        );
        assert_eq!(
            extract_current_mode(&json!({
                "configOptions": [{ "id": "mode", "category": "mode", "currentValue": "plan" }]
            }))
            .as_deref(),
            Some("plan")
        );
        // A resume result with no mode is unknown, never assumed `default`,
        // so the handshake sets the requested mode explicitly.
        assert_eq!(extract_current_mode(&json!({})), None);
    }

    #[test]
    fn parses_session_new_config_options_camel_case_only() {
        let fixtures = samples();
        let cfg = extract_config_state(&fixtures["session_new_result"]);
        assert_eq!(cfg.current_model.as_deref(), Some("gemini-4-neon"));
        assert_eq!(cfg.current_effort.as_deref(), Some("high"));
        assert_eq!(cfg.thought_levels, ["low", "medium", "high", "xhigh"]);

        let mut runtime = AgyRuntimeState::default();
        apply_extracted_config(&mut runtime, cfg);
        // Switching to a standalone model without `thought_level` clears stale
        // effort state from the previous model.
        let standalone = extract_config_state(&json!({
            "configOptions": [{
                "id": "model",
                "category": "model",
                "currentValue": "gpt-oss-120b-medium",
                "options": []
            }]
        }));
        apply_extracted_config(&mut runtime, standalone);
        assert_eq!(
            runtime.current_model.as_deref(),
            Some("gpt-oss-120b-medium")
        );
        assert_eq!(runtime.current_effort, None);
        assert!(runtime.thought_levels.is_empty());
    }

    #[test]
    fn unknown_server_request_replies_with_method_not_found_and_read_output_normalizes() {
        let state = Mutex::new(AgyRuntimeState::default());
        let suppress = AtomicBool::new(false);
        let (commands, cmd_rx) = unbounded();
        let (events, rx) = test_event_channel();
        let mut stream = AgyStreamState::default();

        handle_acp_message(
            json!({
                "jsonrpc": "2.0",
                "id": "req-99",
                "method": "fs/read_text_file",
                "params": { "path": "/tmp/x" }
            }),
            &state,
            &suppress,
            &commands,
            &events,
            &mut stream,
            RuntimeMode::Ask,
            false,
        );
        let cmd = cmd_rx.try_recv().expect("method-not-found reply");
        assert!(matches!(
            cmd,
            CommandMessage::RespondErrorRaw { raw_id, code: -32601, .. }
                if raw_id == json!("req-99")
        ));

        handle_acp_message(
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s1",
                    "update": {
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": "read-1",
                        "name": "read_file",
                        "kind": "read",
                        "status": "completed",
                        "rawInput": { "Path": "/work/src/lib.rs", "StartLine": 10, "EndLine": 12 },
                        "rawOutput": {
                            "output": "Created At: 2026-11-26T20:00:00+09:00\nCompleted At: 2026-11-26T20:00:01+09:00\nShowing lines 10-12 of 90\n10: fn a() {}\n11: \n12: fn b() {}\n"
                        }
                    }
                }
            }),
            &state,
            &suppress,
            &commands,
            &events,
            &mut stream,
            RuntimeMode::Ask,
            false,
        );
        let ev = rx.try_recv().expect("rich activity");
        let DriverEvent::RichActivity(item) = ev else {
            panic!("expected RichActivity, got {ev:?}");
        };
        assert_eq!(item.kind, ActivityKind::FileRead);
        assert_eq!(
            item.read_range,
            Some(raku_protocol::model::ActivityReadRange { start: 10, end: 12 })
        );
        assert_eq!(
            item.output.as_deref(),
            Some("10\tfn a() {}\n11\t\n12\tfn b() {}")
        );
    }

    #[test]
    fn streams_thoughts_text_tools_commands_usage_and_title() {
        let fixtures = samples();
        let state = Mutex::new(AgyRuntimeState {
            active_prompt_id: Some(10),
            ..Default::default()
        });
        let suppress = AtomicBool::new(false);
        let (commands, _cmd_rx) = unbounded();
        let (events, rx) = test_event_channel();
        let mut stream = AgyStreamState::default();

        for key in [
            "available_commands_update",
            "usage_update",
            "session_info_update",
            "agent_thought_chunk",
            "tool_call",
            "tool_call_completed",
            "agent_message_chunk",
            "prompt_end_turn",
        ] {
            handle_acp_message(
                fixtures[key].clone(),
                &state,
                &suppress,
                &commands,
                &events,
                &mut stream,
                RuntimeMode::Ask,
                false,
            );
        }

        let emitted: Vec<_> = rx.try_iter().collect();
        assert!(emitted.iter().any(|e| matches!(
            e,
            DriverEvent::AvailableCommands(cmds) if cmds.len() == 2 && cmds[0].name == "goal"
        )));
        assert!(emitted.iter().any(|e| matches!(
            e,
            DriverEvent::UsageUpdated {
                context_tokens: Some(10538),
                context_window: Some(1048576)
            }
        )));
        assert!(emitted.iter().any(|e| matches!(
            e,
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Shell Command Execution Test"
        )));
        assert!(
            emitted
                .iter()
                .any(|e| matches!(e, DriverEvent::ReasoningStarted { .. }))
        );
        assert!(emitted.iter().any(|e| matches!(
            e,
            DriverEvent::ReasoningBlockDelta { delta, .. } if delta == "Checking workspace state."
        )));
        assert!(
            emitted
                .iter()
                .any(|e| matches!(e, DriverEvent::ReasoningCompleted { .. }))
        );
        assert!(emitted.iter().any(|e| matches!(
            e,
            DriverEvent::RichActivity(item) if item.complete && !item.failed && item.kind == ActivityKind::Command
        )));
        assert!(emitted.iter().any(|e| matches!(
            e,
            DriverEvent::TextDelta(text) if text == "BANANA"
        )));
        assert!(emitted.iter().any(|e| matches!(
            e,
            DriverEvent::TurnFinished {
                success: true,
                summary: None
            }
        )));
    }

    #[test]
    fn permission_request_preserves_string_id_and_respects_mode_policies() {
        let fixtures = samples();
        let req = fixtures["request_permission"].clone();

        // 1. Ask mode: surfaces Permission event and keeps raw string id `"1"`.
        let state = Mutex::new(AgyRuntimeState::default());
        let suppress = AtomicBool::new(false);
        let (commands, cmd_rx) = unbounded();
        let (events, rx) = test_event_channel();
        let mut stream = AgyStreamState::default();
        handle_acp_message(
            req.clone(),
            &state,
            &suppress,
            &commands,
            &events,
            &mut stream,
            RuntimeMode::Ask,
            false,
        );
        assert!(cmd_rx.try_recv().is_err());
        let event = rx.try_recv().expect("permission event");
        assert!(matches!(
            event,
            DriverEvent::Permission { ref request_id, ref options, .. }
                if request_id == "1" && options.len() == 2 && options[0].allow && !options[1].allow
        ));
        assert_eq!(state.lock().pending_permissions["1"].0, json!("1"));

        // 2. FullAccess mode: auto-approves via writer without raising UI card.
        let state = Mutex::new(AgyRuntimeState::default());
        let (commands, cmd_rx) = unbounded();
        let (events, rx) = test_event_channel();
        handle_acp_message(
            req.clone(),
            &state,
            &suppress,
            &commands,
            &events,
            &mut stream,
            RuntimeMode::FullAccess,
            false,
        );
        assert!(rx.try_recv().is_err());
        let cmd = cmd_rx.try_recv().expect("auto-approve reply");
        assert!(matches!(
            cmd,
            CommandMessage::RespondPermissionRaw { raw_id, result }
                if raw_id == json!("1") && result["outcome"]["optionId"] == "allow"
        ));

        // 3. Subagent child: auto-declines without raising UI card.
        let state = Mutex::new(AgyRuntimeState::default());
        let (commands, cmd_rx) = unbounded();
        let (events, rx) = test_event_channel();
        handle_acp_message(
            req,
            &state,
            &suppress,
            &commands,
            &events,
            &mut stream,
            RuntimeMode::FullAccess,
            true,
        );
        assert!(rx.try_recv().is_err());
        let cmd = cmd_rx.try_recv().expect("auto-decline reply");
        assert!(matches!(
            cmd,
            CommandMessage::RespondPermissionRaw { raw_id, result }
                if raw_id == json!("1") && result["outcome"]["optionId"] == "deny"
        ));
    }

    #[test]
    fn session_load_suppresses_replay_but_forwards_metadata() {
        let fixtures = samples();
        let state = Mutex::new(AgyRuntimeState::default());
        let suppress = AtomicBool::new(true);
        let (commands, _cmd_rx) = unbounded();
        let (events, rx) = test_event_channel();
        let mut stream = AgyStreamState::default();

        for notif in [
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s",
                    "update": { "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "old" } }
                }
            }),
            fixtures["agent_message_chunk"].clone(),
            fixtures["agent_thought_chunk"].clone(),
            fixtures["tool_call"].clone(),
            fixtures["available_commands_update"].clone(),
            fixtures["usage_update"].clone(),
            fixtures["session_info_update"].clone(),
        ] {
            handle_acp_message(
                notif,
                &state,
                &suppress,
                &commands,
                &events,
                &mut stream,
                RuntimeMode::Ask,
                false,
            );
        }

        let emitted: Vec<_> = rx.try_iter().collect();
        assert_eq!(emitted.len(), 3);
        assert!(matches!(emitted[0], DriverEvent::AvailableCommands(_)));
        assert!(matches!(emitted[1], DriverEvent::UsageUpdated { .. }));
        assert!(matches!(emitted[2], DriverEvent::AutoTitleUpdated(Some(_))));
    }

    #[test]
    fn plan_mode_emits_plan_proposed_on_end_turn_and_non_end_turn_fails() {
        let fixtures = samples();
        let state = Mutex::new(AgyRuntimeState {
            plan_mode: true,
            active_prompt_id: Some(10),
            ..Default::default()
        });
        let suppress = AtomicBool::new(false);
        let (commands, _cmd_rx) = unbounded();
        let (events, rx) = test_event_channel();
        // Plain text in plan mode (a clarifying question) is not a plan.
        let mut stream = AgyStreamState::default();
        handle_acp_message(
            fixtures["agent_message_chunk"].clone(),
            &state,
            &suppress,
            &commands,
            &events,
            &mut stream,
            RuntimeMode::FullAccess,
            false,
        );
        handle_acp_message(
            fixtures["prompt_end_turn"].clone(),
            &state,
            &suppress,
            &commands,
            &events,
            &mut stream,
            RuntimeMode::FullAccess,
            false,
        );
        let emitted: Vec<_> = rx.try_iter().collect();
        assert!(
            !emitted
                .iter()
                .any(|event| matches!(event, DriverEvent::PlanProposed { .. })),
            "text alone must not propose a plan"
        );

        state.lock().active_prompt_id = Some(10);
        let mut stream = AgyStreamState {
            saw_plan_artifact: true,
            ..Default::default()
        };

        handle_acp_message(
            fixtures["prompt_end_turn"].clone(),
            &state,
            &suppress,
            &commands,
            &events,
            &mut stream,
            RuntimeMode::FullAccess,
            false,
        );
        let emitted: Vec<_> = rx.try_iter().collect();
        assert!(matches!(
            emitted.as_slice(),
            [
                DriverEvent::PlanProposed { request_id: None },
                DriverEvent::TurnFinished {
                    success: true,
                    summary: None
                }
            ]
        ));

        state.lock().active_prompt_id = Some(11);
        handle_acp_message(
            fixtures["prompt_cancelled"].clone(),
            &state,
            &suppress,
            &commands,
            &events,
            &mut stream,
            RuntimeMode::FullAccess,
            false,
        );
        let emitted: Vec<_> = rx.try_iter().collect();
        assert!(matches!(
            emitted.as_slice(),
            [DriverEvent::TurnFinished {
                success: false,
                summary: Some(_)
            }]
        ));
    }

    #[test]
    fn blocking_rpc_answers_reader_replies_and_honors_auth_shutdown() {
        let state = Mutex::new(AgyRuntimeState::default());
        let (urgent_tx, urgent_rx) = unbounded();
        let (_shutdown_tx, shutdown_rx) = bounded::<()>(1);
        let signals = WriterSignals {
            shutdown: shutdown_rx,
            urgent: urgent_rx,
            stop: Cell::new(false),
        };
        urgent_tx
            .send(CommandMessage::RespondErrorRaw {
                raw_id: json!("req-1"),
                code: -32601,
                message: "Method not found: fs/read_text_file".into(),
            })
            .unwrap();
        let mut stdin = Vec::new();
        let mut next_id = 0;
        let result = call_rpc_sync(
            &mut stdin,
            &mut next_id,
            &state,
            &signals,
            "session/load",
            json!({}),
            Duration::from_millis(100),
        );
        assert!(matches!(result, Err(RpcCallError::Failed(_))));
        let lines: Vec<Value> = String::from_utf8(stdin)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["method"], "session/load");
        assert_eq!(lines[1]["id"], "req-1");
        assert_eq!(lines[1]["error"]["code"], -32601);
        assert!(state.lock().pending_rpc.is_empty());

        urgent_tx.send(CommandMessage::Shutdown).unwrap();
        let mut stdin = Vec::new();
        let started = std::time::Instant::now();
        let result = call_rpc_sync(
            &mut stdin,
            &mut next_id,
            &state,
            &signals,
            "session/set_config_option",
            json!({}),
            Duration::from_secs(5),
        );
        assert!(matches!(result, Err(RpcCallError::Disconnected)));
        assert!(signals.stop.get());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn resolves_stale_requested_models_against_offered_options() {
        let result = json!({
            "configOptions": [{
                "id": "model",
                "category": "model",
                "currentValue": "gemini-4-neon",
                "options": [
                    { "value": "gemini-4-neon", "name": "Gemini 4 Neon" },
                    { "value": "claude-sonnet-5-5", "name": "Claude Sonnet 5.5" },
                    { "value": "gpt-oss-120b-medium", "name": "GPT-OSS 120B (Medium)" }
                ]
            }]
        });
        let offered = offered_model_ids(&result);
        assert_eq!(
            resolve_requested_model("claude-sonnet-5-5", &offered),
            Some(("claude-sonnet-5-5", None))
        );
        // A standalone id that merely looks suffixed stays as listed.
        assert_eq!(
            resolve_requested_model("gpt-oss-120b-medium", &offered),
            Some(("gpt-oss-120b-medium", None))
        );
        // An id the catalog has since collapsed maps to its base and effort.
        assert_eq!(
            resolve_requested_model("claude-sonnet-5-5-medium", &offered),
            Some(("claude-sonnet-5-5", Some("medium")))
        );
        // A retired model is skipped instead of failing the handshake.
        assert_eq!(resolve_requested_model("gemini-2-pro", &offered), None);
        // Without an option list the request cannot be checked.
        assert_eq!(
            resolve_requested_model("gemini-2-pro", &[]),
            Some(("gemini-2-pro", None))
        );
    }

    #[test]
    fn clamps_thought_level_and_detects_auth_error() {
        let fixtures = samples();
        assert!(is_auth_error(&fixtures["auth_error"]["error"]));
        assert!(!is_auth_error(
            &json!({"code": -32602, "message": "Invalid params"})
        ));

        let flash_levels = vec!["low".into(), "medium".into(), "high".into()];
        assert_eq!(
            clamp_thought_level(Some("xhigh"), &flash_levels, Some("high")).as_deref(),
            Some("high")
        );
        assert_eq!(
            clamp_thought_level(Some("medium"), &flash_levels, Some("high")).as_deref(),
            Some("medium")
        );
        assert_eq!(clamp_thought_level(Some("high"), &[], Some("high")), None);
    }

    #[test]
    fn build_prompt_blocks_attaches_valid_images() {
        let dir = std::env::temp_dir().join(format!("raku-agy-img-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let img = dir.join("sample.png");
        std::fs::write(&img, b"\x89PNG\r\n\x1a\nfake").unwrap();

        let prompt = format!("Describe @{}", img.display());
        let blocks = build_prompt_blocks(&prompt, Path::new("/nonexistent"));
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["mimeType"], "image/png");
        assert!(blocks[1]["data"].as_str().is_some_and(|d| !d.is_empty()));

        // Project-relative mentions resolve against the session cwd, and a
        // path with spaces stays one mention, even when another follows it.
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets/mock up.png"), b"\x89PNG\r\n\x1a\nfake").unwrap();
        let prompt = format!(
            "Compare @assets/mock up.png @{} @missing.png @src/main.rs",
            img.display()
        );
        let blocks = build_prompt_blocks(&prompt, &dir);
        assert_eq!(blocks.len(), 3, "{blocks:?}");
        assert!(blocks[1..].iter().all(|block| block["type"] == "image"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn driver_handles_handshake_cancel_apply_options_and_eof_teardown() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("raku-agy-fake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-agy");
        std::fs::write(
            &script,
            r#"#!/bin/sh
if [ "$1" != "acp" ]; then exit 2; fi
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}'
      ;;
    *'"method":"session/new"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"fake-sess-1","configOptions":[{"id":"model","category":"model","currentValue":"gemini-4-neon","options":[{"value":"gemini-4-neon","name":"Gemini 4 Neon"},{"value":"gemini-3.8-flash","name":"Gemini 3.8 Flash"}]},{"id":"thought_level","category":"thought_level","currentValue":"high","options":[{"value":"low","name":"Low"},{"value":"medium","name":"Medium"},{"value":"high","name":"High"},{"value":"xhigh","name":"Extra"}]},{"id":"mode","category":"mode","currentValue":"default","options":[]}]}}'
      ;;
    *'"method":"session/set_config_option"'*)
      if printf '%s' "$line" | grep -q '"gemini-3.8-flash"'; then
        printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"configOptions":[{"id":"model","category":"model","currentValue":"gemini-3.8-flash","options":[]},{"id":"thought_level","category":"thought_level","currentValue":"high","options":[{"value":"low","name":"Low"},{"value":"medium","name":"Medium"},{"value":"high","name":"High"}]}]}}'
      else
        id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
        printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":[]}}\n' "$id"
      fi
      ;;
    *'"method":"session/prompt"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{"jsonrpc":"2.0","id":"perm-1","method":"session/request_permission","params":{"sessionId":"fake-sess-1","toolCall":{"toolCallId":"t1","name":"run_command","title":"echo hi","kind":"execute"},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]}}'
      # Wait for cancel or permission reply, then finish prompt
      IFS= read -r followup || break
      case "$followup" in
        *'"method":"session/cancel"'*)
          printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"cancelled"}}\n' "$id"
          ;;
        *)
          printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake-sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}}}'
          printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
          ;;
      esac
      ;;
  esac
done
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let (events, rx) = test_event_channel();
        let driver = AgyDriver::start(
            DriverStartOptions {
                binary: script,
                cwd: dir.clone(),
                mode: RuntimeMode::Ask,
                plan_mode: false,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                provider_cursor: None,
                read_only: false,
                subagent: Default::default(),
            },
            events,
        )
        .unwrap();

        // Wait for Connected
        let mut connected = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(200))
                && matches!(
                    ev,
                    DriverEvent::Connected {
                        provider_cursor: Some(ProviderResumeCursor::Antigravity { ref session_id })
                    } if session_id == "fake-sess-1"
                )
            {
                connected = true;
                break;
            }
        }
        assert!(connected, "expected Connected event");

        // apply_options: mode change forces restart, model/effort/plan_mode apply in-session
        assert!(!driver.apply_options(SessionOptions {
            mode: RuntimeMode::FullAccess,
            plan_mode: false,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
        }));
        assert!(driver.apply_options(SessionOptions {
            mode: RuntimeMode::Ask,
            plan_mode: true,
            model: Some("gemini-3.8-flash".into()),
            reasoning_effort: Some("xhigh".into()),
            service_tier: None,
            context_window: None,
        }));

        // Prompt -> receives Permission -> cancel() -> TurnFinished { success: false }
        driver.prompt("run something".into());
        let mut got_perm = false;
        let mut got_finish = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) {
                match ev {
                    DriverEvent::Permission { .. } => {
                        got_perm = true;
                        driver.cancel();
                    }
                    DriverEvent::TurnFinished { success: false, .. } => {
                        got_finish = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
        assert!(got_perm, "expected Permission event");
        assert!(got_finish, "expected TurnFinished after cancel");

        drop(driver);
        let mut exited = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(DriverEvent::ProcessExited) = rx.recv_timeout(Duration::from_millis(200)) {
                exited = true;
                break;
            }
        }
        assert!(exited, "expected ProcessExited on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn early_exit_during_handshake_unblocks_immediately_without_waiting_for_timeout() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("raku-agy-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("crash-agy");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'error: fatal boot failure' >&2\nexit 1\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let started = std::time::Instant::now();
        let (events, rx) = test_event_channel();
        let _driver = AgyDriver::start(
            DriverStartOptions {
                binary: script,
                cwd: dir.clone(),
                mode: RuntimeMode::Ask,
                plan_mode: false,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                provider_cursor: None,
                read_only: false,
                subagent: Default::default(),
            },
            events,
        )
        .unwrap();

        let mut saw_exit = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Ok(DriverEvent::ProcessExited) = rx.recv_timeout(Duration::from_millis(100)) {
                saw_exit = true;
                break;
            }
        }
        assert!(saw_exit, "expected ProcessExited quickly after early crash");
        assert!(started.elapsed() < Duration::from_secs(3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore]
    fn live_agy_acp_turn() {
        let binary = which_agy().unwrap_or_else(|| "agy".into());
        let cwd = std::env::temp_dir().join(format!("raku-agy-live-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&cwd);
        let (events, rx) = test_event_channel();
        let driver = AgyDriver::start(
            DriverStartOptions {
                binary,
                cwd: cwd.clone(),
                mode: RuntimeMode::FullAccess,
                plan_mode: false,
                model: Some("gemini-3.8-flash".into()),
                reasoning_effort: Some("low".into()),
                service_tier: None,
                context_window: None,
                agent_preset: None,
                provider_cursor: None,
                read_only: false,
                subagent: Default::default(),
            },
            events,
        )
        .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut connected = false;
        while std::time::Instant::now() < deadline {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(250)) {
                if let DriverEvent::Error(e) = &ev {
                    panic!("unexpected error during connect: {e}");
                }
                if matches!(ev, DriverEvent::Connected { .. }) {
                    connected = true;
                    break;
                }
            }
        }
        assert!(connected, "timed out waiting for Connected");

        driver.prompt("Reply with only the single word PONG.".into());
        let mut text = String::new();
        let mut finished = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        while std::time::Instant::now() < deadline {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(250)) {
                match ev {
                    DriverEvent::TextDelta(delta) => text.push_str(&delta),
                    DriverEvent::TurnFinished { success, summary } => {
                        assert!(success, "turn failed: {summary:?}");
                        finished = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
        assert!(finished, "timed out waiting for TurnFinished");
        assert!(!text.trim().is_empty(), "expected non-empty reply");
        drop(driver);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    fn which_agy() -> Option<std::path::PathBuf> {
        crate::model::provider_probe(ProviderKind::Antigravity, None).path
    }
}
