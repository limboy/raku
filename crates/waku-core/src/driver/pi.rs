//! Pi RPC transport, shared by Pi and Oh My Pi.
//!
//! Oh My Pi is a fork of Pi that kept the newline-delimited RPC transport but
//! renamed part of the surface: forking is `branch`, a run settles on
//! `agent_end` instead of `agent_settled`, and oversized frames are chunked
//! once protocol v2 is negotiated. [`PiFlavor`] carries those differences so
//! both providers share one transport instead of two near-copies.

use super::compaction::CompactionTracker;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use waku_protocol::compaction::{
    CompactionRequest, CompactionStatus, CompactionTrigger, ContextCompaction,
};

use anyhow::{Context as _, anyhow};
use crossbeam_channel::{Sender, bounded, unbounded};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::{activity, computer_use as computer_use_runtime};
use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{
    ActivityKind, DriverEvent, ExecutionPolicy, ProviderResumeCursor, ReportedCommand, RuntimeMode,
    unix_time_millis,
};

/// The tool allowlist a review subtask is launched with.
///
/// Both Pi and Oh My Pi take the same read-tool names, and an allowlist that
/// omits every mutating tool is stronger than an approval prompt nobody can
/// answer: the capability is not there to be requested.
const REVIEW_READ_ONLY_TOOLS: &str = "read,grep,glob,lsp,web_search";

/// The tool names the CLI itself ships with.
///
/// A parent session must keep every built-in tool and *add* Waku's two
/// delegation tools, but `--tools` is a strict allowlist that replaces the
/// defaults. The CLI prints its own list when asked for an unknown tool, which
/// is the only version-accurate source: Oh My Pi ships a different set from
/// Pi, and both grow over time.
fn builtin_tool_names(binary: &Path, cwd: &Path) -> Option<Vec<String>> {
    use std::collections::HashMap;
    use std::sync::LazyLock;

    // Keyed by working directory too: tool availability follows the project's
    // own configuration, so one project's answer is not another's.
    static TOOL_CACHE: LazyLock<parking_lot::Mutex<HashMap<(PathBuf, PathBuf), Vec<String>>>> =
        LazyLock::new(Default::default);
    let key = (binary.to_path_buf(), cwd.to_path_buf());

    if let Some(cached) = TOOL_CACHE.lock().get(&key) {
        return Some(cached.clone());
    }

    let output = crate::command_env::plain_command(binary)
        .args(["--mode", "rpc", "--tools", "-waku-delegation-probe"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let mut names = parse_builtin_tools(&text)?;

    // The first probe lists every tool the CLI knows; some of them are disabled
    // for this session and are rejected if named. A second probe with the list
    // reveals that difference, and the result is cached, so this costs one
    // extra process per binary per daemon lifetime.
    for _ in 0..4 {
        let Ok(output) = crate::command_env::plain_command(binary)
            .args(["--mode", "rpc", "--tools", &names.join(",")])
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .output()
        else {
            break;
        };
        let mut again = String::from_utf8_lossy(&output.stdout).into_owned();
        again.push_str(&String::from_utf8_lossy(&output.stderr));
        let unavailable = parse_unavailable_tools(&again);
        if unavailable.is_empty() {
            break;
        }
        // Each pass can reveal more disabled tools, so repeat until the CLI
        // accepts the list or stops naming any.
        names.retain(|name| !unavailable.contains(name));
        if names.is_empty() {
            return None;
        }
    }
    TOOL_CACHE.lock().insert(key, names.clone());
    Some(names)
}

/// Read the `Built-in tools:` list out of a CLI's unknown-tool diagnostic.
///
/// One line packs several lists: the CLI's own tools, then `Other registered
/// tools:` (extensions and MCP servers). Only the first is wanted — the second
/// names tools the session keeps whether or not they are allowed.
fn parse_builtin_tools(text: &str) -> Option<Vec<String>> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = normalized.split("Built-in tools:").nth(1)?;
    let builtins = split_tool_list(
        line.split("Other registered tools:")
            .next()
            .unwrap_or(line)
            .split("Built-in tools unavailable")
            .next()
            .unwrap_or(line),
    );
    (!builtins.is_empty()).then_some(builtins)
}

/// Read `Built-in tools unavailable in this session: …`, which the CLI reports
/// when a named tool exists but is disabled here. Naming one of those in
/// `--tools` is an error, so they must be dropped from the allowlist.
fn parse_unavailable_tools(text: &str) -> Vec<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized
        .split("Built-in tools unavailable in this session:")
        .nth(1)
        .or_else(|| {
            normalized
                .split("Built-in tool unavailable in this session:")
                .nth(1)
        })
        // The CLI follows the list with a usage hint; that prose must not be
        // read as part of the last tool name.
        .map(|list| list.split("Run `omp --help`").next().unwrap_or(list))
        .map(split_tool_list)
        .unwrap_or_default()
}

fn split_tool_list(text: &str) -> Vec<String> {
    text.trim()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches('.')
        .split(',')
        .map(str::trim)
        .map(|name| name.trim_end_matches('.'))
        .filter(|name| !name.is_empty() && name.chars().all(is_tool_name_char))
        .map(str::to_owned)
        .collect()
}

/// Tool names are identifiers; anything else in the diagnostic is prose that
/// must not end up in an allowlist.
fn is_tool_name_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | ':')
}

/// The delegation tools the extension registers, named so a parent session can
/// enable them explicitly.
const DELEGATION_TOOL_NAMES: [&str; 2] = ["delegate_batch", "wait_delegates"];

/// The arguments a review subtask is launched with. No `--approve` / `--yolo`:
/// those would auto-approve exactly the writes the allowlist removes.
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

/// Oh My Pi has to start a whole second agent to clone a session, so it needs
/// more headroom than a request against the already-running process.
const CLONE_TIMEOUT: Duration = Duration::from_secs(30);

/// Oh My Pi refuses to reassemble beyond this, so neither should Waku.
const MAX_REASSEMBLED_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Only a possible standalone period waits for the completed message. Prose
/// streams immediately, and even malformed whitespace streams stay bounded.
const MAX_PENDING_PLACEHOLDER_BYTES: usize = 256;

/// Which dialect of the Pi RPC protocol a session speaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PiFlavor {
    Pi,
    OhMyPi,
}

impl PiFlavor {
    fn display_name(self) -> &'static str {
        match self {
            Self::Pi => "Pi",
            Self::OhMyPi => "Oh My Pi",
        }
    }

    /// Pi has no permission system and only needs project-local files trusted;
    /// Oh My Pi does have one, and Waku only ever runs these in Full access.
    fn full_access_arg(self) -> &'static str {
        match self {
            Self::Pi => "--approve",
            Self::OhMyPi => "--yolo",
        }
    }

    /// The event that means the run is over and no more work is scheduled.
    fn settled_event(self) -> &'static str {
        match self {
            Self::Pi => "agent_settled",
            Self::OhMyPi => "agent_end",
        }
    }

    fn session_info_event(self) -> &'static str {
        match self {
            Self::Pi => "session_info_changed",
            Self::OhMyPi => "session_info_update",
        }
    }

    fn session_info_title_field(self) -> &'static str {
        match self {
            Self::Pi => "name",
            Self::OhMyPi => "title",
        }
    }

    fn branch_messages_command(self) -> &'static str {
        match self {
            Self::Pi => "get_fork_messages",
            Self::OhMyPi => "get_branch_messages",
        }
    }

    fn branch_command(self) -> &'static str {
        match self {
            Self::Pi => "fork",
            Self::OhMyPi => "branch",
        }
    }

    /// Oh My Pi dropped Pi's opt-out env var; it gates its update check on a
    /// setting instead, and does it off the startup path either way.
    fn skips_version_check_by_env(self) -> bool {
        matches!(self, Self::Pi)
    }

    /// Only Oh My Pi chunks oversized frames, and only after it is asked to.
    fn negotiates_protocol_v2(self) -> bool {
        matches!(self, Self::OhMyPi)
    }

    /// Waku's computer-use bridge is a Pi extension written against Pi's
    /// extension API. Oh My Pi ships its own `/computer` instead.
    fn supports_waku_computer_use(self) -> bool {
        matches!(self, Self::Pi)
    }

    fn cursor(self, session_id: String, session_file: Option<PathBuf>) -> ProviderResumeCursor {
        match self {
            Self::Pi => ProviderResumeCursor::Pi {
                session_id,
                session_file,
            },
            Self::OhMyPi => ProviderResumeCursor::OhMyPi {
                session_id,
                session_file,
            },
        }
    }

    fn session_file_from_cursor(self, cursor: &ProviderResumeCursor) -> Option<&PathBuf> {
        match (self, cursor) {
            (Self::Pi, ProviderResumeCursor::Pi { session_file, .. })
            | (Self::OhMyPi, ProviderResumeCursor::OhMyPi { session_file, .. }) => {
                session_file.as_ref()
            }
            _ => None,
        }
    }

    fn owns_cursor(self, cursor: &ProviderResumeCursor) -> bool {
        matches!(
            (self, cursor),
            (Self::Pi, ProviderResumeCursor::Pi { .. })
                | (Self::OhMyPi, ProviderResumeCursor::OhMyPi { .. })
        )
    }
}

enum CommandMessage {
    Compact(CompactionRequest),
    AutoTitle {
        request: AutoTitleRequest,
        phase: AutoTitlePhase,
    },
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
    AutoTitle {
        request: AutoTitleRequest,
        phase: AutoTitlePhase,
    },
    Request(Sender<Result<Value, String>>),
    Prompt,
}

type PendingResponses = Arc<Mutex<HashMap<String, PendingResponse>>>;

#[derive(Clone, Copy)]
enum AutoTitlePhase {
    CheckName,
    Generate,
}

#[derive(Clone)]
struct AutoTitleRequest {
    session_id: String,
    started: Instant,
}

/// OMP enables automatic naming only in its TUI. RPC hosts have to ask its
/// native `/rename` command to use the configured title model themselves.
/// This state belongs to the native session, not to an individual agent turn:
/// the background rename can finish after `agent_end`.
#[derive(Clone, Default)]
struct PiAutoTitle {
    state: Arc<Mutex<PiAutoTitleState>>,
    disabled: bool,
}

#[derive(Default)]
struct PiAutoTitleState {
    session_id: Option<String>,
    named: bool,
    attempted: bool,
    in_flight: Option<Instant>,
    suppress_rename_output: bool,
}

impl PiAutoTitle {
    fn mark_generating(&self) {
        let mut state = self.state.lock();
        state.suppress_rename_output = true;
    }

    fn observe_state(&self, flavor: PiFlavor, response: &Value) {
        if flavor != PiFlavor::OhMyPi || self.disabled {
            return;
        }
        let Some(session_id) = response.pointer("/data/sessionId").and_then(Value::as_str) else {
            return;
        };
        let mut state = self.state.lock();
        if state.session_id.as_deref() != Some(session_id) {
            *state = PiAutoTitleState {
                session_id: Some(session_id.to_owned()),
                ..Default::default()
            };
        }
        state.named = response
            .pointer("/data/sessionName")
            .and_then(Value::as_str)
            .is_some_and(|title| !title.trim().is_empty());
    }

    fn prepare_prompt(&self, prompt: &str) {
        let mut state = self.state.lock();
        if prompt.split_whitespace().next() == Some("/rename") {
            // The explicit command owns both the title and its output. OMP's
            // title revision also invalidates an older background rename.
            state.in_flight = None;
            state.suppress_rename_output = false;
            state.attempted = true;
        } else {
            state.attempted = false;
            if state
                .in_flight
                .is_some_and(|started| started.elapsed() >= Duration::from_secs(60))
            {
                state.in_flight = None;
                state.suppress_rename_output = false;
            }
        }
    }

    fn cancel(&self) {
        let mut state = self.state.lock();
        state.in_flight = None;
        state.attempted = true;
    }

    fn start_restored(&self, response: &Value) -> Option<AutoTitleRequest> {
        response
            .pointer("/data/messageCount")
            .and_then(Value::as_u64)
            .filter(|count| *count > 0)?;
        self.start()
    }

    fn start(&self) -> Option<AutoTitleRequest> {
        if self.disabled {
            return None;
        }
        let mut state = self.state.lock();
        if state.named || state.attempted || state.in_flight.is_some() {
            return None;
        }
        let session_id = state.session_id.clone()?;
        let started = Instant::now();
        state.attempted = true;
        state.in_flight = Some(started);
        Some(AutoTitleRequest {
            session_id,
            started,
        })
    }

    fn is_current(&self, request: &AutoTitleRequest) -> bool {
        let state = self.state.lock();
        state.session_id.as_deref() == Some(&request.session_id)
            && state.in_flight == Some(request.started)
    }

    fn finish(&self, request: &AutoTitleRequest) {
        let mut state = self.state.lock();
        if state.session_id.as_deref() == Some(&request.session_id)
            && state.in_flight == Some(request.started)
        {
            state.in_flight = None;
            state.suppress_rename_output = false;
        }
    }

    fn observe_title(&self, session_id: Option<&str>, title: Option<&str>) -> bool {
        let mut state = self.state.lock();
        if let (Some(received), Some(current)) = (session_id, state.session_id.as_deref())
            && received != current
        {
            return false;
        }
        state.named = title.is_some();
        if title.is_some() {
            state.in_flight = None;
        }
        true
    }

    fn consume_output(&self, text: &str) -> bool {
        let mut state = self.state.lock();
        // RPC command_output has no request id. Match only OMP's native
        // rename reports so unrelated user commands continue to render.
        let renamed = text.starts_with("Session renamed to ");
        if !renamed
            && text != "Could not generate a session title. Use /rename <title> to set one."
            && text != "Session name not changed (a user-set name takes precedence)."
            && !text.starts_with("Rename failed: ")
        {
            return false;
        }
        if !state.suppress_rename_output && state.in_flight.is_none() {
            return false;
        }
        state.suppress_rename_output = false;
        state.in_flight = None;
        if !renamed {
            eprintln!("Oh My Pi automatic title: {text}");
        }
        true
    }
}

fn sanitize_ohmypi_title(title: &str) -> Option<String> {
    let sanitized: String = title
        .chars()
        .filter(|c| !matches!(c, '\u{e000}'..='\u{f8ff}' | '\u{f0000}'..='\u{10ffff}'))
        .collect();
    let trimmed = sanitized.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

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
    computer_use: Option<computer_use_runtime::ComputerUseRuntime>,
}

/// Give the parent agent Waku's delegation tools.
///
/// Pi cannot inherit an MCP server the way Codex does, so the tools arrive as
/// an extension that spawns `waku-daemon mcp-agent-tools` itself; the session
/// registration file tells that bridge whose turn this is. Without the
/// extension the task still runs, so a missing asset is reported and skipped
/// rather than failing the launch.
fn configure_pi_delegation_command(
    command: &mut std::process::Command,
    flavor: PiFlavor,
    binary: &Path,
    cwd: &Path,
    session_path: Option<&Path>,
    extension: impl FnOnce() -> anyhow::Result<PathBuf>,
) {
    let Some(session_path) = session_path else {
        return;
    };
    match extension() {
        Ok(extension) => {
            let daemon = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("waku-daemon"));
            // `--tools` replaces the default tool set, so it must carry the
            // CLI's own built-ins as well as ours — otherwise enabling
            // delegation would silently take away editing and shell access.
            if let Some(mut tools) = builtin_tool_names(binary, cwd) {
                tools.extend(DELEGATION_TOOL_NAMES.iter().map(|name| (*name).to_owned()));
                command.arg("--tools").arg(tools.join(","));
            } else {
                eprintln!(
                    "{} cannot list its built-in tools, so delegation stays unavailable",
                    flavor.display_name()
                );
            }
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

fn configure_pi_computer_use_command(
    command: &mut std::process::Command,
    config: Option<(&computer_use_runtime::ComputerUseConfig, &Path)>,
) {
    if let Some((config, extension)) = config {
        command
            .arg("--extension")
            .arg(extension)
            .arg("--skill")
            .arg(&config.skill_path)
            .env("WAKU_JS_REPL_SERVER", &config.repl_path)
            .env("WAKU_COMPUTER_USE_SERVER", &config.server_path)
            .env(
                "WAKU_COMPUTER_USE_PROCESS_DIRECTORY",
                &config.process_directory,
            );
    }
}

impl PiDriver {
    pub fn start(
        flavor: PiFlavor,
        options: DriverStartOptions,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
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
            computer_use_enabled,
            provider_cursor,
            delegation_session_path,
            delegation_registry_dir: _,
        } = options;
        // A review subtask runs unattended, so it never gets the full-access
        // launch: the tool allowlist below is what makes it read-only, and it
        // is enforced by the CLI refusing to expose anything else.
        let review_read_only = execution_policy == ExecutionPolicy::ReviewReadOnly;
        if !review_read_only && mode != RuntimeMode::FullAccess {
            return Err(anyhow!(
                "{} currently supports Full access only",
                flavor.display_name()
            ));
        }
        if review_read_only && computer_use_enabled {
            return Err(anyhow!(
                "{} cannot run Computer Use in a review subtask",
                flavor.display_name()
            ));
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

        let computer_use = (computer_use_enabled && flavor.supports_waku_computer_use())
            .then(|| computer_use_runtime::ComputerUseRuntime::start(events.clone()))
            .transpose()?;
        let pi_extension = computer_use
            .as_ref()
            .map(|_| crate::computer_use::pi_extension_path())
            .transpose()?;
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
        configure_pi_computer_use_command(
            &mut command,
            computer_use
                .as_ref()
                .zip(pi_extension.as_deref())
                .map(|(runtime, extension)| (&runtime.config, extension)),
        );
        configure_pi_delegation_command(
            &mut command,
            flavor,
            &binary,
            &cwd,
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
        let auto_title = PiAutoTitle {
            disabled: review_read_only,
            ..Default::default()
        };
        let reader_auto_title = auto_title.clone();
        let reader_pending = pending.clone();
        let reader_commands = commands.clone();
        let reader_events = events.clone();
        let reader_thread =
            thread::Builder::new()
                .name("waku-pi-reader".into())
                .spawn(move || {
                    let mut stream_state = PiStreamState {
                        auto_title: reader_auto_title,
                        ..Default::default()
                    };
                    let mut chunks = ChunkAssembly::default();
                    for line in BufReader::new(stdout).lines() {
                        match line {
                            Ok(line) if !line.trim().is_empty() => {
                                match serde_json::from_str::<Value>(&line) {
                                    Ok(value) => {
                                        // A chunked frame arrives as an
                                        // uninterrupted run of `rpc_chunk`
                                        // envelopes that reassemble into one
                                        // logical message.
                                        match chunks.accept(value) {
                                            Ok(Some(value)) => handle_pi_message(
                                                flavor,
                                                value,
                                                &reader_pending,
                                                &reader_commands,
                                                &reader_events,
                                                &mut stream_state,
                                            ),
                                            Ok(None) => {}
                                            Err(error) => {
                                                let _ =
                                                    reader_events.send(DriverEvent::Error(tr!(
                                                        "errors.provider_transport_read",
                                                        provider = flavor.display_name(),
                                                        error = error
                                                    )));
                                            }
                                        }
                                    }
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
                    // The agent does not answer until it has finished loading,
                    // so every handshake request gets the startup timeout.
                    // Negotiate before anything else so a large first response
                    // arrives chunked rather than shrunk to an error frame.
                    if flavor.negotiates_protocol_v2() {
                        send_request_with_timeout(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({"type": "negotiate_protocol", "protocolVersion": 2}),
                            INITIALIZE_TIMEOUT,
                        )?;
                    }
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
                auto_title.observe_state(flavor, &state);
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
                    let title = if flavor == PiFlavor::OhMyPi {
                        sanitize_ohmypi_title(title)
                    } else {
                        Some(title.to_owned())
                    };
                    let _ =
                        writer_events.send(DriverEvent::AutoTitleUpdated(title));
                }

                // Repair older unnamed conversations when their native
                // history is reattached, without waiting for another reply.
                // The session state was already verified to be unnamed above.
                if let Some(request) = auto_title.start_restored(&state)
                    && let Err(error) = send_auto_title_request(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        &auto_title,
                        request.clone(),
                        AutoTitlePhase::Generate,
                    )
                {
                    auto_title.finish(&request);
                    eprintln!("Oh My Pi automatic title: {error}");
                }

                // Pi reports extension commands, prompts and skills on request;
                // Oh My Pi pushes its registry at startup and after reloads.
                if flavor == PiFlavor::Pi
                    && let Ok(catalog) = send_request(
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
                        CommandMessage::AutoTitle { request, phase } => {
                            if !auto_title.is_current(&request) {
                                continue;
                            }
                            if matches!(phase, AutoTitlePhase::Generate) && auto_title.state.lock().named {
                                auto_title.finish(&request);
                                continue;
                            }
                            if let Err(error) = send_auto_title_request(
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                &auto_title,
                                request.clone(),
                                phase,
                            ) {
                                auto_title.finish(&request);
                                eprintln!("Oh My Pi automatic title: {error}");
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
                            auto_title.prepare_prompt(&prompt);
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
                            auto_title.cancel();
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
                                &binary,
                                &cwd,
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
                                &binary,
                                &cwd,
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

        Ok(Self {
            flavor,
            commands,
            computer_use,
        })
    }
}

impl DriverControl for PiDriver {
    fn compact(&self, request: CompactionRequest) {
        let _ = self.commands.send(CommandMessage::Compact(request));
    }

    fn prompt(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Prompt(prompt));
    }

    fn supports_steer(&self) -> bool {
        true
    }

    fn steer(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Steer(prompt));
    }

    fn cancel(&self) {
        let _ = self.commands.send(CommandMessage::Cancel);
    }

    fn cancel_computer_use(&self) {
        if let Some(computer_use) = self.computer_use.as_ref() {
            computer_use.stop();
        }
    }

    fn respond(&self, _request_id: String, _option_id: String) {}

    fn apply_options(&self, options: SessionOptions) -> bool {
        // Both flavors have setters for the model and thinking level, so those
        // apply to the live session. Neither exposes one for permissions — and
        // Waku only runs them with Full access anyway, so a mode change asks
        // for a fresh start, which is where that is reported.
        if options.mode != RuntimeMode::FullAccess {
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
        self.cancel_computer_use();
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

fn send_auto_title_request(
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    auto_title: &PiAutoTitle,
    request: AutoTitleRequest,
    phase: AutoTitlePhase,
) -> Result<(), String> {
    if matches!(phase, AutoTitlePhase::Generate) {
        auto_title.mark_generating();
    }
    *next_request_id += 1;
    let id = format!("waku-{}", next_request_id);
    pending
        .lock()
        .insert(id.clone(), PendingResponse::AutoTitle { request, phase });
    let mut request = match phase {
        AutoTitlePhase::CheckName => json!({"type": "get_state"}),
        AutoTitlePhase::Generate => json!({"type": "prompt", "message": "/rename"}),
    };
    request["id"] = json!(id);
    // Both the name check and /rename admission are asynchronous. Waiting for
    // the title model here would keep the writer from sending Stop or steer.
    if let Err(error) = write_json_line(stdin, &request) {
        pending.lock().remove(&id);
        return Err(format!("transport write failed: {error}"));
    }
    Ok(())
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
                PendingResponse::Request(_)
                    | PendingResponse::Compact(_)
                    | PendingResponse::AutoTitle { .. }
            )
        });
        pending.insert(id.clone(), PendingResponse::Prompt);
    }
    // OMP built-ins can hold the prompt response until compaction or another
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

/// Reassembles the `rpc_chunk` runs Oh My Pi emits for frames over its 1 MiB
/// stdout ceiling. Without this a large tool result degrades to an error frame
/// and the activity row renders empty.
#[derive(Default)]
struct ChunkAssembly {
    active: Option<PendingChunks>,
}

struct PendingChunks {
    chunk_id: String,
    count: u64,
    next_index: u64,
    byte_length: usize,
    data: Vec<u8>,
}

impl ChunkAssembly {
    /// Returns the logical message to dispatch, or `None` while a chunked
    /// frame is still arriving.
    fn accept(&mut self, value: Value) -> Result<Option<Value>, String> {
        if value.get("type").and_then(Value::as_str) != Some("rpc_chunk") {
            // The run must be uninterrupted, so anything else invalidates a
            // partial frame rather than silently splicing around it.
            if self.active.take().is_some() {
                return Err("chunked frame was interrupted".to_owned());
            }
            return Ok(Some(value));
        }
        let (chunk_id, index, count, byte_length, data) = (|| {
            Some((
                value.get("chunkId").and_then(Value::as_str)?,
                value.get("index").and_then(Value::as_u64)?,
                value.get("count").and_then(Value::as_u64)?,
                value.get("byteLength").and_then(Value::as_u64)?,
                value.get("data").and_then(Value::as_str)?,
            ))
        })()
        .ok_or_else(|| "chunk frame was malformed".to_owned())?;
        let byte_length = usize::try_from(byte_length)
            .map_err(|_| "chunked frame exceeds the reassembly limit".to_owned())?;
        if count == 0 || index >= count {
            self.active = None;
            return Err("chunk frame was malformed".to_owned());
        }
        if byte_length > MAX_REASSEMBLED_FRAME_BYTES {
            self.active = None;
            return Err("chunked frame exceeds the reassembly limit".to_owned());
        }
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
            .map_err(|error| format!("chunk payload was not valid base64: {error}"))?;

        let pending = match self.active.take() {
            Some(pending)
                if pending.chunk_id == chunk_id
                    && pending.count == count
                    && pending.byte_length == byte_length
                    && pending.next_index == index =>
            {
                pending
            }
            Some(_) => {
                return Err("chunked frame was interrupted".to_owned());
            }
            None if index == 0 => PendingChunks {
                chunk_id: chunk_id.to_owned(),
                count,
                next_index: 0,
                byte_length,
                data: Vec::with_capacity(byte_length),
            },
            None => return Err("chunked frame started mid-sequence".to_owned()),
        };
        let mut pending = pending;
        pending.data.extend_from_slice(&decoded);
        pending.next_index += 1;
        if pending.data.len() > pending.byte_length {
            return Err("chunked frame overran its declared length".to_owned());
        }
        if pending.next_index < pending.count {
            self.active = Some(pending);
            return Ok(None);
        }
        if pending.data.len() != pending.byte_length {
            return Err("chunked frame did not match its declared length".to_owned());
        }
        let text = String::from_utf8(pending.data)
            .map_err(|_| "chunked frame was not valid UTF-8".to_owned())?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| format!("chunked frame was not valid JSON: {error}"))
    }
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

#[allow(clippy::too_many_arguments)]
fn fork_pi_session(
    flavor: PiFlavor,
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    binary: &Path,
    cwd: &Path,
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

    // Keeping every turn is a whole-session copy, which Pi does in place and
    // Oh My Pi only does at launch. The out-of-process copy leaves this
    // session untouched, so it never needs restoring afterwards.
    if turns_to_remove == 0 && flavor == PiFlavor::OhMyPi {
        let session_file = flavor
            .session_file_from_cursor(original_cursor)
            .ok_or_else(|| format!("{name}'s original session file is unavailable"))?;
        return clone_ohmypi_session(binary, cwd, session_file);
    }

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

/// Copies a whole Oh My Pi session by launching a throwaway agent with
/// `--fork`, which is the only place it exposes a full-session copy, then
/// reading back the session the copy landed in.
fn clone_ohmypi_session(
    binary: &Path,
    cwd: &Path,
    session_file: &Path,
) -> Result<ProviderResumeCursor, String> {
    let mut command = crate::command_env::command(binary);
    let command = command
        .args(["--mode", "rpc"])
        .arg(PiFlavor::OhMyPi.full_access_arg())
        .arg("--fork")
        .arg(session_file)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::command_env::spawn(command)
        .map_err(|error| format!("could not start Oh My Pi to copy the session: {error}"))?;
    let result = (|| {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Oh My Pi stdin unavailable".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Oh My Pi stdout unavailable".to_owned())?;
        let (tx, rx) = bounded(1);
        thread::Builder::new()
            .name("waku-ohmypi-clone".into())
            .spawn(move || {
                let mut chunks = ChunkAssembly::default();
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let Ok(Some(value)) = chunks.accept(value) else {
                        continue;
                    };
                    if value.get("id").and_then(Value::as_str) == Some("waku-clone") {
                        let _ = tx.send(value);
                        break;
                    }
                }
            })
            .map_err(|error| format!("could not read the Oh My Pi session copy: {error}"))?;
        write_json_line(
            &mut stdin,
            &json!({"id": "waku-clone", "type": "get_state"}),
        )
        .map_err(|error| format!("could not ask Oh My Pi for the copied session: {error}"))?;
        let state = rx
            .recv_timeout(CLONE_TIMEOUT)
            .map_err(|_| "timed out waiting for Oh My Pi to copy the session".to_owned())?;
        if state.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(state
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Oh My Pi could not copy the session")
                .to_owned());
        }
        cursor_from_state(PiFlavor::OhMyPi, &state)
            .ok_or_else(|| "Oh My Pi did not report the copied session cursor".to_owned())
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[derive(Default)]
struct PiStreamState {
    auto_title: PiAutoTitle,
    compaction: CompactionTracker,
    run_started: bool,
    message_saw_text: bool,
    message_saw_reasoning: bool,
    message_is_deepseek: bool,
    message_pending_text: String,
    reasoning_blocks: HashMap<u64, (String, bool)>,
    failed: bool,
    tools: HashMap<String, (ActivityKind, String)>,
}

impl PiStreamState {
    fn reset_turn(&mut self) {
        *self = Self {
            auto_title: self.auto_title.clone(),
            ..Default::default()
        };
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
        let title_response = {
            let mut pending = pending.lock();
            if matches!(pending.get(id), Some(PendingResponse::AutoTitle { .. })) {
                pending.remove(id)
            } else {
                None
            }
        };
        if let Some(PendingResponse::AutoTitle { request, phase }) = title_response {
            if !state.auto_title.is_current(&request) {
                return;
            }
            if value.get("success").and_then(Value::as_bool) != Some(true) {
                state.auto_title.finish(&request);
                eprintln!(
                    "Oh My Pi automatic title: {}",
                    value.get("error").unwrap_or(&Value::Null)
                );
                return;
            }
            if let AutoTitlePhase::CheckName = phase {
                if value.pointer("/data/sessionId").and_then(Value::as_str)
                    != Some(&request.session_id)
                {
                    state.auto_title.finish(&request);
                    return;
                }
                state.auto_title.observe_state(flavor, &value);
                if let Some(title) = value
                    .pointer("/data/sessionName")
                    .and_then(Value::as_str)
                    .filter(|title| !title.trim().is_empty())
                {
                    state.auto_title.finish(&request);
                    let title = if flavor == PiFlavor::OhMyPi {
                        sanitize_ohmypi_title(title)
                    } else {
                        Some(title.to_owned())
                    };
                    let _ = events.send(DriverEvent::AutoTitleUpdated(title));
                } else {
                    let _ = commands.send(CommandMessage::AutoTitle {
                        request,
                        phase: AutoTitlePhase::Generate,
                    });
                }
            }
            return;
        }
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
                // OMP may acknowledge a prompt before reporting that an
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
            if value.get("command").and_then(Value::as_str) == Some("get_state") {
                state.auto_title.observe_state(flavor, &value);
            }
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

    if event_type == "available_commands_update" {
        publish_commands(
            crate::slash_command_catalog::parse_oh_my_pi_commands(&value),
            events,
        );
        return;
    }

    if event_type == flavor.session_info_event() {
        let title = value
            .get(flavor.session_info_title_field())
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty());
        let title = if flavor == PiFlavor::OhMyPi {
            title.and_then(sanitize_ohmypi_title)
        } else {
            title.map(str::to_owned)
        };
        if flavor == PiFlavor::OhMyPi
            && !state.auto_title.observe_title(
                value.get("sessionId").and_then(Value::as_str),
                title.as_deref(),
            )
        {
            return;
        }
        let _ = events.send(DriverEvent::AutoTitleUpdated(title));
        return;
    }

    // Oh My Pi reuses `agent_end` for intermediate settles, flagging the real
    // one with `isTerminal`. Anything else here would end the turn early while
    // maintenance or async delivery still has work queued.
    if event_type == flavor.settled_event() {
        if value.get("isTerminal").and_then(Value::as_bool) == Some(false) {
            return;
        }
        flush_pending_message_text(events, state);
        if state.run_started {
            pending.lock().retain(|_, response| {
                matches!(
                    response,
                    PendingResponse::Request(_)
                        | PendingResponse::Compact(_)
                        | PendingResponse::AutoTitle { .. }
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
        "compaction_start" | "auto_compaction_start" => {
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
                _ => None,
            };
            state.compaction.start(None, trigger, events);
        }
        "compaction_end" | "auto_compaction_end" => {
            let status = if value.get("aborted").and_then(Value::as_bool) == Some(true) {
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
        }
        "command_output" => {
            if flavor == PiFlavor::OhMyPi
                && value
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| state.auto_title.consume_output(text))
            {
                return;
            }
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
                if flavor == PiFlavor::OhMyPi
                    && let Some(request) = state.auto_title.start()
                {
                    let _ = commands.send(CommandMessage::AutoTitle {
                        request,
                        phase: AutoTitlePhase::CheckName,
                    });
                }
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
            let tool_name = value.get("toolName").and_then(Value::as_str);
            let (kind, mut title) = id
                .as_ref()
                .and_then(|id| state.tools.get(id))
                .cloned()
                .unwrap_or_else(|| {
                    tool_name
                        .map(|tool_name| (classify_tool(tool_name), tool_title(tool_name)))
                        .unwrap_or_else(|| (ActivityKind::Tool, tr!("activity.tool")))
                });
            if event_type == "tool_execution_start"
                && let Some(input_title) = activity::input_title(value.get("args"))
            {
                title = input_title;
            }
            if event_type == "tool_execution_start"
                && let Some(id) = id.as_ref()
            {
                state.tools.insert(id.clone(), (kind, title.clone()));
            }
            let arguments = (event_type == "tool_execution_start")
                .then(|| value.get("args"))
                .flatten();
            let output = match event_type {
                "tool_execution_update" => value.get("partialResult"),
                "tool_execution_end" => value.get("result"),
                _ => None,
            };
            let complete = event_type == "tool_execution_end";
            let failed = value.get("isError").and_then(Value::as_bool) == Some(true);
            let item = activity::tool_activity(
                id.clone(),
                kind,
                title,
                arguments,
                output,
                output,
                failed,
                complete,
            )
            .with_tool_name(tool_name);
            let _ = events.send(DriverEvent::RichActivity(item));
            if complete && let Some(id) = id {
                state.tools.remove(&id);
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
    use crossbeam_channel::TryRecvError;

    #[test]
    fn native_compaction_response_is_metadata_and_never_an_assistant_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
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
    }

    #[test]
    fn cancelled_manual_compaction_response_preserves_interrupted_status() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, received) = unbounded();
        let item = ContextCompaction::new("manual", Some(CompactionTrigger::Manual), 1);
        state.compaction.seed(item.clone());
        pending
            .lock()
            .insert("rpc-compact".into(), PendingResponse::Compact(item));
        interrupt_pending_compactions(&pending, &events);
        handle_pi_message(
            PiFlavor::OhMyPi,
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
    }

    #[test]
    fn live_command_updates_reach_the_composer_and_clear_removed_commands() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for entries in [
            json!([
                {"name": "compact", "source": "builtin", "description": "Compact context"},
                {"name": "skill:verify", "source": "skill", "description": "Verify changes"}
            ]),
            json!([]),
        ] {
            handle_pi_message(
                PiFlavor::OhMyPi,
                json!({"type": "available_commands_update", "commands": entries}),
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        let DriverEvent::AvailableCommands(reported) = event_rx.recv().unwrap() else {
            panic!("missing command update")
        };
        assert_eq!(
            reported
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["compact", "verify"]
        );
        assert_eq!(reported[0].description, "Compact context");
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::AvailableCommands(commands) if commands.is_empty())
        );
        assert!(!state.run_started);
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
            json!({"type": "agent_end", "isTerminal": true}),
        ] {
            handle_pi_message(
                PiFlavor::OhMyPi,
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
            PiFlavor::Pi,
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
                computer_use_enabled: false,
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
            computer_use: None,
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
    fn pi_computer_use_uses_only_session_scoped_extension_and_skill_arguments() {
        let config = computer_use_runtime::ComputerUseConfig {
            server_path: PathBuf::from("/tmp/Waku Computer Use"),
            repl_path: PathBuf::from("/Applications/Waku.app/Resources/waku_js_repl"),
            skill_path: PathBuf::from("/Applications/Waku.app/Resources/skills/SKILL.md"),
            process_directory: PathBuf::from("/tmp/waku-computer-use/session"),
        };
        let mut command = std::process::Command::new("pi");

        configure_pi_computer_use_command(
            &mut command,
            Some((
                &config,
                Path::new("/Applications/Waku.app/Resources/computer-use/pi-extension.ts"),
            )),
        );

        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            [
                "--extension",
                "/Applications/Waku.app/Resources/computer-use/pi-extension.ts",
                "--skill",
                "/Applications/Waku.app/Resources/skills/SKILL.md",
            ]
        );
        let environment = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(
            environment.get("WAKU_JS_REPL_SERVER"),
            Some(&Some(
                "/Applications/Waku.app/Resources/waku_js_repl".into()
            ))
        );
        assert_eq!(
            environment.get("WAKU_COMPUTER_USE_PROCESS_DIRECTORY"),
            Some(&Some("/tmp/waku-computer-use/session".into()))
        );
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

    /// Drives the installed Oh My Pi RPC through one real provider turn.
    /// Ignored by default because it needs the CLI, credentials, and network.
    #[test]
    #[ignore = "requires an installed, authenticated omp"]
    fn ohmypi_context_usage_against_the_real_rpc() {
        let binary = crate::command_env::find_executable("omp").expect("omp is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = PiDriver::start(
            PiFlavor::OhMyPi,
            DriverStartOptions {
                binary,
                cwd: std::env::temp_dir(),
                mode: RuntimeMode::FullAccess,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                execution_policy: Default::default(),
                computer_use_enabled: false,
                provider_cursor: None,
                delegation_session_path: None,
                delegation_registry_dir: None,
            },
            events,
        )
        .expect("the Oh My Pi RPC session should start");

        let mut cursor = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(60)) {
            match event {
                DriverEvent::Connected { provider_cursor } => {
                    cursor = provider_cursor;
                    break;
                }
                DriverEvent::Error(error) => panic!("Oh My Pi failed to initialize: {error}"),
                _ => {}
            }
        }
        assert!(
            matches!(
                cursor,
                Some(ProviderResumeCursor::OhMyPi {
                    session_file: Some(_),
                    ..
                })
            ),
            "Oh My Pi should report its own cursor with a session file, got {cursor:?}"
        );

        driver.prompt("Reply with exactly: OK. Do not use any tools.".into());
        let mut finished = false;
        let mut context_tokens = None;
        let mut context_window = None;
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
                    assert!(success, "Oh My Pi should finish the probe turn");
                    finished = true;
                    break;
                }
                DriverEvent::Error(error) => panic!("Oh My Pi reported: {error}"),
                _ => {}
            }
        }

        assert!(finished, "Oh My Pi never settled the probe turn");
        assert!(context_tokens.is_some_and(|tokens| tokens > 0));
        assert!(context_window.is_some_and(|window| window > 0));
    }

    #[test]
    #[ignore = "requires an installed, authenticated omp"]
    fn ohmypi_automatic_title_against_the_real_rpc() {
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = PiDriver::start(
            PiFlavor::OhMyPi,
            DriverStartOptions {
                binary: crate::command_env::find_executable("omp").expect("omp is not installed"),
                cwd: std::env::temp_dir(),
                mode: RuntimeMode::FullAccess,
                model: std::env::var("WAKU_TEST_OMP_MODEL").ok(),
                reasoning_effort: Some("low".into()),
                service_tier: None,
                context_window: None,
                agent_preset: None,
                execution_policy: Default::default(),
                computer_use_enabled: false,
                provider_cursor: None,
                delegation_session_path: None,
                delegation_registry_dir: None,
            },
            events,
        )
        .expect("the Oh My Pi RPC session should start");
        let prompt = "请用一句话解释怎样优化长对话列表的滚动性能，不要使用任何工具。";
        driver.prompt(prompt.into());
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut title = None;
        let mut native_file = None;
        let mut transcript = String::new();
        let mut starts = 0;
        let mut finishes = 0;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            let Ok(event) = event_rx.recv_timeout(remaining) else {
                break;
            };
            match event {
                DriverEvent::Connected {
                    provider_cursor: Some(ProviderResumeCursor::OhMyPi { session_file, .. }),
                } => native_file = session_file,
                DriverEvent::AutoTitleUpdated(Some(value)) => title = Some(value),
                DriverEvent::TextDelta(value) => transcript.push_str(&value),
                DriverEvent::TurnStarted => starts += 1,
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "Oh My Pi should finish the probe turn");
                    finishes += 1;
                }
                DriverEvent::Error(error) => panic!("Oh My Pi reported: {error}"),
                _ => {}
            }
            if title.is_some() && finishes > 0 {
                break;
            }
        }
        assert_eq!(starts, 1);
        assert_eq!(finishes, 1);
        let title = title.expect("Oh My Pi should replace the prompt with a generated title");
        assert_ne!(title, prompt);
        assert!(!title.trim().is_empty());
        assert!(!transcript.is_empty());
        assert!(!transcript.contains("Session renamed to"));
        let native = std::fs::read_to_string(native_file.expect("native session file")).unwrap();
        let slot: Value = serde_json::from_str(native.lines().next().unwrap()).unwrap();
        assert_eq!(slot["title"], title);
        eprintln!("OMP generated title: {title}");
        drop(driver);
    }

    #[test]
    #[ignore = "requires an installed, authenticated omp"]
    fn ohmypi_titles_unnamed_history_on_resume_without_a_new_turn() {
        let directory =
            std::env::temp_dir().join(format!("waku-omp-title-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let session_id = uuid::Uuid::new_v4().to_string();
        let session_file = directory.join("history.jsonl");
        let entries = [
            json!({"type":"session", "version":3, "id":session_id,
                "timestamp":"2026-10-31T11:00:00Z", "cwd":directory}),
            json!({"type":"message", "id":"user-1", "parentId":null,
                "timestamp":"2026-10-31T11:00:01Z", "message": {"role":"user",
                "content":[{"type":"text", "text":"如果想一次把 prompt 发给多个模型，可以怎么做？"}], "timestamp":1793444401000_u64}}),
        ];
        std::fs::write(
            &session_file,
            entries
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = PiDriver::start(
            PiFlavor::OhMyPi,
            DriverStartOptions {
                binary: crate::command_env::find_executable("omp").expect("omp is not installed"),
                cwd: directory.clone(),
                mode: RuntimeMode::FullAccess,
                model: std::env::var("WAKU_TEST_OMP_MODEL").ok(),
                reasoning_effort: Some("low".into()),
                service_tier: None,
                context_window: None,
                agent_preset: None,
                execution_policy: Default::default(),
                computer_use_enabled: false,
                provider_cursor: Some(ProviderResumeCursor::OhMyPi {
                    session_id,
                    session_file: Some(session_file.clone()),
                }),
                delegation_session_path: None,
                delegation_registry_dir: None,
            },
            events,
        )
        .expect("the old unnamed conversation should resume");
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut generated = None;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match event_rx.recv_timeout(remaining) {
                Ok(DriverEvent::AutoTitleUpdated(Some(title))) => {
                    generated = Some(title);
                    break;
                }
                Ok(
                    DriverEvent::TurnStarted
                    | DriverEvent::TurnFinished { .. }
                    | DriverEvent::TextDelta(_),
                ) => panic!("repairing a title must not create a conversation turn"),
                Ok(DriverEvent::Error(error)) => panic!("Oh My Pi reported: {error}"),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let title = generated.expect(
            "an old unnamed conversation should receive a title without another user prompt",
        );
        assert!(!title.trim().is_empty());
        eprintln!("OMP restored title: {title}");
        drop(driver);
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(10)) {
            if matches!(event, DriverEvent::ProcessExited) {
                break;
            }
            assert!(!matches!(
                event,
                DriverEvent::TurnStarted
                    | DriverEvent::TurnFinished { .. }
                    | DriverEvent::TextDelta(_)
            ));
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ohmypi_branches_where_pi_forks_and_never_asks_it_to_clone() {
        let messages = [
            json!({"entryId": "turn-1"}),
            json!({"entryId": "turn-2"}),
            json!({"entryId": "turn-3"}),
        ];
        assert_eq!(
            pi_fork_request(PiFlavor::OhMyPi, &messages, 2).unwrap(),
            json!({"type": "branch", "entryId": "turn-2"})
        );
        assert!(pi_fork_request(PiFlavor::OhMyPi, &messages, 4).is_err());
    }

    /// Oh My Pi reuses `agent_end` for intermediate settles, so only the
    /// terminal one may end the turn.
    #[test]
    fn ohmypi_settles_on_the_terminal_agent_end_only() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "text_delta", "delta": "done"}
            }),
            json!({"type": "agent_end", "isTerminal": false}),
            json!({"type": "agent_end", "messages": []}),
        ] {
            handle_pi_message(
                PiFlavor::OhMyPi,
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
            DriverEvent::TextDelta(value) if value == "done"
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    /// Pi's own settle event carries no meaning for Oh My Pi, and vice versa.
    #[test]
    fn each_flavor_ignores_the_other_settle_and_title_events() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for (flavor, value) in [
            (PiFlavor::OhMyPi, json!({"type": "agent_start"})),
            (PiFlavor::OhMyPi, json!({"type": "agent_settled"})),
            (
                PiFlavor::OhMyPi,
                json!({"type": "session_info_changed", "name": "Pi's spelling"}),
            ),
            (PiFlavor::Pi, json!({"type": "agent_end"})),
            (
                PiFlavor::Pi,
                json!({"type": "session_info_update", "title": "Oh My Pi's spelling"}),
            ),
        ] {
            handle_pi_message(flavor, value, &pending, &commands, &events, &mut state);
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn ohmypi_session_titles_arrive_on_its_own_event() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::OhMyPi,
            json!({"type": "session_info_update", "title": "Named by Oh My Pi"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Named by Oh My Pi"
        ));
    }

    #[test]
    fn ohmypi_asks_for_a_title_without_settling_or_polluting_the_turn() {
        let (pending, commands, command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        state.auto_title.observe_state(
            PiFlavor::OhMyPi,
            &json!({"data": {"sessionId": "session-1"}}),
        );
        let auto_title = state.auto_title.clone();
        let mut wire = Vec::new();
        let mut next = 0;
        send_prompt(
            &mut wire,
            &pending,
            &mut next,
            "Improve transcript scrolling",
        )
        .unwrap();
        let mut handle = |value| {
            handle_pi_message(
                PiFlavor::OhMyPi,
                value,
                &pending,
                &commands,
                &events,
                &mut state,
            )
        };
        handle(json!({"type": "agent_start"}));
        handle(json!({"type": "message_start", "message": {"role": "assistant"}}));
        handle(json!({"type": "message_start", "message": {"role": "assistant"}}));
        let CommandMessage::AutoTitle {
            request,
            phase: AutoTitlePhase::CheckName,
        } = command_rx.try_recv().unwrap()
        else {
            panic!("expected a native name check")
        };
        assert!(
            command_rx.try_recv().is_err(),
            "one request per user prompt"
        );
        send_auto_title_request(
            &mut wire,
            &pending,
            &mut next,
            &auto_title,
            request,
            AutoTitlePhase::CheckName,
        )
        .unwrap();
        handle(json!({"type": "response", "id": "waku-2", "success": true,
            "data": {"sessionId": "session-1", "sessionName": ""}}));
        let CommandMessage::AutoTitle {
            request,
            phase: AutoTitlePhase::Generate,
        } = command_rx.try_recv().unwrap()
        else {
            panic!("expected native /rename")
        };
        send_auto_title_request(
            &mut wire,
            &pending,
            &mut next,
            &auto_title,
            request,
            AutoTitlePhase::Generate,
        )
        .unwrap();
        handle(json!({"type": "response", "id": "waku-3", "success": true,
            "data": {"agentInvoked": false}}));
        handle(json!({"type": "message_update", "assistantMessageEvent": {
            "type": "text_delta", "delta": "Use a virtualized list."}}));
        handle(json!({"type": "agent_end", "isTerminal": true}));
        // A background title can land after the normal turn has already ended.
        handle(
            json!({"type": "session_info_update", "sessionId": "old-session", "title": "Stale title"}),
        );
        handle(
            json!({"type": "session_info_update", "sessionId": "session-1", "title": "Improve Transcript Scrolling"}),
        );
        handle(
            json!({"type": "command_output", "text": "Session renamed to Improve Transcript Scrolling."}),
        );
        handle(json!({"type": "message_start", "message": {"role": "assistant"}}));
        assert!(
            command_rx.try_recv().is_err(),
            "named sessions must stay named"
        );
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::TextDelta(text) if text == "Use a virtualized list.")
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::AutoTitleUpdated(Some(title)) if title == "Improve Transcript Scrolling")
        );
        assert!(event_rx.try_recv().is_err());
        assert!(pending.lock().is_empty());
        let wire: Vec<Value> = String::from_utf8(wire)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(wire[1]["type"], "get_state");
        assert_eq!(wire[2]["message"], "/rename");
    }

    #[test]
    fn ohmypi_rechecks_the_native_name_before_generating() {
        let (pending, commands, command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        state.auto_title.observe_state(
            PiFlavor::OhMyPi,
            &json!({"data": {"sessionId": "session-1"}}),
        );
        let request = state.auto_title.start().unwrap();
        send_auto_title_request(
            &mut Vec::new(),
            &pending,
            &mut 0,
            &state.auto_title,
            request,
            AutoTitlePhase::CheckName,
        )
        .unwrap();
        handle_pi_message(
            PiFlavor::OhMyPi,
            json!({"type": "response", "id": "waku-1", "success": true,
                "data": {"sessionId": "session-1", "sessionName": "User's native name"}}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(command_rx.try_recv().is_err());
        assert!(state.auto_title.start().is_none());
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::AutoTitleUpdated(Some(title)) if title == "User's native name")
        );
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn automatic_title_failure_is_retried_only_on_a_later_prompt() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        state.auto_title.observe_state(
            PiFlavor::OhMyPi,
            &json!({"data": {"sessionId": "session-1"}}),
        );
        let old = state.auto_title.start().unwrap();
        send_auto_title_request(
            &mut Vec::new(),
            &pending,
            &mut 0,
            &state.auto_title,
            old.clone(),
            AutoTitlePhase::Generate,
        )
        .unwrap();
        state.run_started = true;
        handle_pi_message(
            PiFlavor::OhMyPi,
            json!({"type": "response", "id": "waku-1", "success": false, "error": "Title model unavailable"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(
            event_rx.try_recv().is_err(),
            "best-effort title failure cannot fail the main turn"
        );
        assert!(state.run_started);
        assert!(state.auto_title.start().is_none());
        state.auto_title.prepare_prompt("Try another task");
        let next = state.auto_title.start().unwrap();
        state.auto_title.finish(&old);
        assert!(!state.auto_title.is_current(&old));
        assert!(
            state.auto_title.is_current(&next),
            "an older response cannot retire a newer attempt"
        );
    }

    #[test]
    fn automatic_titles_preserve_explicit_renames_and_other_command_output() {
        let title = PiAutoTitle::default();
        title.observe_state(
            PiFlavor::OhMyPi,
            &json!({"data": {"sessionId": "session-1"}}),
        );
        let request = title.start().unwrap();
        assert!(!title.consume_output("Context usage: 500 tokens"));
        assert!(title.is_current(&request));
        title.prepare_prompt("/rename My name");
        assert!(!title.is_current(&request));
        assert!(!title.consume_output("Session renamed to My name."));
        assert!(title.start().is_none());

        let title = PiAutoTitle::default();
        title.observe_state(
            PiFlavor::OhMyPi,
            &json!({"data": {"sessionId": "named", "sessionName": "Already named"}}),
        );
        assert!(title.start().is_none());
        let pi = PiAutoTitle::default();
        pi.observe_state(PiFlavor::Pi, &json!({"data": {"sessionId": "pi-session"}}));
        assert!(
            pi.start().is_none(),
            "upstream Pi has its own title behavior"
        );
    }

    #[test]
    fn restored_unnamed_sessions_get_titles_and_stopped_attempts_can_retry() {
        let restored = json!({"data": {"sessionId": "restored", "messageCount": 12}});
        let title = PiAutoTitle::default();
        title.observe_state(PiFlavor::OhMyPi, &restored);
        assert!(
            title
                .start_restored(&json!({"data": {"messageCount": 0}}))
                .is_none()
        );
        let old = title
            .start_restored(&restored)
            .expect("restored history can be named without another turn");
        title.cancel();
        title.prepare_prompt("Continue the task");
        let retry = title.start().expect("Stop must not leave naming in flight");
        title.finish(&old);
        assert!(title.is_current(&retry));

        assert!(
            title.consume_output(
                "Could not generate a session title. Use /rename <title> to set one."
            )
        );
        assert!(!title.is_current(&retry));
        assert!(
            title.start().is_none(),
            "a failed model response must not retry within the same prompt"
        );

        let review = PiAutoTitle {
            disabled: true,
            ..Default::default()
        };
        review.observe_state(PiFlavor::OhMyPi, &restored);
        assert!(
            review.observe_title(Some("restored"), Some("Provider-supplied title")),
            "native title events still pass through when generation is disabled"
        );
        assert!(
            review.start_restored(&restored).is_none(),
            "review subtasks must not pay for hidden title requests"
        );
    }

    #[test]
    fn sanitizes_nerd_font_pua_from_ohmypi_titles() {
        assert_eq!(
            sanitize_ohmypi_title("\u{ea76} TSK: Fix title polling").as_deref(),
            Some("TSK: Fix title polling")
        );
        assert_eq!(
            sanitize_ohmypi_title("🧠 TSK: Fix title polling").as_deref(),
            Some("🧠 TSK: Fix title polling")
        );
        assert_eq!(
            sanitize_ohmypi_title("Fix title polling").as_deref(),
            Some("Fix title polling")
        );
        assert_eq!(sanitize_ohmypi_title("\u{ea76}   "), None);
    }

    #[test]
    fn cancelled_auto_title_output_is_still_consumed_and_does_not_leak() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        state.auto_title.observe_state(
            PiFlavor::OhMyPi,
            &json!({"data": {"sessionId": "session-1"}}),
        );
        let request = state.auto_title.start().unwrap();
        send_auto_title_request(
            &mut Vec::new(),
            &pending,
            &mut 0,
            &state.auto_title,
            request,
            AutoTitlePhase::Generate,
        )
        .unwrap();
        state.auto_title.cancel();
        // A late background rename output after cancellation must be consumed
        // without entering the transcript or wedging TurnStarted.
        handle_pi_message(
            PiFlavor::OhMyPi,
            json!({"type": "command_output", "text": "Session renamed to Cancelled Task."}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(event_rx.try_recv().is_err());
        assert!(!state.run_started);
    }

    #[test]
    fn standalone_command_output_while_idle_settles_immediately() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        // Standalone user-facing command output when no prompt or turn is
        // active must open and immediately settle the turn.
        handle_pi_message(
            PiFlavor::OhMyPi,
            json!({"type": "command_output", "text": "Session renamed to Explicit Name."}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::TextDelta(text) if text == "Session renamed to Explicit Name.\n\n")
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, summary: None }
        ));
        assert!(event_rx.try_recv().is_err());
        assert!(!state.run_started);
    }

    #[test]
    fn observe_title_clears_in_flight_without_waiting_for_command_output() {
        let title = PiAutoTitle::default();
        title.observe_state(
            PiFlavor::OhMyPi,
            &json!({"data": {"sessionId": "session-1"}}),
        );
        let request = title.start().unwrap();
        assert!(title.is_current(&request));
        title.mark_generating();
        // session_info_update arrives first in OMP
        assert!(title.observe_title(Some("session-1"), Some("Generated Title")));
        assert!(!title.is_current(&request));
        // command_output arrives subsequently and is still consumed
        assert!(title.consume_output("Session renamed to Generated Title."));
    }

    #[test]
    fn prepare_prompt_resets_attempted_for_subsequent_prompts() {
        let title = PiAutoTitle::default();
        title.observe_state(
            PiFlavor::OhMyPi,
            &json!({"data": {"sessionId": "session-1"}}),
        );
        let first = title.start().unwrap();
        title.finish(&first);
        // While within the same turn, attempted is true
        assert!(title.start().is_none());
        // A new user prompt arrives, resetting attempted
        title.prepare_prompt("A new user task");
        let second = title.start().expect("new prompt can attempt title generation");
        assert!(title.is_current(&second));
    }

    #[test]
    fn chunked_frames_reassemble_and_reject_broken_runs() {
        use base64::Engine as _;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);

        let payload = json!({"type": "response", "id": "waku-1", "success": true});
        let bytes = serde_json::to_vec(&payload).unwrap();
        let (first, second) = bytes.split_at(bytes.len() / 2);
        let chunk = |index: u64, data: &[u8]| {
            json!({
                "type": "rpc_chunk",
                "chunkId": "rpc-1",
                "index": index,
                "count": 2,
                "byteLength": bytes.len(),
                "data": encode(data),
            })
        };

        let mut assembly = ChunkAssembly::default();
        assert_eq!(assembly.accept(chunk(0, first)).unwrap(), None);
        assert_eq!(assembly.accept(chunk(1, second)).unwrap(), Some(payload));

        // An ordinary frame passes straight through.
        let mut assembly = ChunkAssembly::default();
        let plain = json!({"type": "agent_start"});
        assert_eq!(assembly.accept(plain.clone()).unwrap(), Some(plain.clone()));

        // Anything interleaved into a run invalidates it rather than splicing.
        let mut assembly = ChunkAssembly::default();
        assert_eq!(assembly.accept(chunk(0, first)).unwrap(), None);
        assert!(assembly.accept(plain).is_err());
        assert!(assembly.active.is_none());

        // A run that starts mid-sequence is not a frame Waku can trust.
        let mut assembly = ChunkAssembly::default();
        assert!(assembly.accept(chunk(1, second)).is_err());
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
        for flavor in [PiFlavor::Pi, PiFlavor::OhMyPi] {
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
                PiFlavor::OhMyPi,
                json!({"type": "message_start", "message": {"role": "assistant", "provider": provider, "model": model}}),
                &pending,
                &commands,
                &events,
                &mut state,
            );
            for delta in deltas {
                handle_pi_message(
                    PiFlavor::OhMyPi,
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
                PiFlavor::OhMyPi,
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
                PiFlavor::OhMyPi,
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
                PiFlavor::OhMyPi,
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
        for flavor in [PiFlavor::Pi, PiFlavor::OhMyPi] {
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
        std::process::Command::new("omp")
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
            PiFlavor::OhMyPi,
            Path::new("/nonexistent/omp"),
            Path::new("/tmp"),
            Some(Path::new("/tmp/registration.json")),
            || Ok(PathBuf::from("/bundle/delegation/pi-extension.ts")),
        );

        let args = args(&command);
        let flag = args.iter().position(|arg| arg == "--extension").unwrap();
        assert_eq!(args[flag + 1], "/bundle/delegation/pi-extension.ts");
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
        configure_pi_delegation_command(
            &mut command,
            PiFlavor::Pi,
            Path::new("/nonexistent/pi"),
            Path::new("/tmp"),
            None,
            || panic!("the extension must not be probed without a registration"),
        );
        assert!(args(&command).is_empty());
        assert!(env(&command, "WAKU_AGENT_TOOLS_SERVER").is_none());
    }

    /// The subtask launch must not carry the full-access flag: `--approve` /
    /// `--yolo` would auto-approve the writes the allowlist exists to prevent.
    #[test]
    fn a_review_subtask_is_launched_with_a_read_only_tool_allowlist() {
        for _flavor in [PiFlavor::Pi, PiFlavor::OhMyPi] {
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
    }

    #[test]
    fn a_missing_extension_leaves_the_task_runnable() {
        let mut command = command();
        // A build without the asset still launches the agent; it simply cannot
        // delegate, which is what the eprintln! records.
        configure_pi_delegation_command(
            &mut command,
            PiFlavor::Pi,
            Path::new("/nonexistent/pi"),
            Path::new("/tmp"),
            Some(Path::new("/tmp/registration.json")),
            || anyhow::bail!("asset missing"),
        );
        assert!(args(&command).is_empty());
    }
}

#[cfg(test)]
mod builtin_tool_tests {
    use super::{parse_builtin_tools, parse_unavailable_tools};

    #[test]
    fn the_cli_diagnostic_yields_its_builtin_tool_names() {
        // Shape taken from a real `omp --mode rpc --tools -probe` run.
        let text = "Error: Unknown tool in --tools: -probe.\nBuilt-in tools: read, bash, edit, ast_grep, glob, grep, lsp, task, wait, todo, web_search, write.\nOther registered tools: goal, mcp__server_tool.\nRun `omp --help` for available flags.\n";
        let names = parse_builtin_tools(text).expect("the list is present");
        assert_eq!(names.first().map(String::as_str), Some("read"));
        assert!(names.iter().any(|name| name == "write"));
        assert!(names.iter().any(|name| name == "task"));
        // Only the CLI's own tools: another registered tool is not part of the
        // set a parent session would otherwise lose.
        assert!(!names.iter().any(|name| name == "goal"));
        assert!(!names.iter().any(|name| name.starts_with("mcp__")));
    }

    #[test]
    fn the_other_registered_tools_are_not_taken_for_builtins() {
        // Real shape: both lists share one line, which is why a naive split
        // produced names like "goal" that the CLI then rejected.
        let text = "Error: Unknown tool in --tools: -probe.\nBuilt-in tools: read, bash, write. Other registered tools: goal, mcp__server_tool.\n";
        let names = parse_builtin_tools(text).expect("the list is present");
        assert_eq!(names, vec!["read", "bash", "write"]);
    }

    #[test]
    fn builtins_that_are_unavailable_here_are_dropped() {
        // The CLI answers a second time when a named tool exists but is
        // disabled here; naming it is an error, so it must not reach the
        // allowlist. This text is the real shape, wrapped mid-list.
        let text = "Error: Built-in tools unavailable in this session: ast_grep, ask,\nida, github, find.\nRun `omp --help` for available flags.\n";
        let unavailable = parse_unavailable_tools(text);
        assert_eq!(
            unavailable,
            vec!["ast_grep", "ask", "ida", "github", "find"]
        );
    }

    #[test]
    fn prose_never_becomes_a_tool_name() {
        let text = "Built-in tools: read, bash, write. Other registered tools: goal. Run `omp --help` for available flags.\n";
        let names = parse_builtin_tools(text).expect("the list is present");
        assert_eq!(names, vec!["read", "bash", "write"]);
        assert!(!names.iter().any(|name| name.contains(' ')));
    }

    #[test]
    fn an_unexpected_diagnostic_is_not_guessed_at() {
        assert!(parse_builtin_tools("Error: something else entirely").is_none());
        assert!(parse_builtin_tools("Built-in tools: \n").is_none());
    }
}
