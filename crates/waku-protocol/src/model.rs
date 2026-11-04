//! Provider-neutral projects, sessions, transcript items, and driver events.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    Claude,
    #[default]
    Codex,
    Pi,
}

impl ProviderKind {
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Pi];

    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex CLI",
            Self::Pi => "Pi",
        }
    }

    pub fn short_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Pi => "Pi",
        }
    }

    pub fn command(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "provider"
)]
pub enum ProviderResumeCursor {
    Claude {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_at: Option<String>,
    },
    Codex {
        thread_id: String,
    },
    Pi {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_file: Option<PathBuf>,
    },
}

impl ProviderResumeCursor {
    pub fn from_session_id(provider: ProviderKind, id: String) -> Self {
        match provider {
            ProviderKind::Claude => Self::Claude {
                session_id: id,
                resume_at: None,
            },
            ProviderKind::Codex => Self::Codex { thread_id: id },
            ProviderKind::Pi => Self::Pi {
                session_id: id,
                session_file: None,
            },
        }
    }

    pub fn provider(&self) -> ProviderKind {
        match self {
            Self::Claude { .. } => ProviderKind::Claude,
            Self::Codex { .. } => ProviderKind::Codex,
            Self::Pi { .. } => ProviderKind::Pi,
        }
    }

    pub fn native_id(&self) -> &str {
        match self {
            Self::Claude { session_id, .. } | Self::Pi { session_id, .. } => session_id,
            Self::Codex { thread_id } => thread_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecutionPolicy {
    #[default]
    Interactive,
    ReviewReadOnly,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeMode {
    /// Older state files used `plan` as a combined read-only mode. Keep those
    /// sessions readable without retaining it as a product mode.
    Ask,
    AutoAcceptEdits,
    Auto,
    #[default]
    FullAccess,
}

impl<'de> Deserialize<'de> for RuntimeMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "plan" | "ask" => Ok(Self::Ask),
            "autoAcceptEdits" => Ok(Self::AutoAcceptEdits),
            "auto" => Ok(Self::Auto),
            "fullAccess" => Ok(Self::FullAccess),
            other => Err(<D::Error as serde::de::Error>::unknown_variant(
                other,
                &["ask", "autoAcceptEdits", "auto", "fullAccess"],
            )),
        }
    }
}

impl RuntimeMode {
    pub const ACCESS_OPTIONS: [Self; 4] = [
        Self::Ask,
        Self::AutoAcceptEdits,
        Self::Auto,
        Self::FullAccess,
    ];

    pub fn label(self) -> String {
        match self {
            Self::Ask => tr!("mode.supervised"),
            Self::AutoAcceptEdits => tr!("mode.auto_accept_edits"),
            Self::Auto => tr!("mode.auto"),
            Self::FullAccess => tr!("mode.full_access"),
        }
    }

    pub fn description(self) -> String {
        match self {
            Self::Ask => tr!("mode.supervised_description"),
            Self::AutoAcceptEdits => tr!("mode.auto_accept_edits_description"),
            Self::Auto => tr!("mode.auto_description"),
            Self::FullAccess => tr!("mode.full_access_description"),
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Ask => "icons/lock.svg",
            Self::AutoAcceptEdits => "icons/pencil.svg",
            Self::Auto => "icons/sparkle.svg",
            Self::FullAccess => "icons/lock-open.svg",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderModelOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl ProviderModelOption {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        let description = description.into();
        if !description.trim().is_empty() {
            self.description = Some(description);
        }
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderModel {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_provider: Option<String>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub reasoning_efforts: Vec<ProviderModelOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
    #[serde(default)]
    pub service_tiers: Vec<ProviderModelOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_service_tier: Option<String>,
    /// Context window sizes the provider exposes as a per-session choice.
    /// Claude Code keeps its 1M window opt-in behind a model-id suffix, so the
    /// window is a trait of the session rather than of the model.
    #[serde(default)]
    pub context_windows: Vec<ProviderModelOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_context_window: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FavoriteModel {
    pub provider: ProviderKind,
    pub model: String,
}

/// One provider-owned agent composition available when a task starts.
///
/// A preset chooses the tools and prompt composition for a session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderAgentPreset {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub is_default: bool,
}

impl ProviderAgentPreset {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: None,
            is_default: false,
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        let description = description.into();
        if !description.trim().is_empty() {
            self.description = Some(description);
        }
        self
    }

    pub fn default(mut self) -> Self {
        self.is_default = true;
        self
    }
}

impl ProviderModel {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            sub_provider: None,
            is_default: false,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            service_tiers: Vec::new(),
            default_service_tier: None,
            context_windows: Vec::new(),
            default_context_window: None,
        }
    }

    pub fn default(mut self) -> Self {
        self.is_default = true;
        self
    }

    pub fn sub_provider(mut self, sub_provider: impl Into<String>) -> Self {
        self.sub_provider = Some(sub_provider.into());
        self
    }

    pub fn reasoning(
        mut self,
        efforts: impl IntoIterator<Item = ProviderModelOption>,
        default: impl Into<String>,
    ) -> Self {
        self.reasoning_efforts = efforts.into_iter().collect();
        self.default_reasoning_effort = Some(default.into());
        self
    }

    pub fn service_tiers(
        mut self,
        tiers: impl IntoIterator<Item = ProviderModelOption>,
        default: impl Into<String>,
    ) -> Self {
        self.service_tiers = tiers.into_iter().collect();
        self.default_service_tier = Some(default.into());
        self
    }

    pub fn context_windows(
        mut self,
        windows: impl IntoIterator<Item = ProviderModelOption>,
        default: impl Into<String>,
    ) -> Self {
        self.context_windows = windows.into_iter().collect();
        self.default_context_window = Some(default.into());
        self
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderProbe {
    pub provider: ProviderKind,
    pub installed: bool,
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub models: Vec<ProviderModel>,
    #[serde(default)]
    pub agent_presets: Vec<ProviderAgentPreset>,
}

impl ProviderProbe {
    pub fn preferred_model(&self) -> Option<&ProviderModel> {
        self.models
            .iter()
            .find(|model| model.is_default)
            .or_else(|| self.models.first())
    }

    pub fn model(&self, requested: &str) -> Option<&ProviderModel> {
        self.models.iter().find(|model| model.id == requested)
    }
}

pub fn parse_cli_version(output: &str) -> Option<String> {
    let line = output.lines().find(|line| !line.trim().is_empty())?;
    line.split_whitespace()
        .map(|token| {
            token
                .trim_start_matches('v')
                .trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
        })
        .find(|token| {
            let mut parts = token.split('.');
            let leading_number = parts
                .next()
                .is_some_and(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
            leading_number
                && parts
                    .next()
                    .is_some_and(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))
        })
        .map(str::to_owned)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub path: PathBuf,
    /// When the project was added, unix seconds.
    #[serde(default)]
    pub created_at: u64,
}

/// Filesystem context a task runs in.
///
/// Drafts may carry [`Self::NewWorktree`] until their first prompt. Waku then
/// creates the Git worktree and replaces it with [`Self::Worktree`] before any
/// checkpoint or provider process can observe the task.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum SessionWorkspace {
    /// Work directly in the project's ordinary checkout.
    #[default]
    Local,
    /// Create an isolated worktree when this draft is first submitted. A
    /// selected base branch is remembered without checking it out in the
    /// ordinary project directory.
    NewWorktree {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_branch: Option<String>,
    },
    /// A materialized worktree. `path` preserves a project that points at a
    /// subdirectory of its repository rather than the repository root itself.
    Worktree { path: PathBuf, branch: String },
}

impl SessionWorkspace {
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    pub fn is_worktree(&self) -> bool {
        matches!(self, Self::NewWorktree { .. } | Self::Worktree { .. })
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Worktree { path, .. } => Some(path),
            Self::Local | Self::NewWorktree { .. } => None,
        }
    }
}

impl Project {
    pub const PROJECTLESS_NAME: &'static str = "No project";

    pub fn display_name(&self) -> String {
        if self.is_projectless() {
            tr!("project.no_project_name")
        } else {
            self.name.clone()
        }
    }

    pub fn from_path(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("Project")
            .to_owned();
        Self {
            id: Uuid::new_v4(),
            name,
            path,
            created_at: unix_time(),
        }
    }

    pub fn is_projectless(&self) -> bool {
        crate::projectless::is_projectless_path(&self.path)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionStatus {
    #[default]
    Idle,
    Connecting,
    Working,
    Waiting,
    /// The turn is parked: the provider's reply ended, but detached work it
    /// will wake the session for is still running — Claude Code re-enters the
    /// model with a task notification once a backgrounded command, subagent
    /// or monitor settles. The turn stays open for that wake. Busy, but the
    /// provider is idle, so a new message steers straight in rather than
    /// waiting in the follow-up queue.
    Background,
    Failed,
}

impl SessionStatus {
    pub fn is_busy(self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Working | Self::Waiting | Self::Background
        )
    }
}

/// A follow-up message queued while the agent is busy. It becomes its own
/// turn once the current turn settles successfully.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QueuedMessage {
    pub id: Uuid,
    pub content: String,
    /// The text typed before Waku appended provider-facing attachment
    /// mentions. `None` is the legacy/plain-message representation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<MessageAttachment>,
    /// Delegation targets chosen for this queued prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_references: Vec<crate::delegation::ModelReference>,
    pub created_at: u64,
}

impl QueuedMessage {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            content: content.into(),
            display_content: None,
            attachments: Vec::new(),
            model_references: Vec::new(),
            created_at: unix_time(),
        }
    }

    pub fn with_presentation(
        content: impl Into<String>,
        display_content: Option<String>,
        attachments: Vec<MessageAttachment>,
    ) -> Self {
        Self {
            display_content,
            attachments,
            ..Self::new(content)
        }
    }

    /// Queue a prompt that also names the models it may delegate to.
    pub fn with_presentation_and_targets(
        content: impl Into<String>,
        display_content: Option<String>,
        attachments: Vec<MessageAttachment>,
        model_references: Vec<crate::delegation::ModelReference>,
    ) -> Self {
        Self {
            model_references,
            ..Self::with_presentation(content, display_content, attachments)
        }
    }

    pub fn visible_content(&self) -> &str {
        self.display_content.as_deref().unwrap_or(&self.content)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CheckpointStatus {
    Ready,
    Unavailable,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointFile {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Checkpoint {
    pub turn_count: usize,
    pub git_ref: String,
    pub status: CheckpointStatus,
    #[serde(default)]
    pub files: Vec<CheckpointFile>,
    /// Cached once at capture time so a visible transcript row never walks a
    /// potentially huge file list on every frame.
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    pub created_at: u64,
}

impl Checkpoint {
    pub fn refresh_totals(&mut self) {
        self.additions = self.files.iter().map(|file| file.additions).sum();
        self.deletions = self.files.iter().map(|file| file.deletions).sum();
    }

    pub fn totals_are_current(&self) -> bool {
        self.additions == self.files.iter().map(|file| file.additions).sum::<u64>()
            && self.deletions == self.files.iter().map(|file| file.deletions).sum::<u64>()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentTurn {
    pub id: Uuid,
    pub turn_count: usize,
    pub status: TurnStatus,
    #[serde(default)]
    pub provider_turn_started: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_resume_at: Option<String>,
    /// Model and reasoning effort captured for this turn. Missing on older
    /// transcripts; never reconstruct them from the session's current options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    #[serde(default)]
    pub checkpoint: Option<Checkpoint>,
}

/// How full the provider's context window is, from the latest main-thread
/// model call. `tokens` is prompt + cache + output of that call; `window` is
/// the model's context size, which the provider only reports once a turn
/// settles — `None` means "not known yet", and the meter degrades to a bare
/// token count.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ContextUsage {
    pub tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u64>,
}

/// Where Codex stands on a thread goal. Mirrors the app-server's
/// `ThreadGoalStatus` vocabulary; the serialized names are Codex's own so a
/// status can round-trip through `thread/goal/set` unchanged.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadGoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

impl ThreadGoalStatus {
    /// Whether the goal is still being pursued or can be resumed. Complete
    /// and budget-limited goals are terminal: replacing them starts fresh
    /// accounting instead of continuing the old ledger.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::BudgetLimited)
    }
}

/// A resumable conversation discovered in a provider CLI's own history.
///
/// This is deliberately lightweight: the command palette can list hundreds of
/// native sessions without moving their transcripts over the daemon protocol.
/// [`ProviderSessionHistory`] is fetched only after the user chooses one.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderSessionSummary {
    pub cursor: ProviderResumeCursor,
    pub title: String,
    pub cwd: PathBuf,
    pub created_at: u64,
    pub updated_at: u64,
}

impl ProviderSessionSummary {
    pub fn provider(&self) -> ProviderKind {
        self.cursor.provider()
    }
}

/// The displayable portion of a provider-native conversation imported into a
/// Waku task. Provider history remains authoritative; unsupported native
/// items such as private reasoning or provider-only control records are
/// intentionally absent.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProviderSessionHistory {
    #[serde(default)]
    pub transcript_blocks: Vec<TranscriptBlock>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub turns: Vec<AgentTurn>,
}

/// A provider-persisted objective the agent keeps pursuing across turns.
/// Field names follow the Codex app-server payload so its `goal` objects
/// deserialize directly.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadGoal {
    pub objective: String,
    pub status: ThreadGoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<i64>,
    #[serde(default)]
    pub tokens_used: i64,
    #[serde(default)]
    pub time_used_seconds: i64,
}

/// A goal mutation the client asks the provider runtime to perform. Results
/// come back asynchronously as [`DriverEvent::GoalUpdated`]; failures surface
/// through [`DriverEvent::Error`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum GoalOperation {
    /// Re-read the provider's current goal without changing it.
    Refresh,
    /// Create or update the goal. `None` fields keep their provider-side
    /// value; `replace` clears the existing goal first so a new objective
    /// starts with fresh token and time accounting.
    Set {
        objective: Option<String>,
        status: Option<ThreadGoalStatus>,
        replace: bool,
    },
    Clear,
}

/// Last daemon event incorporated into a session's persisted projection.
///
/// The daemon runtime and its replay journal can outlive any particular
/// desktop or browser connection. Persisting this cursor with the transcript
/// lets a newly attached client replay only the events the stored projection
/// has not already applied.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeEventCursor {
    pub runtime_id: Uuid,
    pub epoch: Uuid,
    pub sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentSession {
    pub id: Uuid,
    /// A title explicitly chosen by the user. [`Self::DEFAULT_TITLE`] means
    /// no explicit title has been set, so [`Self::auto_title`] may be shown.
    pub title: String,
    /// Best-effort title supplied by the provider, or derived locally from the
    /// first prompt until the provider reports a better one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_title: Option<String>,
    pub project_id: Uuid,
    /// Local project checkout or an isolated Git worktree for this task.
    #[serde(default, skip_serializing_if = "SessionWorkspace::is_local")]
    pub workspace: SessionWorkspace,
    pub provider: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub runtime_mode: RuntimeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Selected context window, when the provider exposes more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<String>,
    /// Provider-owned agent composition selected for the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_preset: Option<String>,
    pub status: SessionStatus,
    pub created_at: u64,
    /// Any mutation, including title edits and truncation. Use
    /// [`Self::last_reply_at`] for conversation recency.
    pub updated_at: u64,
    /// Activity time of the newest turn. Set as soon as the user submits it,
    /// then refreshed when the turn settles, whatever its outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reply_at: Option<u64>,
    #[serde(default)]
    pub provider_cursor: Option<ProviderResumeCursor>,
    /// Slash commands the provider reported for this session's live process,
    /// kept so a resumed session still completes them before its next
    /// handshake.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_commands: Vec<ReportedCommand>,
    /// The provider-persisted goal for this session's thread, kept so a
    /// resumed session shows its goal before the runtime reconnects.
    /// Currently populated by Codex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_goal: Option<ThreadGoal>,
    /// Context-window occupancy from the live stream, kept so a resumed
    /// session's meter starts where the conversation left off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_compaction: Option<String>,
    #[serde(default)]
    pub context_usage_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_event_cursor: Option<RuntimeEventCursor>,
    /// Read-only compatibility field for v1 state files. New saves omit it.
    #[serde(default, skip_serializing)]
    pub provider_session_id: Option<String>,
    /// Not stored in the session JSON — these are rows in the `messages`
    /// table, reattached when the session is hydrated.
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub transcript_blocks: Vec<TranscriptBlock>,
    #[serde(default)]
    pub turns: Vec<AgentTurn>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_messages: Vec<QueuedMessage>,
    /// Whether the transcript has been read from the database.
    ///
    /// Startup loads only the columns the session list needs, so a session
    /// begins as a skeleton with empty `messages`, `transcript_blocks` and
    /// `turns`. Those are empty because nothing fetched them, not because the
    /// session is empty — never persist a skeleton, and never conclude from one
    /// that a session has no history.
    ///
    /// Serialized only when `false`, so a skeleton a client saves is still a
    /// skeleton when the daemon reads it. Skipping it outright made every
    /// saved skeleton arrive as a loaded session with no turns — which the
    /// store treats as never started and drops: renaming an unopened task
    /// deleted it.
    #[serde(
        default = "detail_loaded_default",
        skip_serializing_if = "is_detail_loaded"
    )]
    pub detail_loaded: bool,
}

/// Anything deserialized from a `data` blob carries its full detail.
fn detail_loaded_default() -> bool {
    true
}

fn is_detail_loaded(detail_loaded: &bool) -> bool {
    *detail_loaded
}

impl AgentSession {
    pub const DEFAULT_TITLE: &'static str = "New task";

    pub fn new(project_id: Uuid, provider: ProviderKind) -> Self {
        let now = unix_time();
        Self {
            id: Uuid::new_v4(),
            title: Self::DEFAULT_TITLE.to_owned(),
            auto_title: None,
            project_id,
            workspace: SessionWorkspace::Local,
            provider,
            model: None,
            runtime_mode: RuntimeMode::FullAccess,
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
            agent_preset: None,
            status: SessionStatus::Idle,
            created_at: now,
            updated_at: now,
            last_reply_at: None,
            detail_loaded: true,
            provider_cursor: None,
            available_commands: Vec::new(),
            thread_goal: None,
            context_usage: None,
            active_compaction: None,
            context_usage_pending: false,
            runtime_event_cursor: None,
            provider_session_id: None,
            messages: Vec::new(),
            transcript_blocks: Vec::new(),
            turns: Vec::new(),
            queued_messages: Vec::new(),
        }
    }

    /// Returns the lightweight projection used by task lists.
    ///
    /// A daemon can hold hydrated sessions in memory, but catalog refreshes
    /// must never clone or transmit their transcripts. Clients hydrate one
    /// selected session explicitly when they need its detail.
    pub fn list_projection(&self) -> Self {
        Self {
            id: self.id,
            title: self.title.clone(),
            auto_title: self.auto_title.clone(),
            project_id: self.project_id,
            workspace: SessionWorkspace::Local,
            provider: self.provider,
            model: self.model.clone(),
            runtime_mode: RuntimeMode::default(),
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
            agent_preset: None,
            status: self.status,
            created_at: self.created_at,
            updated_at: self.updated_at,
            last_reply_at: self.last_reply_at,
            provider_cursor: None,
            available_commands: Vec::new(),
            thread_goal: None,
            context_usage: None,
            active_compaction: self.active_compaction.clone(),
            context_usage_pending: false,
            runtime_event_cursor: None,
            provider_session_id: None,
            messages: Vec::new(),
            transcript_blocks: Vec::new(),
            turns: Vec::new(),
            queued_messages: Vec::new(),
            detail_loaded: false,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.status.is_busy() || self.active_compaction.is_some()
    }

    /// Forced cleanup must terminalize provider metadata as well as the row.
    pub fn interrupt_compactions(&mut self, now: u64) -> bool {
        let mut changed = self.active_compaction.take().is_some();
        for activity in self
            .transcript_blocks
            .iter_mut()
            .flat_map(|block| &mut block.activities)
        {
            if let Some(item) = &mut activity.compaction
                && item.status == crate::compaction::CompactionStatus::Running
            {
                item.finish(crate::compaction::CompactionStatus::Interrupted, now);
                activity.title = tr!("compaction.interrupted");
                activity.complete = true;
                changed = true;
            }
        }
        changed
    }

    /// Derives [`Self::last_reply_at`] from the turn history when it is not
    /// already known, so a session stored before the field existed still sorts
    /// and displays correctly.
    pub fn backfill_last_reply_at(&mut self) {
        if self.last_reply_at.is_some() {
            return;
        }
        self.last_reply_at = self
            .turns
            .last()
            .map(|turn| turn.completed_at.unwrap_or(turn.started_at))
            .filter(|_| self.has_started());
    }

    pub fn has_started(&self) -> bool {
        // A skeleton came from a stored row, and only started sessions are
        // stored, so it has started even though its transcript is not loaded.
        !self.detail_loaded
            || !self.turns.is_empty()
            || !self.messages.is_empty()
            || self.provider_cursor.is_some()
    }

    /// Drops the loaded transcript so the session returns to its skeleton
    /// state, releasing the heap its messages, blocks and turns occupied.
    ///
    /// A session must be fully persisted and unmodified before this runs —
    /// callers check the store's dirty set — because the released fields are
    /// gone until the next store `hydrate` reloads them. The session keeps
    /// its list columns and cursors, and a later save of the skeleton only
    /// touches those columns, never the untouched detail row.
    pub fn release_transcript(&mut self) {
        self.messages = Vec::new();
        self.transcript_blocks = Vec::new();
        self.turns = Vec::new();
        self.queued_messages = Vec::new();
        self.detail_loaded = false;
    }

    /// Identifier owned by the underlying agent CLI, once its native session
    /// has been established.
    pub fn provider_native_id(&self) -> Option<&str> {
        self.provider_cursor
            .as_ref()
            .map(ProviderResumeCursor::native_id)
            .filter(|id| !id.trim().is_empty())
    }

    pub fn display_title(&self) -> &str {
        if self.title != Self::DEFAULT_TITLE && !self.title.trim().is_empty() {
            &self.title
        } else {
            self.auto_title
                .as_deref()
                .map(crate::title::without_leading_icons)
                .filter(|title| !title.is_empty())
                .unwrap_or(Self::DEFAULT_TITLE)
        }
    }

    /// Sets the user-owned title shown ahead of any provider fallback.
    /// Empty names are rejected so a cancelled inline rename cannot hide the
    /// existing title. Returns whether the stored title changed.
    pub fn set_title(&mut self, title: impl AsRef<str>) -> bool {
        let title = title.as_ref().trim();
        if title.is_empty() || self.title == title {
            return false;
        }
        self.title = title.to_owned();
        self.updated_at = unix_time();
        true
    }

    pub fn set_title_from_prompt(&mut self, prompt: &str) {
        if self.messages.len() > 1 || self.title != Self::DEFAULT_TITLE || self.auto_title.is_some()
        {
            return;
        }
        let mut title = prompt
            .split_whitespace()
            .take(7)
            .collect::<Vec<_>>()
            .join(" ");
        if !title.is_empty() {
            if title.chars().count() > 54 {
                title = format!("{}…", title.chars().take(53).collect::<String>());
            }
            self.auto_title = Some(title);
        }
    }

    /// Replaces the provider-owned title without disturbing an explicit user
    /// title. Returns whether the stored fallback changed.
    pub fn set_auto_title(&mut self, title: Option<String>) -> bool {
        let title = title.and_then(|title| {
            let title = crate::title::without_leading_icons(&title);
            (!title.is_empty()).then(|| title.to_owned())
        });
        if self.auto_title == title {
            return false;
        }
        self.auto_title = title;
        self.updated_at = unix_time();
        true
    }

    pub fn can_choose_model(&self, provider: ProviderKind) -> bool {
        !self.is_busy() && (self.messages.is_empty() || self.provider == provider)
    }

    pub fn migrate_legacy_state(&mut self) {
        if self.provider_cursor.is_none()
            && let Some(id) = self.provider_session_id.take()
        {
            self.provider_cursor = Some(ProviderResumeCursor::from_session_id(self.provider, id));
        }
        if self.provider == ProviderKind::Codex {
            for message in &mut self.messages {
                if message.role == MessageRole::Assistant && message.content.contains('\u{e200}') {
                    message.content = strip_legacy_codex_citations(&message.content);
                }
            }
        }
        let mut merged_blocks: Vec<TranscriptBlock> =
            Vec::with_capacity(self.transcript_blocks.len());
        for mut block in std::mem::take(&mut self.transcript_blocks) {
            if let Some(previous) = merged_blocks.last_mut()
                && previous.after_message == block.after_message
                && previous.turn_id == block.turn_id
                && !previous
                    .activities
                    .iter()
                    .chain(&block.activities)
                    .any(ActivityItem::is_standalone)
            {
                previous.activities.append(&mut block.activities);
            } else {
                merged_blocks.push(block);
            }
        }
        self.transcript_blocks = merged_blocks;

        for block in &mut self.transcript_blocks {
            for activity in &mut block.activities {
                if activity.kind == ActivityKind::Search && activity.title.trim() == "Search for" {
                    activity.title = tr!("activity.browsed_web");
                }
                let named_kind = ActivityKind::from_tool_name(&activity.title);
                if named_kind != ActivityKind::Tool
                    && matches!(
                        activity.kind,
                        ActivityKind::Search | ActivityKind::Tool | ActivityKind::FileChange
                    )
                {
                    activity.kind = named_kind;
                }
                if activity.arguments.is_none()
                    && activity.output.is_none()
                    && !activity.failed
                    && activity.detail.as_deref().is_some_and(|detail| {
                        serde_json::from_str::<serde_json::Value>(detail).is_ok()
                    })
                {
                    // Older provider transcripts stored input JSON in
                    // `detail`. Promote it once so it stays expandable but no
                    // longer floods the row preview.
                    activity.arguments = activity.detail.take();
                }
                activity.refresh_activity_metadata();
            }
        }

        // Checkpoints written before cached totals were added still have the
        // complete file list. Backfill once on load rather than making every
        // transcript frame rediscover the same totals.
        for turn in &mut self.turns {
            if let Some(checkpoint) = turn.checkpoint.as_mut() {
                checkpoint.refresh_totals();
            }
        }

        if !self.turns.is_empty()
            || !self
                .messages
                .iter()
                .any(|message| message.role == MessageRole::User)
        {
            return;
        }

        let user_indexes = self
            .messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| (message.role == MessageRole::User).then_some(index))
            .collect::<Vec<_>>();
        for (offset, start) in user_indexes.iter().copied().enumerate() {
            let end = user_indexes
                .get(offset + 1)
                .copied()
                .unwrap_or(self.messages.len());
            let id = Uuid::new_v4();
            let started_at = self.messages[start].created_at;
            let completed_at = self.messages[start..end]
                .iter()
                .map(|message| message.created_at)
                .max()
                .unwrap_or(started_at);
            for message in &mut self.messages[start..end] {
                message.turn_id = Some(id);
            }
            for block in &mut self.transcript_blocks {
                if block.after_message > start && block.after_message <= end {
                    block.turn_id = Some(id);
                }
            }
            self.turns.push(AgentTurn {
                id,
                turn_count: offset + 1,
                status: TurnStatus::Completed,
                provider_turn_started: true,
                provider_resume_at: None,
                model: None,
                reasoning_effort: None,
                started_at,
                completed_at: Some(completed_at),
                checkpoint: None,
            });
        }
    }

    #[doc(hidden)]
    pub fn begin_turn(&mut self, prompt: impl Into<String>) -> Uuid {
        self.begin_turn_with_presentation(prompt, None, Vec::new())
    }

    pub fn begin_turn_with_presentation(
        &mut self,
        prompt: impl Into<String>,
        display_content: Option<String>,
        attachments: Vec<MessageAttachment>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let now = unix_time();
        self.turns.push(AgentTurn {
            id,
            turn_count: self.turns.len() + 1,
            status: TurnStatus::Running,
            provider_turn_started: false,
            provider_resume_at: None,
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            started_at: now,
            completed_at: None,
            checkpoint: None,
        });
        self.messages.push(
            Message::new_for_turn(MessageRole::User, prompt, id)
                .with_presentation(display_content, attachments),
        );
        self.last_reply_at = Some(now);
        id
    }

    /// Begin a turn the provider runs on its own — Codex goal continuation
    /// pursues an active goal whenever its thread is idle. There is no user
    /// message; the turn exists so the streamed output has a transcript home.
    /// Like a submission's turn it starts unconfirmed: the provider's own
    /// start report marks it via [`Self::mark_active_turn_provider_started`],
    /// and an unconfirmed one can be unwound if the pursuit never begins.
    pub fn begin_provider_turn(&mut self) -> Uuid {
        let id = Uuid::new_v4();
        let now = unix_time();
        self.turns.push(AgentTurn {
            id,
            turn_count: self.turns.len() + 1,
            status: TurnStatus::Running,
            provider_turn_started: false,
            provider_resume_at: None,
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            started_at: now,
            completed_at: None,
            checkpoint: None,
        });
        self.last_reply_at = Some(now);
        self.updated_at = now;
        id
    }

    /// Whether the running turn is a provider-initiated one whose pursuit has
    /// not been confirmed — no user message belongs to it and the provider
    /// has not reported its start. These are the optimistic turns a `/goal`
    /// begins, and the only turns safe to unwind on failure.
    pub fn active_turn_is_unconfirmed_pursuit(&self) -> bool {
        let Some(turn) = self
            .turns
            .last()
            .filter(|turn| turn.status == TurnStatus::Running && !turn.provider_turn_started)
        else {
            return false;
        };
        !self
            .messages
            .iter()
            .any(|message| message.turn_id == Some(turn.id))
    }

    pub fn active_turn_id(&self) -> Option<Uuid> {
        self.turns
            .last()
            .filter(|turn| turn.status == TurnStatus::Running)
            .map(|turn| turn.id)
    }

    /// Undo [`Self::begin_turn`] for a turn whose provider never started —
    /// the submission-preparation failure path, where the prompt returns to
    /// the composer. The turn and its messages leave the transcript, and a
    /// first-prompt unwind also gives back the default title that
    /// [`Self::set_title_from_prompt`] replaced. Its submission timestamp stays
    /// as the session's latest activity.
    pub fn unwind_unstarted_turn(&mut self, turn_id: Uuid) {
        let unstarted = self.turns.last().is_some_and(|turn| {
            turn.id == turn_id && turn.status == TurnStatus::Running && !turn.provider_turn_started
        });
        if !unstarted {
            return;
        }
        self.turns.pop();
        self.messages
            .retain(|message| message.turn_id != Some(turn_id));
        if self.messages.is_empty() {
            self.auto_title = None;
        }
    }

    pub fn mark_active_turn_provider_started(&mut self) {
        if let Some(turn) = self
            .turns
            .last_mut()
            .filter(|turn| turn.status == TurnStatus::Running)
        {
            turn.provider_turn_started = true;
        }
    }

    pub fn mark_active_turn_provider_resume_at(&mut self, message_id: String) {
        if let Some(turn) = self
            .turns
            .last_mut()
            .filter(|turn| turn.status == TurnStatus::Running)
        {
            turn.provider_resume_at = Some(message_id);
        }
    }

    pub fn provider_turns_after(&self, turn_count: usize) -> usize {
        self.turns
            .iter()
            .skip(turn_count)
            .filter(|turn| turn.provider_turn_started)
            .count()
    }

    pub fn finish_active_turn(&mut self, status: TurnStatus) -> Option<(Uuid, usize)> {
        let turn = self
            .turns
            .last_mut()
            .filter(|turn| turn.status == TurnStatus::Running)?;
        let completed_at = unix_time();
        turn.status = status;
        turn.completed_at = Some(completed_at);
        let result = (turn.id, turn.turn_count);
        self.last_reply_at = Some(completed_at);
        Some(result)
    }

    pub fn push_message(&mut self, role: MessageRole, content: impl Into<String>) -> Uuid {
        let message = match self.active_turn_id() {
            Some(turn_id) => Message::new_for_turn(role, content, turn_id),
            None => Message::new(role, content),
        };
        let id = message.id;
        self.messages.push(message);
        id
    }

    pub fn push_user_message_with_presentation(
        &mut self,
        content: impl Into<String>,
        display_content: Option<String>,
        attachments: Vec<MessageAttachment>,
    ) -> Uuid {
        let message = match self.active_turn_id() {
            Some(turn_id) => Message::new_for_turn(MessageRole::User, content, turn_id),
            None => Message::new(MessageRole::User, content),
        }
        .with_presentation(display_content, attachments);
        let id = message.id;
        self.messages.push(message);
        id
    }

    pub fn truncate_after_turn(&mut self, turn_count: usize) {
        let retained = self
            .turns
            .iter()
            .take(turn_count)
            .map(|turn| turn.id)
            .collect::<std::collections::HashSet<_>>();
        self.turns.truncate(turn_count);
        self.messages.retain(|message| {
            message
                .turn_id
                .is_none_or(|turn_id| retained.contains(&turn_id))
        });
        self.transcript_blocks.retain(|block| {
            block
                .turn_id
                .is_none_or(|turn_id| retained.contains(&turn_id))
        });
        let message_count = self.messages.len();
        for block in &mut self.transcript_blocks {
            block.after_message = block.after_message.min(message_count);
        }
        self.updated_at = unix_time();
    }

    pub fn fork_through_turn(
        &self,
        turn_count: usize,
        provider_cursor: ProviderResumeCursor,
        fork_title: &str,
    ) -> Option<Self> {
        if turn_count == 0 || turn_count > self.turns.len() {
            return None;
        }

        let mut fork = self.clone();
        fork.truncate_after_turn(turn_count);
        let fork_id = Uuid::new_v4();
        let turn_ids = fork
            .turns
            .iter()
            .map(|turn| (turn.id, Uuid::new_v4()))
            .collect::<std::collections::HashMap<_, _>>();

        for turn in &mut fork.turns {
            turn.id = turn_ids[&turn.id];
        }
        for message in &mut fork.messages {
            message.id = Uuid::new_v4();
            if let Some(turn_id) = message.turn_id {
                message.turn_id = turn_ids.get(&turn_id).copied();
            }
            message.streaming = false;
        }
        for block in &mut fork.transcript_blocks {
            if let Some(turn_id) = block.turn_id {
                block.turn_id = turn_ids.get(&turn_id).copied();
            }
        }

        let now = unix_time();
        fork.id = fork_id;
        fork.title = Self::DEFAULT_TITLE.to_owned();
        fork.auto_title = Some(fork_title.to_owned());
        fork.status = SessionStatus::Idle;
        fork.created_at = now;
        fork.updated_at = now;
        fork.provider_cursor = Some(provider_cursor);
        fork.provider_session_id = None;
        // A fork snapshots the conversation, not the pending follow-ups its
        // source session is still holding for the live agent.
        fork.queued_messages.clear();
        Some(fork)
    }
}

fn strip_legacy_codex_citations(text: &str) -> String {
    const START: char = '\u{e200}';
    const END: char = '\u{e201}';
    const SEPARATOR: char = '\u{e202}';

    let mut remaining = text;
    let mut output = String::with_capacity(text.len());
    while let Some(start) = remaining.find(START) {
        output.push_str(&remaining[..start]);
        let marker_start = start + START.len_utf8();
        let Some(end_offset) = remaining[marker_start..].find(END) else {
            output.push_str(&remaining[start..]);
            return output;
        };
        let marker_end = marker_start + end_offset;
        let marker = &remaining[marker_start..marker_end];
        if marker
            .split(SEPARATOR)
            .next()
            .is_some_and(|prefix| prefix != "cite")
        {
            output.push_str(&remaining[start..marker_end + END.len_utf8()]);
        }
        remaining = &remaining[marker_end + END.len_utf8()..];
    }
    output.push_str(remaining);
    output
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

/// A file represented by a composer chip and retained with the sent message.
///
/// Render paths consume only this cached metadata; they never stat the file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MessageAttachment {
    /// Absolute path on the daemon host, handed to the provider. Clients must
    /// use `blob_reference` rather than opening this path themselves.
    pub path: PathBuf,
    /// Provider-facing path text, relative to the workspace when possible.
    pub mention: String,
    pub name: String,
    pub is_dir: bool,
    pub is_image: bool,
    /// Durable daemon-issued blob or attachment reference. The legacy field
    /// name is retained for storage compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_reference: Option<String>,
    /// Ephemeral or persisted content of pasted text converted to a snippet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pasted_text: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Message {
    pub id: Uuid,
    #[serde(default)]
    pub turn_id: Option<Uuid>,
    pub role: MessageRole,
    pub content: String,
    /// User-visible text before provider-facing attachment mentions were
    /// appended. Plain and legacy messages omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<MessageAttachment>,
    pub created_at: u64,
    pub streaming: bool,
}

impl Message {
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            turn_id: None,
            role,
            content: content.into(),
            display_content: None,
            attachments: Vec::new(),
            created_at: unix_time(),
            streaming: false,
        }
    }

    pub fn new_for_turn(role: MessageRole, content: impl Into<String>, turn_id: Uuid) -> Self {
        Self {
            turn_id: Some(turn_id),
            ..Self::new(role, content)
        }
    }

    pub fn with_presentation(
        mut self,
        display_content: Option<String>,
        attachments: Vec<MessageAttachment>,
    ) -> Self {
        self.display_content = display_content;
        self.attachments = attachments;
        self
    }

    pub fn visible_content(&self) -> &str {
        self.display_content.as_deref().unwrap_or(&self.content)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActivityKind {
    Reasoning,
    Command,
    FileChange,
    FileRead,
    FileSearch,
    FileList,
    Search,
    Plan,
    Tool,
    Compaction,
    ImageGeneration,
}

impl ActivityKind {
    /// Classifies provider tool names without mistaking unrelated MCP tools
    /// such as `create_thread` or `read_mcp_resource` for file operations.
    pub fn from_tool_name(name: &str) -> Self {
        let normalized = name.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        let leaf = normalized
            .rsplit("__")
            .next()
            .unwrap_or(&normalized)
            .rsplit([':', '.', '/'])
            .next()
            .unwrap_or(&normalized);
        let compact = leaf.replace('_', "");

        if matches!(
            compact.as_str(),
            "imagegen" | "imagegeneration" | "generateimage" | "generateimages"
        ) {
            Self::ImageGeneration
        } else if matches!(
            compact.as_str(),
            "todo" | "todowrite" | "updateplan" | "plan" | "managetask"
        ) {
            Self::Plan
        } else if matches!(
            compact.as_str(),
            "bash"
                | "command"
                | "commandstatus"
                | "execute"
                | "executecommand"
                | "commandexecution"
                | "notebookexecution"
                | "runcommand"
                | "runterminalcommand"
                | "sendcommandinput"
                | "shell"
                | "shellcommand"
                | "terminal"
        ) {
            Self::Command
        } else if matches!(
            compact.as_str(),
            "applypatch"
                | "create"
                | "createfile"
                | "delete"
                | "deletefile"
                | "edit"
                | "filechange"
                | "fileedit"
                | "editfile"
                | "move"
                | "movefile"
                | "multiedit"
                | "multireplacefilecontent"
                | "notebookedit"
                | "patch"
                | "rename"
                | "renamefile"
                | "replace"
                | "replacefilecontent"
                | "savefile"
                | "sedfile"
                | "strreplace"
                | "write"
                | "writefile"
                | "writetofile"
        ) {
            Self::FileChange
        } else if matches!(
            compact.as_str(),
            "read" | "fileread" | "readfile" | "readtextfile" | "viewfile"
        ) {
            Self::FileRead
        } else if matches!(
            compact.as_str(),
            "filesearch"
                | "find"
                | "findbyname"
                | "findfiles"
                | "glob"
                | "grep"
                | "grepsearch"
                | "ripgrep"
                | "searchfiles"
                | "searchinfiles"
        ) {
            Self::FileSearch
        } else if matches!(
            compact.as_str(),
            "directorylist"
                | "filelist"
                | "list"
                | "listdir"
                | "listdirectory"
                | "listfiles"
                | "ls"
                | "readdir"
        ) {
            Self::FileList
        } else if matches!(
            compact.as_str(),
            "readurlcontent" | "search" | "searchtool" | "searchweb" | "webfetch" | "websearch"
        ) {
            Self::Search
        } else {
            Self::Tool
        }
    }
}

#[derive(Clone, Debug)]
pub enum DriverEvent {
    CompactionUpdated(crate::compaction::ContextCompaction),
    /// Client-only acknowledgement that every daemon event through this
    /// sequence has been incorporated into the local session projection.
    /// Providers never emit this and the daemon never serializes it.
    RuntimeEventCursorAdvanced(RuntimeEventCursor),
    Connected {
        provider_cursor: Option<ProviderResumeCursor>,
    },
    AgentPresetSelected(Option<String>),
    /// A provider-owned, automatically generated session title. `None`
    /// clears that fallback but never overwrites a user-owned title.
    AutoTitleUpdated(Option<String>),
    /// The slash commands the live process itself reports — Claude's
    /// stream-json init handshake and ACP's `available_commands_update`.
    /// Authoritative over filesystem discovery, which cannot see plugin or
    /// dynamically registered commands.
    AvailableCommands(Vec<ReportedCommand>),
    TurnStarted,
    /// The provider's turn ended while detached work it will wake the
    /// session for is still running. The turn stays open — the wake's
    /// `TurnStarted` continues it — and the session shows it is waiting.
    TurnParked,
    TextDelta(String),
    /// Legacy streams without provider-owned reasoning block identities.
    ReasoningDelta(String),
    ReasoningStarted {
        id: String,
        timestamp_ms: u64,
    },
    ReasoningBlockDelta {
        id: String,
        delta: String,
    },
    /// Final content is authoritative when the provider supplies it. Receipt
    /// timestamps travel with the lifecycle so replay never times the UI pump.
    ReasoningCompleted {
        id: String,
        timestamp_ms: u64,
        content: Option<String>,
    },
    Activity {
        id: Option<String>,
        kind: ActivityKind,
        title: String,
        detail: Option<String>,
        complete: bool,
    },
    RichActivity(ActivityItem),
    /// Session-level work that can outlive the turn which created it. This is
    /// deliberately separate from transcript activities: completing a turn
    /// must not make a detached process or subagent look complete.
    BackgroundWork(BackgroundWorkEvent),
    Permission {
        request_id: String,
        title: String,
        detail: String,
        options: Vec<PermissionOption>,
    },
    /// Structured questions for the user. Unlike a permission, this is never
    /// auto-approved: the content itself has to come from the user.
    /// `delivery` says whether the provider is blocked on a reply or has
    /// already moved on and expects the answer as the next user message.
    UserInputRequested {
        request_id: String,
        questions: Vec<UserInputQuestion>,
        delivery: UserInputDelivery,
    },
    /// The provider accepted a steering message into the running turn.
    SteerAccepted {
        message: String,
    },
    /// The provider could not steer the running turn (for example it ended
    /// before the request arrived). The app decides the fallback.
    SteerRejected {
        message: String,
        reason: String,
    },
    /// Context-window occupancy reported by the live stream. Fields arrive at
    /// different moments — token counts with each assistant message, the
    /// window size with the settled turn — so each is optional and the app
    /// merges them into [`ContextUsage`].
    UsageUpdated {
        context_tokens: Option<u64>,
        context_window: Option<u64>,
    },
    /// Account-level rate-limit meters carried by the provider's own stream
    /// (Codex's `account/rateLimits/updated`). Same shape the OAuth fetcher
    /// produces for Claude, so the panel renders both identically.
    PlanUsageUpdated(crate::usage::PlanUsage),
    /// The provider-persisted thread goal changed — set, edited, progressed,
    /// or (`None`) cleared. Carries the whole goal so late subscribers need
    /// no earlier event.
    GoalUpdated(Option<ThreadGoal>),
    TurnFinished {
        success: bool,
        summary: Option<String>,
    },
    Error(String),
    ProcessExited,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundWorkKind {
    Process,
    Monitor,
    Subagent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundWorkStatus {
    Starting,
    Running,
    Monitoring,
    Stopping,
    Completed,
    Failed,
    Stopped,
    Lost,
}

impl BackgroundWorkStatus {
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Running | Self::Monitoring | Self::Stopping
        )
    }

    pub fn is_stoppable(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Monitoring)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundWorkKey {
    pub kind: BackgroundWorkKind,
    pub provider_id: String,
}

impl BackgroundWorkKey {
    pub fn new(kind: BackgroundWorkKind, provider_id: impl Into<String>) -> Self {
        Self {
            kind,
            provider_id: provider_id.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundWorkItem {
    pub key: BackgroundWorkKey,
    pub title: String,
    pub detail: Option<String>,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub output: Option<String>,
    pub output_truncated: bool,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
    pub duration_ms: Option<u64>,
    pub exit_code: Option<i32>,
    /// Whether the provider considers this detached from the foreground turn.
    pub background: bool,
    pub can_stop: bool,
    /// Provider-native identifier used for an authoritative stop request.
    pub control_id: Option<String>,
    /// Transcript activity that created this work, when the provider exposes it.
    pub origin_activity_id: Option<String>,
    pub role: Option<String>,
    pub model: Option<String>,
    pub parent_id: Option<String>,
    pub status: BackgroundWorkStatus,
}

impl BackgroundWorkItem {
    pub fn new(
        kind: BackgroundWorkKind,
        provider_id: impl Into<String>,
        title: impl Into<String>,
        status: BackgroundWorkStatus,
    ) -> Self {
        let now = unix_time_millis();
        Self {
            key: BackgroundWorkKey::new(kind, provider_id),
            title: title.into(),
            detail: None,
            command: None,
            cwd: None,
            output: None,
            output_truncated: false,
            started_at_ms: now,
            updated_at_ms: now,
            duration_ms: None,
            exit_code: None,
            background: false,
            can_stop: false,
            control_id: None,
            origin_activity_id: None,
            role: None,
            model: None,
            parent_id: None,
            status,
        }
    }

    /// Reconciles a live process item's status against terminal exit lines at
    /// the tail of its output. Leaves subagents and long-lived monitors untouched.
    pub fn settle_from_output(&mut self) -> bool {
        if self.key.kind == BackgroundWorkKind::Process
            && self.status.is_live()
            && let Some((code, settled_status)) =
                self.output.as_deref().and_then(parse_command_exit_status)
        {
            self.status = settled_status;
            if self.exit_code.is_none() {
                self.exit_code = Some(code);
            }
            self.can_stop = false;
            self.updated_at_ms = unix_time_millis();
            true
        } else {
            false
        }
    }

    /// Populates `exit_code` from output once for an already-settled process
    /// whose transport did not report a structured exit code.
    pub fn populate_exit_code_from_output(&mut self) {
        if self.key.kind == BackgroundWorkKind::Process
            && !self.status.is_live()
            && self.exit_code.is_none()
        {
            self.exit_code = self
                .output
                .as_deref()
                .and_then(parse_command_exit_status)
                .map(|(code, _)| code);
        }
    }
}

/// Strips ANSI CSI and OSC escape sequences from terminal output.
pub fn strip_ansi(text: &str) -> String {
    if !text.contains('\u{1b}') {
        return text.to_owned();
    }
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\u{07}' {
                            break;
                        }
                        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => clean.push(character),
            }
        } else {
            clean.push(character);
        }
    }
    clean
}

fn strip_ascii_case_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

fn is_trailing_wrapper_footer_line(line: &str) -> bool {
    strip_ascii_case_prefix(line, "Wall time:").is_some()
        || (line.starts_with('[') && line.ends_with(']'))
        || line.starts_with('…')
        || line.starts_with("...")
}

fn parse_exit_status_line(trimmed: &str) -> Option<(i32, BackgroundWorkStatus)> {
    if matches!(
        trimmed,
        "(command cancelled)" | "(process killed)" | "Command cancelled" | "Process killed"
    ) {
        return Some((130, BackgroundWorkStatus::Stopped));
    }

    if let Some(inner) = trimmed.strip_suffix(']')
        && let Some(rest) = strip_ascii_case_prefix(inner, "[Process exited")
        && rest.starts_with(char::is_whitespace)
        && let Ok(code) = rest.trim().parse::<i32>()
    {
        let status = if code == 0 {
            BackgroundWorkStatus::Completed
        } else {
            BackgroundWorkStatus::Failed
        };
        return Some((code, status));
    }

    for prefix in [
        "Command exited with code",
        "Process exited with code",
        "Exit code:",
        "Exit code",
    ] {
        let Some(rest) = strip_ascii_case_prefix(trimmed, prefix) else {
            continue;
        };
        if !prefix.ends_with(':') && !rest.starts_with(char::is_whitespace) {
            continue;
        }
        if let Ok(code) = rest.trim().trim_end_matches('.').parse::<i32>() {
            let status = if code == 0 {
                BackgroundWorkStatus::Completed
            } else {
                BackgroundWorkStatus::Failed
            };
            return Some((code, status));
        }
    }

    None
}

/// Parses a process/command exit status and exit code from the terminal footer
/// of tool output. Inspects only the trailing non-empty lines without copying
/// the full buffer.
pub fn parse_command_exit_status(text: &str) -> Option<(i32, BackgroundWorkStatus)> {
    let mut trailing_footer_lines = 0usize;
    for raw_line in text.lines().rev() {
        if raw_line.trim().is_empty() {
            continue;
        }
        let stripped;
        let trimmed = if raw_line.contains('\u{1b}') {
            stripped = strip_ansi(raw_line);
            stripped.trim()
        } else {
            raw_line.trim()
        };
        if trimmed.is_empty() {
            continue;
        }
        if let Some(parsed) = parse_exit_status_line(trimmed) {
            return Some(parsed);
        }
        if trailing_footer_lines < 3 && is_trailing_wrapper_footer_line(trimmed) {
            trailing_footer_lines += 1;
            continue;
        }
        break;
    }
    None
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum BackgroundWorkEvent {
    Upsert(BackgroundWorkItem),
    OutputDelta {
        key: BackgroundWorkKey,
        delta: String,
    },
    /// Authoritative snapshot of the provider's detached terminal registry.
    ReconcileProcesses {
        items: Vec<BackgroundWorkItem>,
    },
    /// Authoritative snapshot of all provider work still live. Used by
    /// transports which publish a level signal in addition to edge events.
    ReconcileLive {
        items: Vec<BackgroundWorkItem>,
    },
    StopRequested(BackgroundWorkKey),
    StopFailed {
        key: BackgroundWorkKey,
        message: String,
    },
}

/// A slash command a live provider process advertised for its session.
///
/// Claude's init handshake reports bare names; ACP agents report names with
/// descriptions. Sessions persisted by earlier builds stored plain strings,
/// which the untagged repr still accepts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReportedCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ReportedCommandRepr {
    Name(String),
    Full {
        name: String,
        #[serde(default)]
        description: String,
    },
}

impl From<ReportedCommandRepr> for ReportedCommand {
    fn from(repr: ReportedCommandRepr) -> Self {
        match repr {
            ReportedCommandRepr::Name(name) => Self {
                name,
                description: String::new(),
            },
            ReportedCommandRepr::Full { name, description } => Self { name, description },
        }
    }
}

impl<'de> Deserialize<'de> for ReportedCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        ReportedCommandRepr::deserialize(deserializer).map(Into::into)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
    pub allow: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// How the answer to a [`DriverEvent::UserInputRequested`] reaches the
/// provider.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UserInputDelivery {
    /// The provider is blocked on a request and the answer is its reply.
    #[default]
    Request,
    /// The provider asked without blocking (Codex `request_user_input_async`)
    /// and its turn carries on. The answer is sent as the next user message,
    /// and there is nothing to reply to when the user dismisses it.
    Message,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<UserInputOption>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputAnswer {
    pub question_id: String,
    pub answers: Vec<String>,
}

/// What a provider says happened to a file, when it says anything at all.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActivityFileChangeStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActivityFileChange {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ActivityFileChangeStatus>,
    /// Unified-diff body for this file, normalized once from whatever the
    /// provider sent: a real patch when it supplied one, otherwise synthesized
    /// from its before/after text. Rendering parses this instead of reaching
    /// back into raw tool arguments on every frame.
    ///
    /// Hunk headers are optional here: a bare `@@` line opens a hunk whose
    /// position in the file the provider never told us, which is the common
    /// case for string-replacement edit tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

impl ActivityFileChange {
    pub fn display_name(&self) -> &str {
        Path::new(&self.path)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.path)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActivityItem {
    pub id: Uuid,
    #[serde(default)]
    pub source_id: Option<String>,
    pub kind: ActivityKind,
    pub title: String,
    /// Native tool identity, separate from the human-readable activity title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// MCP server identity, kept separate so clients need not parse tool names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_server: Option<String>,
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Images returned by a tool, kept separate from text so large data URLs
    /// are never truncated or treated as literal activity output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_urls: Vec<String>,
    #[serde(default)]
    pub failed: bool,
    pub complete: bool,
    /// Provider-neutral edit metadata prepared when the tool event arrives.
    /// Rendering reads this directly instead of reparsing potentially large
    /// patches on every frame.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_changes: Vec<ActivityFileChange>,
    /// Compact subject prepared from native tool input (a file, query,
    /// directory, or command). The row builder only formats this cached value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_target: Option<String>,
    /// Human-authored command description prepared from native tool input.
    /// This stays separate from `display_target` so the UI can prefer a short
    /// label without discarding the raw command used by detail views.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_description: Option<String>,
    /// Native model reasoning carried by the same ordered activity stream as
    /// tool work. Generic provider `think` tools can still use the ordinary
    /// activity fields and leave this empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<crate::compaction::ContextCompaction>,
}

impl ActivityItem {
    /// Content that keeps its own ordered row, including after ordinary work folds.
    pub fn is_standalone(&self) -> bool {
        self.compaction.is_some() || self.kind == ActivityKind::ImageGeneration
    }

    pub fn from_compaction(compaction: crate::compaction::ContextCompaction) -> Self {
        use crate::compaction::CompactionStatus;
        let complete = compaction.status != CompactionStatus::Running;
        let title = tr!(match compaction.status {
            CompactionStatus::Running => "activity.compacting_context",
            CompactionStatus::Completed => "activity.compacted_context",
            CompactionStatus::Unconfirmed => "compaction.unconfirmed",
            CompactionStatus::Failed => "activity.compaction_failed",
            CompactionStatus::Interrupted => "compaction.interrupted",
        });
        let mut item = Self::new(
            Some(format!("compaction:{}", compaction.id)),
            ActivityKind::Compaction,
            title,
            None,
            complete,
        );
        item.failed = compaction.status == CompactionStatus::Failed;
        item.compaction = Some(compaction);
        item
    }

    pub fn new(
        source_id: Option<String>,
        kind: ActivityKind,
        title: impl Into<String>,
        detail: Option<String>,
        complete: bool,
    ) -> Self {
        let title = title.into();
        let display_target = fallback_activity_display_target(kind, &title);
        Self {
            id: Uuid::new_v4(),
            source_id,
            kind,
            title,
            tool_name: None,
            mcp_server: None,
            detail,
            arguments: None,
            output: None,
            image_urls: Vec::new(),
            failed: false,
            complete,
            file_changes: Vec::new(),
            display_target,
            display_description: None,
            reasoning: None,
            compaction: None,
        }
    }

    pub fn from_reasoning(reasoning: ReasoningBlock, complete: bool) -> Self {
        Self {
            reasoning: Some(reasoning),
            ..Self::new(None, ActivityKind::Reasoning, "Reasoning", None, complete)
        }
    }

    pub fn with_tool_name(mut self, name: Option<&str>) -> Self {
        if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
            // Keep an unwrapped `xd://` target tool name when the outer
            // transport (`write` / `read`) attaches its own name afterwards.
            let keeps_unwrapped_xdev_tool = matches!(name, "write" | "read")
                && self
                    .tool_name
                    .as_deref()
                    .is_some_and(|existing| !matches!(existing, "write" | "read"));
            if !keeps_unwrapped_xdev_tool {
                self.apply_tool_name(name);
            }
        }
        self
    }

    fn apply_tool_name(&mut self, name: &str) {
        if let Some((server, tool)) = name
            .strip_prefix("mcp__")
            .and_then(|name| name.split_once("__"))
            && !server.is_empty()
            && !tool.is_empty()
        {
            self.mcp_server = Some(server.to_owned());
            self.tool_name = Some(tool.to_owned());
        } else {
            self.mcp_server = None;
            self.tool_name = Some(name.to_owned());
        }
    }

    pub fn with_mcp_server(mut self, server: Option<&str>) -> Self {
        if let Some(server) = server.map(str::trim).filter(|server| !server.is_empty()) {
            self.mcp_server = Some(server.to_owned());
        }
        self
    }

    pub fn with_arguments(mut self, arguments: Option<String>) -> Self {
        self.arguments = arguments;
        self.refresh_activity_metadata();
        self
    }

    pub fn with_activity_source(mut self, source: Option<&serde_json::Value>) -> Self {
        if let Some(source) = source {
            self.refresh_activity_metadata_from_value(source);
        }
        self
    }

    pub fn with_output(mut self, output: Option<String>) -> Self {
        self.output = output;
        self.refresh_command_output();
        self
    }

    pub fn with_image_urls(mut self, image_urls: Vec<String>) -> Self {
        self.image_urls = image_urls;
        self
    }

    pub fn with_failed(mut self, failed: bool) -> Self {
        self.failed = failed;
        self
    }

    /// Extracts the common tool-input shapes emitted by every provider. This
    /// runs while handling an event (and once for legacy persisted rows), never
    /// from a transcript row builder.
    pub fn refresh_activity_metadata(&mut self) {
        if self.kind != ActivityKind::FileChange {
            self.file_changes.clear();
        }

        let source = self
            .arguments
            .as_deref()
            .map(str::trim)
            .filter(|source| !source.is_empty());
        if let Some(source) = source {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(source) {
                if let Some(unwrapped) = self.reclassify_xdev_tool_call(&value) {
                    if !unwrapped.is_null() {
                        self.refresh_activity_metadata_from_value(&unwrapped);
                    }
                } else {
                    self.refresh_activity_metadata_from_value(&value);
                }
            } else if self.kind == ActivityKind::FileChange {
                let extracted = parse_patch_file_changes(source);
                if !extracted.is_empty() {
                    self.file_changes = extracted;
                }
            }
        }
        if self.kind == ActivityKind::Command {
            self.arguments = self
                .arguments
                .take()
                .and_then(normalize_command_activity_command);
            if let Some(command) = self.arguments.as_deref() {
                self.display_target = Some(compact_activity_target(command));
            }
            self.reclassify_patch_command();
        }
        if self.display_target.is_none() {
            self.display_target = fallback_activity_display_target(self.kind, &self.title);
        }
        self.refresh_command_output();
    }

    /// Refile a shell command that only exists to apply a patch as the file
    /// change it really is. Runs once the command text has been unwrapped from
    /// whatever the provider wrapped it in, so the patch can be read out of it.
    fn reclassify_patch_command(&mut self) {
        let Some(changes) = self
            .arguments
            .as_deref()
            .and_then(apply_patch_command_body)
            .map(parse_patch_file_changes)
            .filter(|changes| !changes.is_empty())
        else {
            return;
        };
        self.kind = ActivityKind::FileChange;
        self.file_changes = changes;
        self.display_target = None;
        self.display_description = None;
    }

    /// Unwrap an Oh My Pi `xd://<tool>` virtual device call dispatched through
    /// `read` or `write` so it presents as the target tool rather than a file
    /// read or edit.
    fn reclassify_xdev_tool_call(
        &mut self,
        source: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        let raw_path = source.get("path").and_then(serde_json::Value::as_str)?;
        let (device, has_query) = parse_xdev_uri(raw_path)?;

        self.file_changes.clear();
        self.display_target = None;
        self.display_description = None;

        if !device.is_empty() && !has_query && source.get("content").is_some() {
            self.kind = ActivityKind::from_tool_name(device);
            self.apply_tool_name(device);
            if let Some(content) = source.get("content").and_then(serde_json::Value::as_str) {
                let trimmed = content.trim();
                if let Ok(inner) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    self.arguments = serde_json::to_string_pretty(&inner)
                        .ok()
                        .or_else(|| Some(trimmed.to_owned()));
                    if let Some(inner_title) = find_activity_string(&inner, &["title"], 0) {
                        self.title = compact_activity_target(&inner_title);
                    } else if is_generic_file_tool_title(&self.title) {
                        self.title = device.to_owned();
                    }
                    return Some(inner);
                }
                self.arguments = (!trimmed.is_empty()).then(|| trimmed.to_owned());
            }
            if is_generic_file_tool_title(&self.title) {
                self.title = device.to_owned();
            }
            return Some(serde_json::Value::Null);
        }

        self.kind = ActivityKind::Tool;
        if !device.is_empty() {
            self.apply_tool_name(device);
        } else {
            self.mcp_server = None;
            self.tool_name = Some("xdev".to_owned());
        }
        if is_generic_file_tool_title(&self.title) {
            self.title = if device.is_empty() {
                "xd://".to_owned()
            } else {
                device.to_owned()
            };
        }
        Some(serde_json::Value::Null)
    }

    fn refresh_activity_metadata_from_value(&mut self, source: &serde_json::Value) {
        if self.kind == ActivityKind::FileChange {
            let mut extracted = Vec::new();
            extract_file_changes_from_value(source, &mut extracted, 0);
            if !extracted.is_empty() {
                self.file_changes = extracted;
            }
        }
        if let Some(target) = extract_activity_display_target(self.kind, source) {
            self.display_target = Some(target);
        }
        if self.kind == ActivityKind::Command
            && let Some(description) =
                find_activity_string(source, &["description", "toolSummary"], 0)
        {
            self.display_description = Some(compact_activity_target(&description));
        }
    }

    /// Unwrap a tool-result envelope so the run or edit shows what the tool
    /// said rather than the provider's transport JSON around it. Text that is
    /// not a recognized envelope is returned untouched.
    fn refresh_command_output(&mut self) {
        if matches!(self.kind, ActivityKind::Command | ActivityKind::FileChange) {
            self.output = self
                .output
                .take()
                .and_then(normalize_command_activity_output);
        }
    }
}

/// Parses an Oh My Pi `xd://` virtual tool device URI into `(device, has_query)`.
///
/// `xd://` or `xd://?q=...` addresses the catalog (`device == ""`);
/// `xd://<tool>` addresses `<tool>`, with `?` requesting help rather than
/// execution.
pub fn parse_xdev_uri(raw_path: &str) -> Option<(&str, bool)> {
    let rest = raw_path
        .trim()
        .strip_prefix("xd://")?
        .trim_start_matches('/');
    let (device_part, has_query) = match rest.split_once('?') {
        Some((device, _)) => (device, true),
        None => (rest, false),
    };
    Some((device_part.trim_end_matches('/').trim(), has_query))
}

fn is_generic_file_tool_title(title: &str) -> bool {
    let trimmed = title.trim();
    trimmed.is_empty()
        || matches!(
            trimmed,
            "write" | "read" | "edit" | "Write file" | "Read file" | "Edit file"
        )
        || trimmed == tr!("activity.write_file")
        || trimmed == tr!("activity.read_file")
        || trimmed == tr!("activity.edit_file")
}

fn normalize_command_activity_command(source: String) -> Option<String> {
    let source = source.trim();
    if source.is_empty() {
        return None;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(source) else {
        return Some(source.to_owned());
    };
    match &value {
        serde_json::Value::String(command) => non_empty_activity_text(command),
        serde_json::Value::Array(parts) if parts.iter().all(|part| part.as_str().is_some()) => {
            non_empty_activity_text(
                &parts
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        }
        serde_json::Value::Object(_) => {
            find_activity_string(&value, &["command", "cmd", "script", "CommandLine"], 0)
                .and_then(|command| non_empty_activity_text(&command))
        }
        _ => Some(source.to_owned()),
    }
}

fn normalize_command_activity_output(output: String) -> Option<String> {
    let output = output.trim();
    if output.is_empty() {
        return None;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return Some(output.to_owned());
    };
    if !is_command_output_envelope(&value) {
        return Some(output.to_owned());
    }
    command_output_envelope_text(&value, 0)
}

fn non_empty_activity_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn is_command_output_envelope(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.contains_key("aggregatedOutput")
        || object.contains_key("structuredContent")
        || object.contains_key("stdout")
        || object.contains_key("stderr")
        || object.contains_key("toolCallId")
        || object.contains_key("tool_call_id")
    {
        return true;
    }
    let output_field = object.contains_key("content")
        || object.contains_key("result")
        || object.contains_key("output");
    if output_field && (object.contains_key("isError") || object.contains_key("is_error")) {
        return true;
    }
    let item_type = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(|value| {
            value
                .chars()
                .filter(|character| character.is_ascii_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        });
    if item_type.as_deref().is_some_and(|item_type| {
        matches!(
            item_type,
            "toolresult" | "tooloutput" | "commandresult" | "commandoutput" | "result" | "text"
        )
    }) {
        return true;
    }
    object
        .get("content")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| {
            !items.is_empty()
                && items.iter().all(|item| {
                    item.as_object().is_some_and(|item| {
                        item.get("type")
                            .and_then(serde_json::Value::as_str)
                            .is_some()
                    })
                })
        })
}

fn command_output_envelope_text(value: &serde_json::Value, depth: usize) -> Option<String> {
    if depth > 6 {
        return None;
    }
    match value {
        serde_json::Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            if let Ok(nested) = serde_json::from_str::<serde_json::Value>(text)
                && is_command_output_envelope(&nested)
            {
                return command_output_envelope_text(&nested, depth + 1);
            }
            Some(text.to_owned())
        }
        serde_json::Value::Array(items) => {
            let text = items
                .iter()
                .filter(|item| !is_command_output_image(item))
                .filter_map(|item| command_output_content_text(item, depth + 1))
                .collect::<Vec<_>>()
                .join("\n\n");
            non_empty_activity_text(&text)
        }
        serde_json::Value::Object(object) => {
            if object.get("type").and_then(serde_json::Value::as_str) == Some("text") {
                return object
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .and_then(non_empty_activity_text);
            }
            if let Some(structured) = object
                .get("structuredContent")
                .filter(|value| !value.is_null())
            {
                return serde_json::to_string_pretty(structured)
                    .ok()
                    .and_then(|text| non_empty_activity_text(&text));
            }
            if let Some(output) = object
                .get("aggregatedOutput")
                .and_then(serde_json::Value::as_str)
                .and_then(non_empty_activity_text)
            {
                return Some(output);
            }
            let streams = ["stdout", "stderr"]
                .into_iter()
                .filter_map(|key| {
                    object
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .and_then(non_empty_activity_text)
                })
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(streams) = non_empty_activity_text(&streams) {
                return Some(streams);
            }
            ["content", "result", "output", "message", "text"]
                .into_iter()
                .find_map(|key| {
                    object
                        .get(key)
                        .filter(|value| !value.is_null())
                        .and_then(|value| command_output_content_text(value, depth + 1))
                })
        }
        serde_json::Value::Null => None,
        value => non_empty_activity_text(&value.to_string()),
    }
}

fn command_output_content_text(value: &serde_json::Value, depth: usize) -> Option<String> {
    if is_command_output_envelope(value) || value.is_array() || value.is_string() {
        return command_output_envelope_text(value, depth);
    }
    if is_command_output_image(value) {
        return None;
    }
    serde_json::to_string_pretty(value)
        .ok()
        .and_then(|text| non_empty_activity_text(&text))
}

fn is_command_output_image(value: &serde_json::Value) -> bool {
    let item_type = value.get("type").and_then(serde_json::Value::as_str);
    let mime = value
        .get("mime")
        .or_else(|| value.get("mimeType"))
        .or_else(|| value.get("mime_type"))
        .and_then(serde_json::Value::as_str);
    matches!(item_type, Some("image" | "inputImage"))
        || (item_type == Some("file") && mime.is_some_and(|mime| mime.starts_with("image/")))
}

fn fallback_activity_display_target(kind: ActivityKind, title: &str) -> Option<String> {
    let title = title.trim();
    if title.is_empty() || is_generic_activity_title(kind, title) {
        return None;
    }
    (matches!(kind, ActivityKind::FileRead | ActivityKind::FileList)
        && (title.contains('/') || title.contains('\\') || Path::new(title).extension().is_some()))
    .then(|| compact_activity_target(title))
}

pub fn is_generic_activity_title(kind: ActivityKind, title: &str) -> bool {
    // Classification falling back to `Tool` only means the name is not one of
    // the semantic kinds above. It does not make the provider-supplied tool
    // name generic: otherwise every unknown tool (for example
    // `AskUserQuestion`) loses its identity in the transcript.
    if kind != ActivityKind::Tool && ActivityKind::from_tool_name(title) == kind {
        return true;
    }
    match kind {
        ActivityKind::Command => title == tr!("activity.run_command"),
        ActivityKind::FileChange => {
            title == tr!("activity.edit_file") || title == tr!("activity.write_file")
        }
        ActivityKind::FileRead => title == tr!("activity.read_file"),
        ActivityKind::FileSearch => {
            title == tr!("activity.search_files") || title == tr!("activity.find_files")
        }
        ActivityKind::FileList => title == tr!("activity.list_files"),
        ActivityKind::Plan => title == tr!("activity.plan_updated"),
        ActivityKind::Tool => title.eq_ignore_ascii_case("tool") || title == tr!("activity.tool"),
        _ => false,
    }
}

fn extract_activity_display_target(
    kind: ActivityKind,
    source: &serde_json::Value,
) -> Option<String> {
    let keys: &[&str] = match kind {
        ActivityKind::Command => &["command", "cmd", "CommandLine"],
        ActivityKind::FileRead => &[
            "filePath",
            "file_path",
            "path",
            "Path",
            "targetFile",
            "TargetFile",
            "target_file",
            "AbsolutePath",
            "notebookPath",
            "notebook_path",
        ],
        ActivityKind::FileSearch => &[
            "pattern",
            "Pattern",
            "query",
            "Query",
            "regex",
            "glob",
            "SearchPath",
        ],
        ActivityKind::FileList => &[
            "path",
            "Path",
            "directory",
            "DirectoryPath",
            "SearchDirectory",
            "dir",
            "root",
        ],
        ActivityKind::Search => &["query", "Query", "queries", "Url", "url"],
        ActivityKind::Tool => &["title", "toolSummary"],
        _ => return None,
    };
    find_activity_string(source, keys, 0).map(|value| compact_activity_target(&value))
}

fn find_activity_string(value: &serde_json::Value, keys: &[&str], depth: usize) -> Option<String> {
    if depth > 4 {
        return None;
    }
    match value {
        serde_json::Value::String(value) => serde_json::from_str::<serde_json::Value>(value)
            .ok()
            .and_then(|nested| find_activity_string(&nested, keys, depth + 1))
            .or_else(|| {
                let value = value.trim();
                (!value.is_empty()).then(|| value.to_owned())
            }),
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|value| find_activity_string(value, keys, depth + 1)),
        serde_json::Value::Object(object) => {
            for key in keys {
                let Some(value) = object.get(*key) else {
                    continue;
                };
                if let Some(value) = value.as_str().filter(|value| !value.trim().is_empty()) {
                    return Some(value.to_owned());
                }
                if let Some(value) = value.as_array().and_then(|values| {
                    values
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .find(|value| !value.trim().is_empty())
                }) {
                    return Some(value.to_owned());
                }
            }
            for key in [
                "action",
                "arguments",
                "args",
                "input",
                "params",
                "parameters",
                "rawInput",
                "raw_input",
                "toolInput",
                "tool_input",
            ] {
                if let Some(value) = object.get(key)
                    && let Some(found) = find_activity_string(value, keys, depth + 1)
                {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn compact_activity_target(value: &str) -> String {
    const MAX_CHARS: usize = 240;
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        return compact;
    }
    compact
        .chars()
        .take(MAX_CHARS - 1)
        .chain(std::iter::once('…'))
        .collect()
}

fn extract_file_changes_from_value(
    value: &serde_json::Value,
    changes: &mut Vec<ActivityFileChange>,
    depth: usize,
) {
    if depth > 4 {
        return;
    }
    match value {
        serde_json::Value::String(text) => {
            if let Ok(nested) = serde_json::from_str::<serde_json::Value>(text) {
                extract_file_changes_from_value(&nested, changes, depth + 1);
            } else {
                extend_file_changes(changes, parse_patch_file_changes(text));
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(change) = structured_file_change(item, None) {
                    merge_file_change(changes, change);
                } else {
                    extract_file_changes_from_value(item, changes, depth + 1);
                }
            }
        }
        serde_json::Value::Object(object) => {
            for key in ["changes", "fileChanges", "file_changes"] {
                let Some(collection) = object.get(key) else {
                    continue;
                };
                match collection {
                    serde_json::Value::Array(items) => {
                        for item in items {
                            if let Some(change) = structured_file_change(item, None) {
                                merge_file_change(changes, change);
                            } else {
                                extract_file_changes_from_value(item, changes, depth + 1);
                            }
                        }
                    }
                    serde_json::Value::Object(items) => {
                        for (path, item) in items {
                            if let Some(change) = structured_file_change(item, Some(path)) {
                                merge_file_change(changes, change);
                            }
                        }
                    }
                    _ => {}
                }
            }

            for key in ["patch", "patchText", "patch_text"] {
                if let Some(patch) = object.get(key).and_then(serde_json::Value::as_str) {
                    extend_file_changes(changes, parse_patch_file_changes(patch));
                }
            }

            let structured = structured_file_change(value, None);
            if structured.is_none() {
                for key in ["diff", "unifiedDiff", "unified_diff"] {
                    if let Some(patch) = object.get(key).and_then(serde_json::Value::as_str) {
                        extend_file_changes(changes, parse_patch_file_changes(patch));
                    }
                }
            }
            if let Some(change) = structured {
                merge_file_change(changes, change);
            }

            for key in [
                "arguments",
                "args",
                "input",
                "rawInput",
                "raw_input",
                "toolInput",
                "tool_input",
                "parameters",
            ] {
                if let Some(nested) = object.get(key) {
                    extract_file_changes_from_value(nested, changes, depth + 1);
                }
            }
        }
        _ => {}
    }
}

fn structured_file_change(
    value: &serde_json::Value,
    fallback_path: Option<&str>,
) -> Option<ActivityFileChange> {
    let object = value.as_object()?;
    let path = [
        "path",
        "Path",
        "filePath",
        "file_path",
        "filename",
        "fileName",
        "targetFile",
        "TargetFile",
        "target_file",
        "AbsolutePath",
        "notebookPath",
        "notebook_path",
    ]
    .into_iter()
    .find_map(|key| object.get(key).and_then(serde_json::Value::as_str))
    .or(fallback_path)?
    .trim();
    if path.is_empty() || path.starts_with("xd://") {
        return None;
    }

    let change_type = object
        .get("kind")
        .and_then(|kind| {
            kind.as_str()
                .or_else(|| kind.get("type").and_then(serde_json::Value::as_str))
        })
        .or_else(|| object.get("type").and_then(serde_json::Value::as_str));
    let status = match change_type {
        Some("add" | "create" | "added" | "write") => Some(ActivityFileChangeStatus::Added),
        Some("delete" | "deleted" | "remove") => Some(ActivityFileChangeStatus::Deleted),
        Some("update" | "edit" | "modify" | "modified") => Some(ActivityFileChangeStatus::Modified),
        _ => None,
    };

    let patch = ["diff", "unifiedDiff", "unified_diff"]
        .into_iter()
        .find_map(|key| object.get(key).and_then(serde_json::Value::as_str));
    let old = [
        "oldString",
        "old_string",
        "oldStr",
        "old_str",
        "oldText",
        "old_text",
        "oldContent",
        "old_content",
        "oldSource",
        "old_source",
        "Target",
        "TargetContent",
    ]
    .into_iter()
    .find_map(|key| object.get(key).and_then(serde_json::Value::as_str));
    let new = [
        "newString",
        "new_string",
        "newStr",
        "new_str",
        "newText",
        "new_text",
        "newContent",
        "new_content",
        "newSource",
        "new_source",
        "Replacement",
        "ReplacementContent",
    ]
    .into_iter()
    .find_map(|key| object.get(key).and_then(serde_json::Value::as_str));
    // A provider that reports an add or a delete puts the whole file where an
    // update would carry a patch, so read that key as content rather than as
    // hunks. Codex's `fileChange` item is the case in the field.
    let whole_file = matches!(
        status,
        Some(ActivityFileChangeStatus::Added | ActivityFileChangeStatus::Deleted)
    )
    .then(|| {
        object
            .get("content")
            .and_then(serde_json::Value::as_str)
            .or(patch)
    })
    .flatten();

    let body = if let Some(hunks) = object
        .get("structuredPatch")
        .or_else(|| object.get("structured_patch"))
        .and_then(serde_json::Value::as_array)
        .filter(|hunks| !hunks.is_empty())
    {
        // The one shape that reports where in the file the change landed.
        // Prefer it over the before/after text beside it, which does not.
        structured_patch_diff(hunks)
    } else if let Some(content) = whole_file {
        whole_file_diff(content, status == Some(ActivityFileChangeStatus::Added))
    } else if let Some(patch) = patch {
        normalize_unified_diff(patch)
    } else if let (Some(old), Some(new)) = (old, new) {
        replacement_diff(old, new)
    } else if let Some(edits) = object
        .get("edits")
        .or_else(|| object.get("Edits"))
        .or_else(|| object.get("ReplacementChunks"))
        .and_then(serde_json::Value::as_array)
    {
        edits_diff(edits)
    } else if let Some(content) = object
        .get("content")
        .or_else(|| object.get("Content"))
        .or_else(|| object.get("CodeContent"))
        .and_then(serde_json::Value::as_str)
    {
        // A write tool that only names a file and its new contents. Nothing
        // says whether the file already existed, so present it as added.
        whole_file_diff(content, true)
    } else {
        DiffBody::EMPTY
    };

    let (additions, deletions) = match &body.text {
        Some(_) => (Some(body.additions), Some(body.deletions)),
        None => (None, None),
    };
    Some(ActivityFileChange {
        path: path.to_owned(),
        additions,
        deletions,
        status,
        diff: body.text,
    })
}

/// Hunks a provider already positioned in the file, as Claude's edit tools
/// report them alongside their result. Each carries its own start lines, so
/// the rendered diff numbers both sides the way Git would.
fn structured_patch_diff(hunks: &[serde_json::Value]) -> DiffBody {
    let mut text = String::new();
    let mut additions = 0;
    let mut deletions = 0;
    let mut rendered = 0;
    for hunk in hunks {
        let Some(lines) = hunk.get("lines").and_then(serde_json::Value::as_array) else {
            continue;
        };
        let start = |keys: [&str; 2]| {
            keys.into_iter()
                .find_map(|key| hunk.get(key).and_then(serde_json::Value::as_u64))
                .unwrap_or(1)
        };
        let count = |keys: [&str; 2], fallback: usize| {
            keys.into_iter()
                .find_map(|key| hunk.get(key).and_then(serde_json::Value::as_u64))
                .unwrap_or(fallback as u64)
        };
        if rendered < MAX_ACTIVITY_DIFF_LINES {
            text.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                start(["oldStart", "old_start"]),
                count(["oldLines", "old_lines"], lines.len()),
                start(["newStart", "new_start"]),
                count(["newLines", "new_lines"], lines.len()),
            ));
        }
        for line in lines.iter().filter_map(serde_json::Value::as_str) {
            match line.as_bytes().first() {
                Some(b'+') => additions += 1,
                Some(b'-') => deletions += 1,
                _ => {}
            }
            if rendered >= MAX_ACTIVITY_DIFF_LINES {
                continue;
            }
            rendered += 1;
            text.push_str(line);
            text.push('\n');
        }
    }
    DiffBody {
        text: (!text.is_empty()).then_some(text),
        additions,
        deletions,
    }
}

/// One diff over a tool's list of independent replacements, in the order the
/// provider listed them. Each becomes its own hunk.
fn edits_diff(edits: &[serde_json::Value]) -> DiffBody {
    let mut text = String::new();
    let mut additions = 0;
    let mut deletions = 0;
    let mut rendered = 0;
    for edit in edits {
        let Some(edit) = edit.as_object() else {
            continue;
        };
        let old = [
            "oldString",
            "old_string",
            "oldStr",
            "old_str",
            "oldText",
            "old_text",
        ]
        .into_iter()
        .find_map(|key| edit.get(key).and_then(serde_json::Value::as_str));
        let new = [
            "newString",
            "new_string",
            "newStr",
            "new_str",
            "newText",
            "new_text",
        ]
        .into_iter()
        .find_map(|key| edit.get(key).and_then(serde_json::Value::as_str));
        let Some((old, new)) = old.zip(new) else {
            continue;
        };
        let body = replacement_diff(old, new);
        additions += body.additions;
        deletions += body.deletions;
        if let Some(body) = body.text
            && rendered < MAX_ACTIVITY_DIFF_LINES
        {
            rendered += body.lines().count();
            text.push_str(&body);
        }
    }
    DiffBody {
        text: (!text.is_empty()).then_some(text),
        additions,
        deletions,
    }
}

fn parse_patch_file_changes(patch: &str) -> Vec<ActivityFileChange> {
    #[derive(Default)]
    struct PendingChange {
        path: String,
        additions: u64,
        deletions: u64,
        count_lines: bool,
        status: Option<ActivityFileChangeStatus>,
        body: String,
        rendered: usize,
    }

    impl PendingChange {
        /// Keep the hunk in the body being assembled for this file. Counting
        /// continues past the render cap so the badge stays truthful.
        fn keep(&mut self, line: &str) {
            if self.rendered >= MAX_ACTIVITY_DIFF_LINES {
                return;
            }
            self.rendered += 1;
            self.body.push_str(line);
            self.body.push('\n');
        }
    }

    fn finish(pending: &mut Option<PendingChange>, changes: &mut Vec<ActivityFileChange>) {
        let Some(pending) = pending.take() else {
            return;
        };
        if pending.path.is_empty() || pending.path == "/dev/null" {
            return;
        }
        merge_file_change(
            changes,
            ActivityFileChange {
                path: pending.path,
                additions: Some(pending.additions),
                deletions: Some(pending.deletions),
                status: pending.status,
                diff: (!pending.body.is_empty()).then_some(pending.body),
            },
        );
    }

    let mut changes = Vec::new();
    let mut pending: Option<PendingChange> = None;
    for line in patch.lines() {
        let file_marker = [
            ("*** Update File: ", ActivityFileChangeStatus::Modified),
            ("*** Add File: ", ActivityFileChangeStatus::Added),
            ("*** Delete File: ", ActivityFileChangeStatus::Deleted),
        ]
        .into_iter()
        .find_map(|(prefix, status)| line.strip_prefix(prefix).map(|path| (path, status)));
        if let Some((path, status)) = file_marker {
            finish(&mut pending, &mut changes);
            pending = Some(PendingChange {
                path: path.trim().to_owned(),
                count_lines: true,
                status: Some(status),
                ..PendingChange::default()
            });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Move to: ") {
            if let Some(pending) = pending.as_mut() {
                pending.path = path.trim().to_owned();
            }
            continue;
        }
        if let Some(paths) = line.strip_prefix("diff --git ") {
            finish(&mut pending, &mut changes);
            let path = paths
                .split_whitespace()
                .next_back()
                .map(clean_diff_path)
                .unwrap_or_default();
            pending = Some(PendingChange {
                path,
                ..PendingChange::default()
            });
            continue;
        }
        if line.starts_with("@@") {
            if let Some(pending) = pending.as_mut() {
                pending.count_lines = true;
                pending.keep(line);
            }
            continue;
        }
        if let Some(pending) = pending.as_mut()
            && !pending.count_lines
        {
            if line.starts_with("new file mode ") {
                pending.status = Some(ActivityFileChangeStatus::Added);
            } else if line.starts_with("deleted file mode ") {
                pending.status = Some(ActivityFileChangeStatus::Deleted);
            }
        }
        if pending.as_ref().is_none_or(|pending| !pending.count_lines)
            && let Some(path) = line.strip_prefix("+++ ")
        {
            let path = clean_diff_path(path);
            if path != "/dev/null" {
                if let Some(pending) = pending.as_mut() {
                    pending.path = path;
                } else {
                    pending = Some(PendingChange {
                        path,
                        ..PendingChange::default()
                    });
                }
            }
            continue;
        }
        let Some(pending) = pending.as_mut() else {
            continue;
        };
        if !pending.count_lines {
            continue;
        }
        if line.starts_with('+') {
            pending.additions += 1;
        } else if line.starts_with('-') {
            pending.deletions += 1;
        } else if !line.starts_with(' ') && !line.is_empty() && !line.starts_with('\\') {
            // Codex's dialect ends a file section with the next `*** ` marker
            // and nothing else; anything unmarked here is not diff content.
            continue;
        }
        pending.keep(line);
    }
    finish(&mut pending, &mut changes);
    changes
}

/// The `*** Begin Patch` body of an `apply_patch` run through a shell tool.
///
/// Codex's models edit files by invoking `apply_patch` from the shell rather
/// than through a dedicated tool, and Codex's own TUI recognizes that and
/// presents it as a file change. Doing the same here keeps a real edit from
/// being filed under "ran a command" — and covers any other provider whose
/// model reaches for the same trick.
fn apply_patch_command_body(command: &str) -> Option<&str> {
    const BEGIN: &str = "*** Begin Patch";
    const END: &str = "*** End Patch";

    if !command.contains("apply_patch") {
        return None;
    }
    let start = command.find(BEGIN)?;
    let end = command[start..]
        .find(END)
        .map_or(command.len(), |offset| start + offset + END.len());
    Some(&command[start..end])
}

fn clean_diff_path(path: &str) -> String {
    path.trim()
        .trim_matches('"')
        .strip_prefix("a/")
        .or_else(|| path.trim().trim_matches('"').strip_prefix("b/"))
        .unwrap_or_else(|| path.trim().trim_matches('"'))
        .to_owned()
}

/// Unchanged lines kept around each changed region when a diff is synthesized
/// from a provider's before/after text.
const ACTIVITY_DIFF_CONTEXT_LINES: usize = 3;
/// Ceiling on one stored diff body. Tool arguments are already capped upstream,
/// so this only bounds what synthesis adds — a whole-file write is the case
/// that would otherwise copy an entire source file into the transcript twice.
const MAX_ACTIVITY_DIFF_LINES: usize = 1_000;

/// A synthesized or normalized diff plus the counts taken from the same pass,
/// so the `+N -N` badge can never disagree with the body under it.
struct DiffBody {
    text: Option<String>,
    additions: u64,
    deletions: u64,
}

impl DiffBody {
    const EMPTY: Self = Self {
        text: None,
        additions: 0,
        deletions: 0,
    };
}

/// Diff between the before and after text of a string-replacement edit.
///
/// The fragments carry no position, so hunks open with a bare `@@` rather than
/// invented line numbers. Counts come from the same walk as the body.
fn replacement_diff(old: &str, new: &str) -> DiffBody {
    // Compare over already-split lines: `from_lines` keeps each line's
    // terminator, so a final line without one would never match the same text
    // elsewhere in the file.
    let old = old.lines().collect::<Vec<_>>();
    let new = new.lines().collect::<Vec<_>>();
    let diff = similar::TextDiff::from_slices(&old, &new);
    let mut text = String::new();
    let mut additions = 0;
    let mut deletions = 0;
    let mut rendered = 0;
    for group in diff.grouped_ops(ACTIVITY_DIFF_CONTEXT_LINES) {
        if rendered < MAX_ACTIVITY_DIFF_LINES {
            text.push_str("@@\n");
        }
        for op in &group {
            for change in diff.iter_changes(op) {
                let marker = match change.tag() {
                    similar::ChangeTag::Equal => ' ',
                    similar::ChangeTag::Delete => {
                        deletions += 1;
                        '-'
                    }
                    similar::ChangeTag::Insert => {
                        additions += 1;
                        '+'
                    }
                };
                // Keep counting past the cap: the badge stays truthful even
                // when the body stops.
                if rendered >= MAX_ACTIVITY_DIFF_LINES {
                    continue;
                }
                rendered += 1;
                text.push(marker);
                text.push_str(change.value());
                text.push('\n');
            }
        }
    }
    DiffBody {
        text: (!text.is_empty()).then_some(text),
        additions,
        deletions,
    }
}

/// Diff for a file the provider reported as whole new or whole removed content.
/// One side of the file is empty, so the hunk header carries real positions.
fn whole_file_diff(content: &str, added: bool) -> DiffBody {
    let total = logical_line_count(content);
    if total == 0 {
        return DiffBody::EMPTY;
    }
    let marker = if added { '+' } else { '-' };
    let mut text = if added {
        format!("@@ -0,0 +1,{total} @@\n")
    } else {
        format!("@@ -1,{total} +0,0 @@\n")
    };
    for line in content.lines().take(MAX_ACTIVITY_DIFF_LINES) {
        text.push(marker);
        text.push_str(line);
        text.push('\n');
    }
    DiffBody {
        text: Some(text),
        additions: if added { total } else { 0 },
        deletions: if added { 0 } else { total },
    }
}

/// Strip a provider patch down to hunks. Git file headers only appear before
/// the first hunk, so dropping them by prefix stops there — past it, a line
/// such as `--- x` is a deletion of `-- x`, not a header.
fn normalize_unified_diff(diff: &str) -> DiffBody {
    let mut text = String::with_capacity(diff.len());
    let mut additions = 0;
    let mut deletions = 0;
    let mut rendered = 0;
    // A patch with no hunk header at all is a bare body; open a positionless
    // hunk for it rather than discarding every line looking for a header.
    let mut in_hunk = !diff.lines().any(|line| line.starts_with("@@"));
    if in_hunk {
        text.push_str("@@\n");
    }
    for line in diff.lines() {
        if !in_hunk {
            if !line.starts_with("@@") {
                continue;
            }
            in_hunk = true;
        } else if line.starts_with('+') {
            additions += 1;
        } else if line.starts_with('-') {
            deletions += 1;
        }
        if rendered >= MAX_ACTIVITY_DIFF_LINES {
            continue;
        }
        rendered += 1;
        text.push_str(line);
        text.push('\n');
    }
    DiffBody {
        text: (!text.is_empty()).then_some(text),
        additions,
        deletions,
    }
}

fn logical_line_count(text: &str) -> u64 {
    if text.is_empty() {
        0
    } else {
        text.lines().count() as u64
    }
}

fn extend_file_changes(
    changes: &mut Vec<ActivityFileChange>,
    extracted: impl IntoIterator<Item = ActivityFileChange>,
) {
    for change in extracted {
        merge_file_change(changes, change);
    }
}

fn merge_file_change(changes: &mut Vec<ActivityFileChange>, change: ActivityFileChange) {
    let Some(existing) = changes.iter_mut().find(|item| item.path == change.path) else {
        changes.push(change);
        return;
    };
    match (existing.additions, change.additions) {
        (Some(existing_count), Some(change_count)) => {
            existing.additions = Some(existing_count + change_count);
        }
        (None, Some(change_count)) => existing.additions = Some(change_count),
        _ => {}
    }
    match (existing.deletions, change.deletions) {
        (Some(existing_count), Some(change_count)) => {
            existing.deletions = Some(existing_count + change_count);
        }
        (None, Some(change_count)) => existing.deletions = Some(change_count),
        _ => {}
    }
    existing.status = existing.status.or(change.status);
    match (existing.diff.as_mut(), change.diff) {
        (Some(existing_diff), Some(diff)) => existing_diff.push_str(&diff),
        (None, diff @ Some(_)) => existing.diff = diff,
        _ => {}
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReasoningBlock {
    pub content: String,
    /// Zero when only a final snapshot was available; no elapsed time can be
    /// inferred from the arrival of a summary alone.
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
}

#[derive(Clone, Debug)]
pub struct TranscriptBlock {
    /// Render this block immediately after this many persisted messages.
    pub after_message: usize,
    pub turn_id: Option<Uuid>,
    /// Ordered non-message work emitted at this point in the transcript.
    /// The persisted field keeps its historical tagged shape so existing
    /// sessions remain readable while the runtime model stays activity-only.
    pub activities: Vec<ActivityItem>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "data")]
enum StoredTranscriptBlockContentRef<'a> {
    Activities(&'a [ActivityItem]),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "data")]
pub enum StoredTranscriptBlockContent {
    Reasoning(ReasoningBlock),
    Activities(Vec<ActivityItem>),
}

#[derive(Serialize)]
struct TranscriptBlockRef<'a> {
    after_message: usize,
    turn_id: Option<Uuid>,
    content: StoredTranscriptBlockContentRef<'a>,
}

impl Serialize for TranscriptBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        TranscriptBlockRef {
            after_message: self.after_message,
            turn_id: self.turn_id,
            content: StoredTranscriptBlockContentRef::Activities(&self.activities),
        }
        .serialize(serializer)
    }
}

#[derive(Deserialize)]
struct TranscriptBlockRepr {
    after_message: usize,
    #[serde(default)]
    turn_id: Option<Uuid>,
    content: StoredTranscriptBlockContent,
}

impl<'de> Deserialize<'de> for TranscriptBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let repr = TranscriptBlockRepr::deserialize(deserializer)?;
        let activities = match repr.content {
            StoredTranscriptBlockContent::Reasoning(reasoning) => {
                vec![ActivityItem::from_reasoning(reasoning, true)]
            }
            StoredTranscriptBlockContent::Activities(activities) => activities,
        };
        Ok(Self {
            after_message: repr.after_message,
            turn_id: repr.turn_id,
            activities,
        })
    }
}

#[derive(Clone, Debug)]
pub struct PendingPermission {
    pub request_id: String,
    pub title: String,
    pub detail: String,
    pub options: Vec<PermissionOption>,
}

pub fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

pub fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

pub fn compact_path(path: &Path) -> String {
    let components = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    if components.len() <= 3 {
        return path.display().to_string();
    }
    format!(
        "…/{}/{}",
        components[components.len() - 2],
        components[components.len() - 1]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_plan_access_mode_loads_as_supervised() {
        let mode: RuntimeMode = serde_json::from_str(r#""plan""#).unwrap();

        assert_eq!(mode, RuntimeMode::Ask);
        assert_eq!(serde_json::to_string(&mode).unwrap(), r#""ask""#);
    }

    #[test]
    fn background_work_snapshots_have_serializable_named_items() {
        let item = BackgroundWorkItem::new(
            BackgroundWorkKind::Process,
            "process-1",
            "server",
            BackgroundWorkStatus::Running,
        );
        let json =
            serde_json::to_value(BackgroundWorkEvent::ReconcileProcesses { items: vec![item] })
                .unwrap();

        assert_eq!(json["type"], "reconcileProcesses");
        assert_eq!(json["items"][0]["key"]["providerId"], "process-1");
        let BackgroundWorkEvent::ReconcileProcesses { items } =
            serde_json::from_value(json).unwrap()
        else {
            panic!("unexpected background-work event");
        };
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn parse_command_exit_status_detects_exit_codes_and_ignores_utf8_or_midstream_text() {
        assert_eq!(
            parse_command_exit_status("Wall time: 44.30 seconds\n\nCommand exited with code 101"),
            Some((101, BackgroundWorkStatus::Failed))
        );
        assert_eq!(
            parse_command_exit_status(
                "Command exited with code 101\nWall time: 44.30 seconds\n[raw output: artifact://4]\n"
            ),
            Some((101, BackgroundWorkStatus::Failed))
        );
        assert_eq!(
            parse_command_exit_status("Command exited with code 0"),
            Some((0, BackgroundWorkStatus::Completed))
        );
        assert_eq!(
            parse_command_exit_status("Process exited with code 1"),
            Some((1, BackgroundWorkStatus::Failed))
        );
        assert_eq!(
            parse_command_exit_status("Exit code: 0"),
            Some((0, BackgroundWorkStatus::Completed))
        );
        assert_eq!(
            parse_command_exit_status("exit code 2"),
            Some((2, BackgroundWorkStatus::Failed))
        );
        assert_eq!(
            parse_command_exit_status("[Process exited 127]"),
            Some((127, BackgroundWorkStatus::Failed))
        );
        assert_eq!(
            parse_command_exit_status("(command cancelled)"),
            Some((130, BackgroundWorkStatus::Stopped))
        );
        assert_eq!(
            parse_command_exit_status(
                "\u{1b}]133;D;1\u{07}\u{1b}[31mCommand exited with code 1\u{1b}[0m"
            ),
            Some((1, BackgroundWorkStatus::Failed))
        );
        assert_eq!(
            parse_command_exit_status("界界界界\n──────────\n测试输出 🚀"),
            None
        );
        assert_eq!(
            parse_command_exit_status("Exit code: 1\nCompiling waku v0.1.0\nrunning tests..."),
            None
        );
    }

    #[test]
    fn background_work_item_settles_only_processes_from_output() {
        let mut process = BackgroundWorkItem::new(
            BackgroundWorkKind::Process,
            "proc-1",
            "cargo test",
            BackgroundWorkStatus::Running,
        );
        process.can_stop = true;
        process.output = Some("test failed\nCommand exited with code 101\n".into());
        assert!(process.settle_from_output());
        assert_eq!(process.status, BackgroundWorkStatus::Failed);
        assert_eq!(process.exit_code, Some(101));
        assert!(!process.status.is_live());
        assert!(!process.can_stop);

        for (kind, status) in [
            (BackgroundWorkKind::Subagent, BackgroundWorkStatus::Running),
            (
                BackgroundWorkKind::Monitor,
                BackgroundWorkStatus::Monitoring,
            ),
        ] {
            let mut item = BackgroundWorkItem::new(kind, "live-1", "worker", status);
            item.can_stop = true;
            item.output = Some("quoted failure:\nExit code: 1\n".into());
            assert!(!item.settle_from_output());
            assert_eq!(item.status, status);
            assert_eq!(item.exit_code, None);
            assert!(item.can_stop);
        }
    }

    #[test]
    fn attachment_messages_keep_transport_and_visible_content_separate() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        let attachment = MessageAttachment {
            path: PathBuf::from("/tmp/reference.png"),
            mention: "/tmp/reference.png".to_owned(),
            name: "reference.png".to_owned(),
            is_dir: false,
            is_image: true,
            blob_reference: Some("waku-blob:ab/reference.png".to_owned()),
            pasted_text: None,
        };

        session.begin_turn_with_presentation(
            "compare this @/tmp/reference.png",
            Some("compare this".to_owned()),
            vec![attachment.clone()],
        );

        let message = &session.messages[0];
        assert_eq!(message.content, "compare this @/tmp/reference.png");
        assert_eq!(message.visible_content(), "compare this");
        assert_eq!(message.attachments, vec![attachment]);
    }

    #[test]
    fn activity_tool_identity_preserves_names_and_separates_mcp_servers() {
        let mcp = ActivityItem::new(None, ActivityKind::Tool, "Read notes", None, true)
            .with_tool_name(Some("mcp__filesystem__read_file"));
        assert_eq!(mcp.title, "Read notes");
        assert_eq!(mcp.tool_name.as_deref(), Some("read_file"));
        assert_eq!(mcp.mcp_server.as_deref(), Some("filesystem"));
        let regular = ActivityItem::new(None, ActivityKind::Tool, "Read notes", None, true)
            .with_tool_name(Some("read_file"));
        assert_eq!(regular.tool_name.as_deref(), Some("read_file"));
        assert_eq!(regular.mcp_server, None);
        let legacy: ActivityItem = serde_json::from_value(serde_json::json!({
            "id": Uuid::nil(), "kind": "tool", "title": "Read notes", "detail": null,
            "complete": true,
        }))
        .unwrap();
        assert!(legacy.tool_name.is_none());
        assert!(legacy.mcp_server.is_none());
    }

    #[test]
    fn tool_names_are_classified_without_substring_false_positives() {
        for name in [
            "read",
            "ReadFile",
            "read_text_file",
            "mcp__filesystem__read_file",
        ] {
            assert_eq!(
                ActivityKind::from_tool_name(name),
                ActivityKind::FileRead,
                "{name}"
            );
        }
        for name in ["grep", "Glob", "fileSearch", "search_files"] {
            assert_eq!(
                ActivityKind::from_tool_name(name),
                ActivityKind::FileSearch,
                "{name}"
            );
        }
        for name in ["ls", "ListDirectory", "read_dir"] {
            assert_eq!(
                ActivityKind::from_tool_name(name),
                ActivityKind::FileList,
                "{name}"
            );
        }
        for name in ["WriteFile", "applyPatch", "move_file", "str_replace"] {
            assert_eq!(
                ActivityKind::from_tool_name(name),
                ActivityKind::FileChange,
                "{name}"
            );
        }
        for name in ["create_thread", "read_mcp_resource", "list_threads"] {
            assert_eq!(
                ActivityKind::from_tool_name(name),
                ActivityKind::Tool,
                "{name}"
            );
        }
    }

    #[test]
    fn fallback_tool_names_are_not_treated_as_generic_titles() {
        assert!(!is_generic_activity_title(
            ActivityKind::Tool,
            "AskUserQuestion"
        ));
        assert!(!is_generic_activity_title(
            ActivityKind::Tool,
            "mcp__threads__create_thread"
        ));
        assert!(is_generic_activity_title(ActivityKind::Tool, "Tool"));
    }

    #[test]
    fn activity_targets_are_normalized_when_events_arrive() {
        let cases = [
            (
                ActivityKind::FileRead,
                serde_json::json!({"input": {"file_path": "/tmp/waku/src/app.rs"}}),
                "/tmp/waku/src/app.rs",
            ),
            (
                ActivityKind::FileSearch,
                serde_json::json!({"tool_input": {"regex": "ActivityItem"}}),
                "ActivityItem",
            ),
            (
                ActivityKind::FileList,
                serde_json::json!({"arguments": {"directory": "/tmp/waku/src"}}),
                "/tmp/waku/src",
            ),
            (
                ActivityKind::Command,
                serde_json::json!({"args": {"command": "cargo test activity"}}),
                "cargo test activity",
            ),
            (
                ActivityKind::Search,
                serde_json::json!({"action": {"queries": ["Waku GPUI"]}}),
                "Waku GPUI",
            ),
            (
                ActivityKind::FileRead,
                serde_json::json!("/tmp/waku/README.md"),
                "/tmp/waku/README.md",
            ),
        ];

        for (kind, arguments, expected) in cases {
            let activity = ActivityItem::new(None, kind, "tool", None, false)
                .with_arguments(Some(arguments.to_string()));
            assert_eq!(
                activity.display_target.as_deref(),
                Some(expected),
                "{kind:?}"
            );
        }

        let described = ActivityItem::new(None, ActivityKind::Command, "bash", None, false)
            .with_arguments(Some(
                serde_json::json!({
                    "command": "python3 analyze.py",
                    "description": "Analyze color statistics"
                })
                .to_string(),
            ));
        assert_eq!(
            described.display_description.as_deref(),
            Some("Analyze color statistics")
        );
        assert_eq!(described.arguments.as_deref(), Some("python3 analyze.py"));
    }

    #[test]
    fn command_output_unwraps_provider_results_without_rewriting_real_output() {
        let wrapped = serde_json::json!({
            "type": "tool-result",
            "toolCallId": "call-1",
            "content": [{
                "type": "text",
                "text": "first line\n{\"actual\":\"command json\"}"
            }],
            "isError": false
        })
        .to_string();
        let activity = ActivityItem::new(None, ActivityKind::Command, "bash", None, true)
            .with_arguments(Some(
                serde_json::json!({
                    "command": "printf output",
                    "description": "Print output"
                })
                .to_string(),
            ))
            .with_output(Some(wrapped));

        assert_eq!(activity.arguments.as_deref(), Some("printf output"));
        assert_eq!(
            activity.output.as_deref(),
            Some("first line\n{\"actual\":\"command json\"}")
        );

        let json_output = r#"{"content":"this came from the command"}"#;
        let activity = ActivityItem::new(None, ActivityKind::Command, "bash", None, true)
            .with_output(Some(json_output.to_owned()));
        assert_eq!(activity.output.as_deref(), Some(json_output));
    }

    #[test]
    fn legacy_command_metadata_is_normalized_on_refresh() {
        let mut activity = ActivityItem::new(None, ActivityKind::Command, "bash", None, true);
        activity.arguments =
            Some(r#"{"command":"git status","description":"Check status"}"#.into());
        activity.output = Some(
            r#"{"type":"tool-result","content":[{"type":"text","text":"clean"}],"isError":false}"#
                .into(),
        );

        activity.refresh_activity_metadata();

        assert_eq!(activity.arguments.as_deref(), Some("git status"));
        assert_eq!(activity.display_target.as_deref(), Some("git status"));
        assert_eq!(
            activity.display_description.as_deref(),
            Some("Check status")
        );
        assert_eq!(activity.output.as_deref(), Some("clean"));
    }

    #[test]
    fn file_edit_metadata_is_normalized_for_every_provider_shape() {
        let cases = [
            (
                ProviderKind::Codex,
                serde_json::json!([{
                    "path": "src/codex.rs",
                    "diff": "@@ -1 +1,2 @@\n-old\n+new\n+next",
                    "kind": {"type": "update"}
                }]),
                "src/codex.rs",
                2,
                1,
            ),
            (
                // A line kept across the replacement is context, not one
                // deletion plus one addition.
                ProviderKind::Claude,
                serde_json::json!({
                    "file_path": "src/claude.rs",
                    "old_string": "old\nline",
                    "new_string": "new\nline\nadded"
                }),
                "src/claude.rs",
                2,
                1,
            ),
            (
                ProviderKind::Pi,
                serde_json::json!({
                    "path": "src/pi.rs",
                    "edits": [{"oldText": "old", "newText": "new\nmore"}]
                }),
                "src/pi.rs",
                2,
                1,
            ),
        ];

        for (provider, arguments, path, additions, deletions) in cases {
            let activity = ActivityItem::new(
                Some(format!("{}-edit", provider.id())),
                ActivityKind::FileChange,
                "edit",
                None,
                false,
            )
            .with_arguments(Some(arguments.to_string()));
            assert_eq!(activity.file_changes.len(), 1, "{provider:?}");
            let change = &activity.file_changes[0];
            assert_eq!(change.path, path, "{provider:?}");
            assert_eq!(change.additions, Some(additions), "{provider:?}");
            assert_eq!(change.deletions, Some(deletions), "{provider:?}");
            // Every shape must reach rendering as a diff, and the counts the
            // row badge shows must be the ones its body accounts for.
            let diff = change.diff.as_deref().unwrap_or_else(|| {
                panic!("{provider:?} edit should carry a diff body");
            });
            let (rendered_additions, rendered_deletions) = diff
                .lines()
                .skip_while(|line| !line.starts_with("@@"))
                .fold((0, 0), |(added, deleted), line| match line.as_bytes() {
                    [b'+', ..] => (added + 1, deleted),
                    [b'-', ..] => (added, deleted + 1),
                    _ => (added, deleted),
                });
            assert_eq!(rendered_additions, additions, "{provider:?}");
            assert_eq!(rendered_deletions, deletions, "{provider:?}");
        }
    }

    #[test]
    fn positioned_hunks_beat_the_before_and_after_text_beside_them() {
        // Claude's edit tools answer with the patch they applied. Its hunks
        // know where they landed; `old_string`/`new_string` never do.
        let activity = ActivityItem::new(
            Some("toolu_1".into()),
            ActivityKind::FileChange,
            "Edit",
            None,
            true,
        )
        .with_activity_source(Some(&serde_json::json!({
            "filePath": "/tmp/f.txt",
            "oldString": "bravo",
            "newString": "BRAVO",
            "structuredPatch": [{
                "oldStart": 1,
                "oldLines": 4,
                "newStart": 1,
                "newLines": 4,
                "lines": [" alpha", "-bravo", "+BRAVO", " charlie"]
            }]
        })));

        let change = &activity.file_changes[0];
        assert_eq!(change.path, "/tmp/f.txt");
        assert_eq!(change.additions, Some(1));
        assert_eq!(change.deletions, Some(1));
        assert_eq!(
            change.diff.as_deref(),
            Some("@@ -1,4 +1,4 @@\n alpha\n-bravo\n+BRAVO\n charlie\n")
        );
    }

    #[test]
    fn whole_file_writes_diff_against_an_empty_file() {
        let activity = ActivityItem::new(
            Some("write-1".into()),
            ActivityKind::FileChange,
            "Write",
            None,
            true,
        )
        .with_arguments(Some(
            serde_json::json!({
                "file_path": "src/new.rs",
                "content": "fn main() {}\n"
            })
            .to_string(),
        ));

        let change = &activity.file_changes[0];
        assert_eq!(change.additions, Some(1));
        assert_eq!(change.deletions, Some(0));
        // Positions are real here: a created file starts at line 1.
        assert_eq!(
            change.diff.as_deref(),
            Some("@@ -0,0 +1,1 @@\n+fn main() {}\n")
        );
    }

    #[test]
    fn codex_add_and_delete_changes_carry_their_whole_file_as_the_diff() {
        for (kind, additions, deletions, marker) in [
            ("add", Some(2), Some(0), '+'),
            ("delete", Some(0), Some(2), '-'),
        ] {
            let activity = ActivityItem::new(
                Some(format!("codex-{kind}")),
                ActivityKind::FileChange,
                "File Change",
                None,
                true,
            )
            .with_arguments(Some(
                serde_json::json!([{
                    "path": "src/codex.rs",
                    "kind": {"type": kind},
                    "diff": "first\nsecond"
                }])
                .to_string(),
            ));

            let change = &activity.file_changes[0];
            assert_eq!(change.additions, additions, "{kind}");
            assert_eq!(change.deletions, deletions, "{kind}");
            let diff = change.diff.as_deref().expect("whole-file diff");
            assert!(
                diff.lines()
                    .skip(1)
                    .all(|line| line.starts_with(marker) && line.len() > 1),
                "{kind}: {diff}"
            );
        }
    }

    #[test]
    fn apply_patch_run_through_a_shell_becomes_a_file_change() {
        let mut activity = ActivityItem::new(
            Some("exec-1".into()),
            ActivityKind::Command,
            "Shell",
            None,
            true,
        );
        activity.arguments = Some(
            serde_json::json!({
                "command": "apply_patch <<'PATCH'\n*** Begin Patch\n*** Update File: src/one.rs\n@@\n-old\n+new\n*** End Patch\nPATCH",
            })
            .to_string(),
        );
        activity.refresh_activity_metadata();

        assert_eq!(activity.kind, ActivityKind::FileChange);
        assert_eq!(activity.file_changes.len(), 1);
        assert_eq!(activity.file_changes[0].path, "src/one.rs");
        assert_eq!(activity.file_changes[0].additions, Some(1));
        assert_eq!(activity.file_changes[0].deletions, Some(1));
        assert_eq!(
            activity.file_changes[0].diff.as_deref(),
            Some("@@\n-old\n+new\n")
        );
    }

    #[test]
    fn a_command_that_only_mentions_a_patch_stays_a_command() {
        let mut activity = ActivityItem::new(
            Some("exec-2".into()),
            ActivityKind::Command,
            "Shell",
            None,
            true,
        );
        activity.arguments =
            Some(serde_json::json!({"command": "git apply_patch --help"}).to_string());
        activity.refresh_activity_metadata();

        assert_eq!(activity.kind, ActivityKind::Command);
        assert!(activity.file_changes.is_empty());
    }

    #[test]
    fn apply_patch_metadata_keeps_each_file_and_its_counts() {
        let activity = ActivityItem::new(
            Some("patch-1".into()),
            ActivityKind::FileChange,
            "apply_patch",
            None,
            true,
        )
        .with_arguments(Some(
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/one.rs\n@@\n-old\n+new\n*** Add File: src/two.rs\n+first\n+second\n*** End Patch"
            })
            .to_string(),
        ));

        assert_eq!(activity.file_changes.len(), 2);
        assert_eq!(activity.file_changes[0].path, "src/one.rs");
        assert_eq!(activity.file_changes[0].additions, Some(1));
        assert_eq!(activity.file_changes[0].deletions, Some(1));
        assert_eq!(activity.file_changes[1].path, "src/two.rs");
        assert_eq!(activity.file_changes[1].additions, Some(2));
        assert_eq!(activity.file_changes[1].deletions, Some(0));
    }

    #[test]
    fn projectless_projects_use_projects_root_and_recognize_legacy_paths() {
        let home = dirs::home_dir().expect("test user has a home directory");
        let root = home.join(".waku");
        let legacy = Project::from_path(root.clone());
        let legacy_dated = Project::from_path(root.join("2026-08-08/new-chat"));
        let project = Project::from_path(root.join("projects/2026-08-08/new-chat"));
        let ordinary = Project::from_path(home.join("dev/waku"));

        assert!(legacy.is_projectless());
        assert!(legacy_dated.is_projectless());
        assert!(project.is_projectless());
        assert!(!ordinary.is_projectless());
    }

    #[test]
    fn prompt_generates_a_short_session_title() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.set_title_from_prompt("build a really polished local agent interface for rust");
        assert_eq!(
            session.auto_title.as_deref(),
            Some("build a really polished local agent interface")
        );
        assert_eq!(
            session.display_title(),
            "build a really polished local agent interface"
        );
        assert_eq!(session.title, AgentSession::DEFAULT_TITLE);
    }

    #[test]
    fn provider_title_replaces_prompt_fallback_but_not_an_explicit_title() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.set_title_from_prompt("investigate the broken provider event");

        assert!(session.set_auto_title(Some("Fix provider title events".into())));
        assert_eq!(session.display_title(), "Fix provider title events");

        assert!(session.set_title("  My title  "));
        assert!(session.set_auto_title(Some("A newer provider title".into())));
        assert_eq!(session.display_title(), "My title");
        assert!(!session.set_title("   "));
        assert_eq!(session.display_title(), "My title");
    }

    #[test]
    fn automatic_titles_drop_leading_icons_for_every_provider() {
        for provider in ProviderKind::ALL {
            let mut session = AgentSession::new(Uuid::new_v4(), provider);
            assert!(session.set_auto_title(Some(" 🤖 MODELS: 排查模型安装 ".into())));
            assert_eq!(session.auto_title.as_deref(), Some("MODELS: 排查模型安装"));
            assert_eq!(session.display_title(), "MODELS: 排查模型安装");
            assert!(!session.set_auto_title(Some("🖥️ MODELS: 排查模型安装".into())));

            // Existing stored titles and list projections also lose the icon
            // without rewriting provider history or the user's explicit name.
            session.auto_title = Some("👩🏽‍💻 COMP: Assess the change".into());
            assert_eq!(
                session.list_projection().display_title(),
                "COMP: Assess the change"
            );
            assert!(session.set_title("🧠 My title"));
            assert_eq!(session.display_title(), "🧠 My title");
            assert!(session.set_auto_title(Some("🤖".into())));
            assert!(session.auto_title.is_none());
            assert_eq!(session.display_title(), "🧠 My title");
        }
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        session.auto_title = Some("🖥️".into());
        assert_eq!(session.display_title(), AgentSession::DEFAULT_TITLE);
    }

    #[test]
    fn model_selection_keeps_started_sessions_on_their_provider() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);

        assert!(session.can_choose_model(ProviderKind::Claude));

        session.push_message(MessageRole::User, "first turn");
        assert!(session.can_choose_model(ProviderKind::Codex));
        assert!(!session.can_choose_model(ProviderKind::Claude));
    }

    #[test]
    fn model_selection_waits_for_the_active_turn_to_finish() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.push_message(MessageRole::User, "first turn");

        for status in [
            SessionStatus::Connecting,
            SessionStatus::Working,
            SessionStatus::Waiting,
        ] {
            session.status = status;
            assert!(!session.can_choose_model(ProviderKind::Codex));
        }

        session.status = SessionStatus::Idle;
        assert!(session.can_choose_model(ProviderKind::Codex));
    }

    #[test]
    fn provider_ids_are_stable() {
        assert_eq!(ProviderKind::Claude.id(), "claude");
        assert_eq!(ProviderKind::Codex.command(), "codex");
        assert_eq!(ProviderKind::Pi.command(), "pi");
    }

    #[test]
    fn native_conversation_actions_include_every_provider() {
        for provider in [ProviderKind::Claude, ProviderKind::Codex, ProviderKind::Pi] {
            assert!(provider.capabilities().conversation.fork);
            assert!(provider.capabilities().conversation.rollback);
        }
    }

    #[test]
    fn only_dynamic_provider_catalogs_are_discovered() {
        assert!(ProviderKind::Claude.capabilities().model_discovery);
        assert!(ProviderKind::Codex.capabilities().model_discovery);
        assert!(ProviderKind::Pi.capabilities().model_discovery);
    }

    #[test]
    fn all_contains_every_provider_kind() {
        assert_eq!(ProviderKind::ALL.len(), 3);
        let ids: std::collections::HashSet<_> =
            ProviderKind::ALL.iter().map(|kind| kind.id()).collect();
        assert_eq!(
            ids.len(),
            ProviderKind::ALL.len(),
            "duplicate ProviderKind::id()"
        );
        let commands: std::collections::HashSet<_> = ProviderKind::ALL
            .iter()
            .map(|kind| kind.command())
            .collect();
        assert_eq!(
            commands.len(),
            ProviderKind::ALL.len(),
            "duplicate ProviderKind::command()"
        );
    }

    #[test]
    fn prompt_title_truncation_is_unicode_safe() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Claude);
        let prompt = "界".repeat(70);
        session.set_title_from_prompt(&prompt);
        let title = session.auto_title.as_deref().unwrap();
        assert_eq!(title.chars().count(), 54);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn a_failed_preparation_unwinds_the_turn_it_eagerly_began() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);

        // A first prompt: the unwind restores the default title because the
        // prompt returns to the composer, but keeps the submission activity.
        session.set_title_from_prompt("Build the thing");
        let turn_id = session.begin_turn("Build the thing");
        let submitted_at = session.last_reply_at;
        session.unwind_unstarted_turn(turn_id);
        assert!(session.turns.is_empty());
        assert!(session.messages.is_empty());
        assert_eq!(session.last_reply_at, submitted_at);
        assert_eq!(session.title, AgentSession::DEFAULT_TITLE);
        assert!(session.auto_title.is_none());

        // A follow-up prompt unwinds only itself.
        let first = session.begin_turn("first");
        session.push_message(MessageRole::Assistant, "done");
        session.finish_active_turn(TurnStatus::Completed);
        session.set_title_from_prompt("first");
        let follow_up = session.begin_turn("second");
        let follow_up_submitted_at = session.last_reply_at;
        session.unwind_unstarted_turn(follow_up);
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].id, first);
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.last_reply_at, follow_up_submitted_at);

        // A turn the provider has already started never unwinds — losing a
        // live conversation turn would desync the provider transcript.
        let started = session.begin_turn("third");
        session.mark_active_turn_provider_started();
        session.unwind_unstarted_turn(started);
        assert_eq!(session.turns.len(), 2);
        assert_eq!(session.messages.len(), 3);
    }

    #[test]
    fn turn_truncation_removes_owned_messages_and_blocks() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);

        let first_turn = session.begin_turn("first");
        session.push_message(MessageRole::Assistant, "first answer");
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 2,
            turn_id: Some(first_turn),
            activities: Vec::new(),
        });
        session.finish_active_turn(TurnStatus::Completed);

        let second_turn = session.begin_turn("second");
        session.push_message(MessageRole::Assistant, "second answer");
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 4,
            turn_id: Some(second_turn),
            activities: Vec::new(),
        });
        session.finish_active_turn(TurnStatus::Completed);

        session.truncate_after_turn(1);

        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].id, first_turn);
        assert_eq!(session.messages.len(), 2);
        assert!(
            session
                .messages
                .iter()
                .all(|message| message.turn_id == Some(first_turn))
        );
        assert_eq!(session.transcript_blocks.len(), 1);
        assert_eq!(session.transcript_blocks[0].turn_id, Some(first_turn));
        assert_eq!(session.transcript_blocks[0].after_message, 2);
    }

    #[test]
    fn response_fork_is_a_distinct_idle_session_through_the_selected_turn() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);

        let first_turn = session.begin_turn("first");
        let first_message = session.push_message(MessageRole::Assistant, "first answer");
        session.finish_active_turn(TurnStatus::Completed);
        session.begin_turn("second");
        session.push_message(MessageRole::Assistant, "second answer");
        session.finish_active_turn(TurnStatus::Completed);

        let fork = session
            .fork_through_turn(
                1,
                ProviderResumeCursor::Codex {
                    thread_id: "forked-thread".into(),
                },
                "New task (2)",
            )
            .unwrap();

        assert_ne!(fork.id, session.id);
        assert_eq!(fork.title, AgentSession::DEFAULT_TITLE);
        assert_eq!(fork.auto_title.as_deref(), Some("New task (2)"));
        assert_eq!(fork.status, SessionStatus::Idle);
        assert_eq!(fork.turns.len(), 1);
        assert_eq!(fork.messages.len(), 2);
        assert_ne!(fork.turns[0].id, first_turn);
        assert_ne!(fork.messages[1].id, first_message);
        assert!(
            fork.messages
                .iter()
                .all(|message| message.turn_id == Some(fork.turns[0].id))
        );
        assert!(matches!(
            fork.provider_cursor,
            Some(ProviderResumeCursor::Codex { ref thread_id }) if thread_id == "forked-thread"
        ));
    }

    #[test]
    fn queued_follow_ups_stay_with_the_source_session_not_the_fork() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);

        session.begin_turn("first");
        session.push_message(MessageRole::Assistant, "first answer");
        session.finish_active_turn(TurnStatus::Completed);
        session
            .queued_messages
            .push(QueuedMessage::new("after you finish, also…"));

        let fork = session
            .fork_through_turn(
                1,
                ProviderResumeCursor::Codex {
                    thread_id: "forked-thread".into(),
                },
                "New task (2)",
            )
            .unwrap();

        assert_eq!(session.queued_messages.len(), 1);
        assert!(fork.queued_messages.is_empty());
    }

    #[test]
    fn follow_up_queue_round_trips_through_serde() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session
            .queued_messages
            .push(QueuedMessage::new("first follow-up"));
        session
            .queued_messages
            .push(QueuedMessage::new("second follow-up"));

        let value = serde_json::to_value(&session).unwrap();
        assert!(value["queued_messages"].is_array());
        let restored: AgentSession = serde_json::from_value(value).unwrap();
        assert_eq!(restored.queued_messages.len(), 2);
        assert_eq!(restored.queued_messages[0].content, "first follow-up");
        assert_eq!(restored.queued_messages[1].content, "second follow-up");
        assert_ne!(
            restored.queued_messages[0].id,
            restored.queued_messages[1].id
        );

        // Sessions without the field (older state files) deserialize as empty.
        let mut legacy = serde_json::to_value(&session).unwrap();
        legacy.as_object_mut().unwrap().remove("queued_messages");
        let legacy_session: AgentSession = serde_json::from_value(legacy).unwrap();
        assert!(legacy_session.queued_messages.is_empty());
    }

    #[test]
    fn planned_worktree_base_branch_is_optional_and_round_trips() {
        let legacy: SessionWorkspace =
            serde_json::from_value(serde_json::json!({ "kind": "newWorktree" })).unwrap();
        assert_eq!(legacy, SessionWorkspace::NewWorktree { base_branch: None });

        let selected = SessionWorkspace::NewWorktree {
            base_branch: Some("release/next".into()),
        };
        let restored = serde_json::from_value(serde_json::to_value(&selected).unwrap()).unwrap();
        assert_eq!(selected, restored);
    }

    #[test]
    fn busy_statuses_cover_connecting_working_and_waiting() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        for status in [
            SessionStatus::Connecting,
            SessionStatus::Working,
            SessionStatus::Waiting,
        ] {
            session.status = status;
            assert!(session.is_busy());
        }
        for status in [SessionStatus::Idle, SessionStatus::Failed] {
            session.status = status;
            assert!(!session.is_busy());
        }
    }

    #[test]
    fn provider_resume_cursor_is_explicitly_tagged() {
        let cursor = ProviderResumeCursor::Claude {
            session_id: "session-1".into(),
            resume_at: Some("message-9".into()),
        };
        let value = serde_json::to_value(&cursor).unwrap();
        assert_eq!(value["provider"], "claude");
        assert_eq!(value["sessionId"], "session-1");
        assert_eq!(value["resumeAt"], "message-9");
    }

    #[test]
    fn native_rollback_count_ignores_turns_that_never_reached_the_provider() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);

        session.begin_turn("first");
        session.mark_active_turn_provider_started();
        session.finish_active_turn(TurnStatus::Completed);
        session.begin_turn("failed locally");
        session.finish_active_turn(TurnStatus::Failed);
        session.begin_turn("third");
        session.mark_active_turn_provider_started();
        session.finish_active_turn(TurnStatus::Completed);

        assert_eq!(session.provider_turns_after(1), 1);
        assert_eq!(session.provider_turns_after(2), 1);
        assert_eq!(session.provider_turns_after(3), 0);
    }

    #[test]
    fn legacy_empty_search_titles_are_repaired() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 0,
            turn_id: None,
            activities: vec![ActivityItem::new(
                Some("search-1".into()),
                ActivityKind::Search,
                "Search for ",
                None,
                true,
            )],
        });

        session.migrate_legacy_state();

        let activities = &session.transcript_blocks[0].activities;
        assert_eq!(activities[0].title, "Browsed the web");
    }

    #[test]
    fn legacy_reasoning_blocks_deserialize_as_reasoning_activities() {
        let legacy = serde_json::json!({
            "after_message": 2,
            "turn_id": null,
            "content": {
                "kind": "reasoning",
                "data": {
                    "content": "Checking the source",
                    "started_at_ms": 1_000,
                    "finished_at_ms": 2_500
                }
            }
        });

        let block: TranscriptBlock = serde_json::from_value(legacy).unwrap();
        assert_eq!(block.activities.len(), 1);
        let activity = &block.activities[0];
        assert_eq!(activity.kind, ActivityKind::Reasoning);
        assert!(activity.complete);
        assert_eq!(
            activity
                .reasoning
                .as_ref()
                .map(|reasoning| reasoning.content.as_str()),
            Some("Checking the source")
        );

        let stored = serde_json::to_value(block).unwrap();
        assert_eq!(stored["content"]["kind"], "activities");
        assert_eq!(
            stored["content"]["data"][0]["reasoning"]["content"],
            "Checking the source"
        );
    }

    #[test]
    fn adjacent_legacy_work_blocks_merge_during_session_migration() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.transcript_blocks.extend([
            TranscriptBlock {
                after_message: 1,
                turn_id: None,
                activities: vec![ActivityItem::from_reasoning(
                    ReasoningBlock {
                        content: "Looking around".into(),
                        started_at_ms: 1_000,
                        finished_at_ms: 2_000,
                    },
                    true,
                )],
            },
            TranscriptBlock {
                after_message: 1,
                turn_id: None,
                activities: vec![ActivityItem::new(
                    None,
                    ActivityKind::Command,
                    "Ran tests",
                    None,
                    true,
                )],
            },
        ]);

        session.migrate_legacy_state();

        assert_eq!(session.transcript_blocks.len(), 1);
        assert_eq!(session.transcript_blocks[0].activities.len(), 2);
        assert_eq!(
            session.transcript_blocks[0]
                .activities
                .iter()
                .map(|activity| activity.kind)
                .collect::<Vec<_>>(),
            [ActivityKind::Reasoning, ActivityKind::Command]
        );
    }

    #[test]
    fn legacy_file_edit_details_are_promoted_to_arguments_and_metadata() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 0,
            turn_id: None,
            activities: vec![ActivityItem::new(
                None,
                ActivityKind::FileChange,
                "edit",
                Some(
                    serde_json::json!({
                        "filePath": "/tmp/waku/README.md",
                        "oldString": "old",
                        "newString": "new\nmore"
                    })
                    .to_string(),
                ),
                true,
            )],
        });

        session.migrate_legacy_state();

        let activities = &session.transcript_blocks[0].activities;
        assert!(activities[0].detail.is_none());
        assert!(activities[0].arguments.is_some());
        assert_eq!(activities[0].file_changes[0].path, "/tmp/waku/README.md");
        assert_eq!(activities[0].file_changes[0].additions, Some(2));
        assert_eq!(activities[0].file_changes[0].deletions, Some(1));
    }

    #[test]
    fn legacy_file_tools_are_reclassified_and_gain_cached_targets() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        let mut cached = ActivityItem::new(None, ActivityKind::FileRead, "read", None, true);
        cached.display_target = Some("/tmp/waku/src/persisted.rs".into());
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 0,
            turn_id: None,
            activities: vec![
                ActivityItem::new(
                    None,
                    ActivityKind::Search,
                    "read",
                    Some(r#"{"filePath":"/tmp/waku/src/model.rs"}"#.into()),
                    true,
                ),
                ActivityItem::new(
                    None,
                    ActivityKind::Tool,
                    "glob",
                    Some(r#"{"pattern":"src/**/*.rs"}"#.into()),
                    true,
                ),
                cached,
            ],
        });

        session.migrate_legacy_state();

        let activities = &session.transcript_blocks[0].activities;
        assert_eq!(activities[0].kind, ActivityKind::FileRead);
        assert_eq!(
            activities[0].display_target.as_deref(),
            Some("/tmp/waku/src/model.rs")
        );
        assert_eq!(activities[1].kind, ActivityKind::FileSearch);
        assert_eq!(activities[1].display_target.as_deref(), Some("src/**/*.rs"));
        assert_eq!(
            activities[2].display_target.as_deref(),
            Some("/tmp/waku/src/persisted.rs")
        );
        assert!(
            activities[..2]
                .iter()
                .all(|activity| activity.detail.is_none())
        );
        assert!(
            activities[..2]
                .iter()
                .all(|activity| activity.arguments.is_some())
        );
    }

    #[test]
    fn legacy_codex_citation_markers_are_removed() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.messages.push(Message::new(
            MessageRole::Assistant,
            "Claim.\u{e200}cite\u{e202}turn3view0\u{e202}turn2view2\u{e201}\nNext.",
        ));

        session.migrate_legacy_state();

        assert_eq!(session.messages[0].content, "Claim.\nNext.");
    }

    #[test]
    fn legacy_checkpoint_totals_are_backfilled_from_the_file_summary() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.begin_turn("Build it");
        session.finish_active_turn(TurnStatus::Completed);
        let mut serialized = serde_json::to_value(Checkpoint {
            turn_count: 1,
            git_ref: "refs/waku/test".into(),
            status: CheckpointStatus::Ready,
            files: vec![
                CheckpointFile {
                    path: "src/app.rs".into(),
                    additions: 7,
                    deletions: 2,
                },
                CheckpointFile {
                    path: "src/model.rs".into(),
                    additions: 3,
                    deletions: 5,
                },
            ],
            additions: 10,
            deletions: 7,
            created_at: 1,
        })
        .unwrap();
        let object = serialized.as_object_mut().unwrap();
        object.remove("additions");
        object.remove("deletions");
        let checkpoint: Checkpoint = serde_json::from_value(serialized).unwrap();
        assert_eq!((checkpoint.additions, checkpoint.deletions), (0, 0));
        session.turns[0].checkpoint = Some(checkpoint);

        session.migrate_legacy_state();

        let checkpoint = session.turns[0].checkpoint.as_ref().unwrap();
        assert_eq!((checkpoint.additions, checkpoint.deletions), (10, 7));
        assert!(checkpoint.totals_are_current());
    }

    #[test]
    fn turn_model_metadata_survives_model_changes_and_persistence() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        session.model = Some("claude-opus-5".into());
        session.reasoning_effort = Some("high".into());
        session.begin_turn("first");
        session.finish_active_turn(TurnStatus::Completed);

        session.model = Some("claude-sonnet-5".into());
        session.reasoning_effort = Some("medium".into());
        session.begin_provider_turn();
        session.finish_active_turn(TurnStatus::Completed);
        session.begin_turn("third");
        session.model = None;
        session.reasoning_effort = None;

        let saved = serde_json::to_value(&session).unwrap();
        let loaded: AgentSession = serde_json::from_value(saved).unwrap();
        assert_eq!(loaded.turns[0].model.as_deref(), Some("claude-opus-5"));
        assert_eq!(loaded.turns[0].reasoning_effort.as_deref(), Some("high"));
        for turn in &loaded.turns[1..] {
            assert_eq!(turn.model.as_deref(), Some("claude-sonnet-5"));
            assert_eq!(turn.reasoning_effort.as_deref(), Some("medium"));
        }
    }

    #[test]
    fn legacy_turns_do_not_inherit_the_current_model() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        session.model = Some("gpt-5.5".into());
        session.reasoning_effort = Some("high".into());
        session.begin_turn("first");
        session.finish_active_turn(TurnStatus::Completed);
        let mut saved = serde_json::to_value(&session).unwrap();
        let turn = saved["turns"][0].as_object_mut().unwrap();
        turn.remove("model");
        turn.remove("reasoning_effort");
        let mut loaded: AgentSession = serde_json::from_value(saved).unwrap();
        loaded.migrate_legacy_state();

        assert!(loaded.turns[0].model.is_none());
        assert!(loaded.turns[0].reasoning_effort.is_none());

        // Reconstructing turns for older message-only sessions is equally
        // conservative: today's selection says nothing about those replies.
        loaded.turns.clear();
        loaded.migrate_legacy_state();
        assert!(loaded.turns[0].model.is_none());
        assert!(loaded.turns[0].reasoning_effort.is_none());
    }

    #[test]
    fn list_projection_never_copies_session_detail() {
        let project = Project::from_path(PathBuf::from("/tmp/waku"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.title = "Visible title".into();
        session.model = Some("gpt-5".into());
        session.status = SessionStatus::Working;
        session.active_compaction = Some("pending-compaction".into());
        session.begin_turn("A large prompt");
        session.messages.push(Message::new(
            MessageRole::Assistant,
            "A large streamed response",
        ));
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 1,
            turn_id: None,
            activities: vec![ActivityItem::new(
                None,
                ActivityKind::Tool,
                "Inspect files",
                None,
                false,
            )],
        });
        session
            .queued_messages
            .push(QueuedMessage::new("Follow up"));

        let projection = session.list_projection();

        assert_eq!(projection.id, session.id);
        assert_eq!(projection.title, "Visible title");
        assert_eq!(projection.model.as_deref(), Some("gpt-5"));
        assert_eq!(projection.status, SessionStatus::Working);
        assert_eq!(
            projection.active_compaction.as_deref(),
            Some("pending-compaction")
        );
        assert!(!projection.detail_loaded);
        assert!(projection.messages.is_empty());
        assert!(projection.transcript_blocks.is_empty());
        assert!(projection.turns.is_empty());
        assert!(projection.queued_messages.is_empty());
    }

    #[test]
    fn forced_cleanup_interrupts_compaction_even_if_the_activity_was_marked_complete() {
        use crate::compaction::{CompactionStatus, ContextCompaction};
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        session.active_compaction = Some("pending".into());
        let mut activity =
            ActivityItem::from_compaction(ContextCompaction::new("pending", None, 1));
        activity.complete = true; // An older cleanup may already have run.
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 0,
            turn_id: None,
            activities: vec![activity],
        });
        assert!(session.is_busy());
        session.interrupt_compactions(5);
        session.interrupt_compactions(10);
        assert!(!session.is_busy());
        let activity = &session.transcript_blocks[0].activities[0];
        let item = activity.compaction.as_ref().unwrap();
        assert_eq!(item.status, CompactionStatus::Interrupted);
        assert_eq!(item.completed_at_ms, Some(5));
        assert!(activity.complete);
        assert!(!activity.failed);
    }

    #[test]
    fn xdev_virtual_tool_device_calls_reclassify_as_tools_without_file_diffs() {
        let mut activity = ActivityItem::new(
            Some("tool_xdev_probe".into()),
            ActivityKind::FileChange,
            "Write file",
            None,
            true,
        )
        .with_arguments(Some(
            r#"{"content":"{}","i":"Reading delegate_batch schema","path":"xd://delegate_batch"}"#
                .into(),
        ))
        .with_tool_name(Some("write"));

        assert_eq!(activity.kind, ActivityKind::Tool);
        assert_eq!(activity.title, "delegate_batch");
        assert_eq!(activity.tool_name.as_deref(), Some("delegate_batch"));
        assert!(activity.file_changes.is_empty());
        assert_eq!(activity.display_target, None);
        assert_eq!(activity.arguments.as_deref(), Some("{}"));

        // Simulate a legacy persisted row that stored `file_changes` and
        // `display_target` for `xd://delegate_batch`.
        activity.kind = ActivityKind::FileChange;
        activity.title = "Write file".into();
        activity.tool_name = Some("write".into());
        activity.arguments = Some(
            r#"{"content":"{\"request_key\":\"books\",\"tasks\":[]}","path":"xd://delegate_batch"}"#
                .into(),
        );
        activity.file_changes = vec![ActivityFileChange {
            path: "xd://delegate_batch".into(),
            additions: Some(1),
            deletions: Some(0),
            status: None,
            diff: Some("@@ -0,0 +1,1 @@\n+{}\n".into()),
        }];
        activity.display_target = Some("xd://delegate_batch".into());
        activity.refresh_activity_metadata();

        assert_eq!(activity.kind, ActivityKind::Tool);
        assert_eq!(activity.title, "delegate_batch");
        assert_eq!(activity.tool_name.as_deref(), Some("delegate_batch"));
        assert!(activity.file_changes.is_empty());
        assert_eq!(activity.display_target, None);
    }
}

#[cfg(test)]
mod review_subtask_tests {
    use super::ProviderKind;

    #[test]
    fn only_providers_with_a_read_only_posture_can_be_delegated_to() {
        for provider in [
            ProviderKind::Codex,
            ProviderKind::Claude,
            // Pi launches subtasks with a read-only tool allowlist.
            ProviderKind::Pi,
        ] {
            assert!(
                provider.capabilities().delegation.review_subtask,
                "{} has a verified read-only path",
                provider.display_name()
            );
        }
    }
}
