//! Desktop-owned preferences and RPC proxies for daemon-owned task state.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Command, DaemonSettings, DaemonSupervisor, ResponsePayload};
use raku_protocol::i18n::AppLanguage;
use raku_protocol::identity::DATA_DIRECTORY_NAME;
use raku_protocol::model::{
    AgentSession, FavoriteModel, Project, ProviderKind, ProviderResumeCursor,
    ProviderSessionHistory, ProviderSessionSummary, RuntimeMode, SubagentDefinition,
};
use raku_protocol::persistence::{from_persisted_json, migrate_retired_provider_references};
use raku_protocol::theme::ThemePreference;

pub use raku_protocol::persistence::{
    ComposerDraft, ComposerDraftAttachment, ComposerDraftChange, ComposerDraftKey,
    ComposerDraftTarget, ComposerDrafts, SessionMessageMatch,
};

const STATE_VERSION: u32 = 5;
const APP_STATE_VERSION: u32 = 1;

pub const DEFAULT_SIDEBAR_WIDTH: f32 = 252.0;
pub const DEFAULT_RIGHT_PANEL_WIDTH: f32 = 460.0;

/// How the desktop groups task history in the sidebar.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarGrouping {
    Project,
    #[default]
    Updated,
}

/// Direction of task history inside the sidebar's current grouping.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarOrdering {
    #[default]
    Newest,
    Oldest,
}

fn default_sidebar_visibility() -> bool {
    true
}

fn default_right_panel_visibility() -> bool {
    false
}

fn default_code_font_size() -> f32 {
    DEFAULT_CODE_FONT_SIZE
}

fn default_render_math() -> bool {
    true
}

fn default_analytics_enabled() -> bool {
    false
}

fn default_provider() -> ProviderKind {
    ProviderKind::Codex
}

fn default_sidebar_width() -> f32 {
    DEFAULT_SIDEBAR_WIDTH
}

fn default_right_panel_width() -> f32 {
    DEFAULT_RIGHT_PANEL_WIDTH
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RememberedModelTraits {
    provider: ProviderKind,
    model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_window: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ProjectModelSelection {
    provider: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_window: Option<String>,
}

impl ProjectModelSelection {
    fn for_provider(provider: ProviderKind) -> Self {
        Self {
            provider,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
        }
    }

    fn for_session(session: &AgentSession) -> Self {
        Self {
            provider: session.provider,
            model: session.model.clone(),
            reasoning_effort: session.reasoning_effort.clone(),
            service_tier: session.service_tier.clone(),
            context_window: session.context_window.clone(),
        }
    }

    fn apply_to(&self, session: &mut AgentSession) {
        if session.provider != self.provider {
            session.agent_preset = None;
        }
        session.provider = self.provider;
        session.model.clone_from(&self.model);
        session.reasoning_effort.clone_from(&self.reasoning_effort);
        session.service_tier.clone_from(&self.service_tier);
        session.context_window.clone_from(&self.context_window);
    }
}

/// Remote composer-draft proxy. Draft bytes and attachments remain owned by
/// the daemon even though the desktop keeps an in-memory editing snapshot.
#[derive(Clone)]
pub struct ComposerDraftStore {
    daemon: DaemonSupervisor,
    save_state: Arc<Mutex<ComposerDraftSaveState>>,
}

#[derive(Default)]
struct ComposerDraftSaveState {
    snapshot: ComposerDrafts,
    latest_generation: u64,
}

impl ComposerDraftStore {
    pub fn remote(daemon: DaemonSupervisor) -> Self {
        Self {
            daemon,
            save_state: Arc::new(Mutex::new(ComposerDraftSaveState::default())),
        }
    }

    pub fn load(&self) -> io::Result<ComposerDrafts> {
        match self
            .daemon
            .client()
            .request(Uuid::nil(), Uuid::nil(), Command::LoadComposerDrafts)
            .map_err(to_io_error)?
        {
            ResponsePayload::ComposerDrafts { drafts } => {
                self.save_state.lock().snapshot = drafts.clone();
                Ok(drafts)
            }
            _ => Err(io::Error::other(
                "Raku daemon returned an invalid composer-drafts response",
            )),
        }
    }

    pub fn save(&self, drafts: ComposerDrafts, generation: u64) -> io::Result<()> {
        // Serialize this client's detached background saves and compare only
        // against its last local snapshot. A second client may add or remove
        // other keys without those changes being interpreted as ours.
        let mut state = self.save_state.lock();
        if generation < state.latest_generation {
            return Ok(());
        }
        let changes = composer_draft_changes(&state.snapshot, &drafts);
        if changes.is_empty() {
            state.latest_generation = generation;
            return Ok(());
        }
        match self
            .daemon
            .client()
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::ApplyComposerDraftChanges { changes },
            )
            .map_err(to_io_error)?
        {
            ResponsePayload::Ack => {
                state.snapshot = drafts;
                state.latest_generation = generation;
                Ok(())
            }
            _ => Err(io::Error::other(
                "Raku daemon returned an invalid composer-drafts save response",
            )),
        }
    }
}

fn composer_draft_changes(
    previous: &ComposerDrafts,
    next: &ComposerDrafts,
) -> Vec<ComposerDraftChange> {
    let mut changes = Vec::new();
    collect_composer_draft_changes(
        &previous.new_sessions,
        &next.new_sessions,
        |project_id| ComposerDraftTarget::NewSession { project_id },
        &mut changes,
    );
    collect_composer_draft_changes(
        &previous.sessions,
        &next.sessions,
        |session_id| ComposerDraftTarget::Session { session_id },
        &mut changes,
    );
    changes
}

fn collect_composer_draft_changes(
    previous: &HashMap<Uuid, ComposerDraft>,
    next: &HashMap<Uuid, ComposerDraft>,
    target: impl Fn(Uuid) -> ComposerDraftTarget,
    changes: &mut Vec<ComposerDraftChange>,
) {
    for (id, draft) in next {
        if previous.get(id) != Some(draft) {
            changes.push(ComposerDraftChange {
                target: target(*id),
                draft: Some(draft.clone()),
            });
        }
    }
    for id in previous.keys() {
        if !next.contains_key(id) {
            changes.push(ComposerDraftChange {
                target: target(*id),
                draft: None,
            });
        }
    }
}

/// Last observed main-window frame in logical pixels. GPUI window bounds are
/// relative to the display the window sits on, so the frame only means
/// something together with `display` — the stable display UUID (Zed persists
/// the same pair; display *ids* renumber across reboots and replugs). While
/// the window is maximized or fullscreen these bounds keep the floating frame
/// the window returns to, not the screen-filling one.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct PersistedWindowState {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AppSettings {
    pub analytics_enabled: bool,
    pub favorite_models: Vec<FavoriteModel>,
    pub model_picker_favorites_only: bool,
    pub theme: ThemePreference,
    pub language: AppLanguage,
    /// An unset family follows the platform's system font.
    pub interface_font: Option<String>,
    pub transcript_font: Option<String>,
    /// Family that supplies glyphs the interface and transcript fonts lack.
    /// Unset follows the interface language.
    pub fallback_font: Option<String>,
    /// Text size for transcript prose. Chrome stays at the fixed
    /// [`DEFAULT_UI_FONT_SIZE`] it is authored against.
    #[serde(deserialize_with = "lenient_transcript_font_size")]
    pub transcript_font_size: TranscriptFontSize,
    /// Text size for code surfaces — the file editor, diffs, code blocks,
    /// and tool output — in pixels. Hand-edited values are clamped when
    /// applied.
    pub code_font_size: f32,
    pub render_math: bool,
    pub backup_directory: Option<PathBuf>,
    /// Provider and model for read-only side chats. Pi defaults to a
    /// discovered DeepSeek model; an unset legacy provider is treated as Pi.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side_chat_provider: Option<ProviderKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side_chat_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side_chat_reasoning_effort: Option<String>,
    /// Provider and model that write short labels, such as reasoning
    /// headlines. No provider turns generation off; that is the default,
    /// because every label is a model request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_generation_provider: Option<ProviderKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_generation_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_generation_reasoning_effort: Option<String>,
    /// Agents a task's agent may delegate to, in the order Settings lists
    /// them. The built-ins are always present; see
    /// [`normalize_subagents`].
    pub subagents: Vec<SubagentDefinition>,
}

/// Brings a stored agent list back to a usable shape: the built-ins exist,
/// names are valid and unique, and only the built-ins claim to be built in.
pub fn normalize_subagents(agents: &mut Vec<SubagentDefinition>) {
    let builtins = SubagentDefinition::builtins();
    let mut seen = HashSet::new();
    agents.retain_mut(|agent| {
        agent.builtin = builtins.iter().any(|builtin| builtin.name == agent.name);
        SubagentDefinition::is_valid_name(&agent.name) && seen.insert(agent.name.clone())
    });
    for (position, builtin) in builtins.into_iter().enumerate() {
        if !seen.contains(&builtin.name) {
            agents.insert(position.min(agents.len()), builtin);
        }
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            analytics_enabled: default_analytics_enabled(),
            favorite_models: Vec::new(),
            model_picker_favorites_only: false,
            theme: ThemePreference::System,
            language: AppLanguage::default(),
            interface_font: None,
            transcript_font: None,
            fallback_font: None,
            transcript_font_size: TranscriptFontSize::default(),
            code_font_size: DEFAULT_CODE_FONT_SIZE,
            render_math: true,
            backup_directory: None,
            side_chat_provider: Some(ProviderKind::Pi),
            side_chat_model: None,
            side_chat_reasoning_effort: Some("low".into()),
            text_generation_provider: None,
            text_generation_model: None,
            text_generation_reasoning_effort: None,
            subagents: SubagentDefinition::builtins(),
        }
    }
}

impl AppSettings {
    /// Retired desktop preferences must not revive from an older settings file.
    pub fn normalize_desktop_defaults(&mut self) {
        self.analytics_enabled = false;
        self.render_math = true;
        self.side_chat_provider.get_or_insert(ProviderKind::Pi);
        self.side_chat_reasoning_effort
            .get_or_insert_with(|| "low".into());
        normalize_subagents(&mut self.subagents);
    }
}

/// The size chrome is authored against. Fixed: the window's rem size, which
/// every `sp` dimension resolves through.
pub const DEFAULT_UI_FONT_SIZE: f32 = 14.0;
pub const DEFAULT_CODE_FONT_SIZE: f32 = 13.0;

/// Transcript prose size, one of three steps rather than a free pixel value.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptFontSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl TranscriptFontSize {
    pub const ALL: [Self; 3] = [Self::Small, Self::Medium, Self::Large];

    pub fn px(self) -> f32 {
        match self {
            Self::Small => 15.0,
            Self::Medium => 16.0,
            Self::Large => 17.0,
        }
    }
}

/// A missing, hand-edited, or unknown size falls back to the default instead
/// of rejecting the whole settings document.
fn lenient_transcript_font_size<'de, D>(deserializer: D) -> Result<TranscriptFontSize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// Bounds a possibly hand-edited font size to something the layout survives.
fn sanitized_font_size(size: f32, fallback: f32) -> f32 {
    if size.is_finite() {
        size.clamp(9.0, 24.0)
    } else {
        fallback
    }
}

pub fn sanitized_code_font_size(size: f32) -> f32 {
    sanitized_font_size(size, DEFAULT_CODE_FONT_SIZE)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AppState {
    app_state_version: u32,
    #[serde(default = "Uuid::new_v4")]
    analytics_id: Uuid,
    #[serde(default)]
    selected_project: Option<Uuid>,
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    removed_project_ids: HashSet<Uuid>,
    #[serde(default)]
    selected_session: Option<Uuid>,
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pinned_session_ids: HashSet<Uuid>,
    #[serde(default = "default_provider")]
    last_provider: ProviderKind,
    #[serde(default)]
    last_runtime_mode: RuntimeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_context_window: Option<String>,
    /// The nil UUID groups all daemon-created No Project workspaces together.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    project_model_selections: HashMap<Uuid, ProjectModelSelection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    remembered_model_traits: Vec<RememberedModelTraits>,
    #[serde(default = "default_sidebar_visibility")]
    sidebar_visible: bool,
    #[serde(default = "default_right_panel_visibility")]
    right_panel_visible: bool,
    #[serde(default = "default_sidebar_width")]
    sidebar_width: f32,
    #[serde(default)]
    sidebar_grouping: SidebarGrouping,
    #[serde(default)]
    sidebar_ordering: SidebarOrdering,
    #[serde(default = "default_right_panel_width")]
    right_panel_width: f32,
    /// Whether markdown files in the right panel open as a rendered preview
    /// instead of source. One global mode, not per file.
    #[serde(default)]
    markdown_preview: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window_state: Option<PersistedWindowState>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PersistedState {
    pub version: u32,
    #[serde(default = "Uuid::new_v4")]
    pub analytics_id: Uuid,
    #[serde(default = "default_analytics_enabled")]
    pub analytics_enabled: bool,
    pub projects: Vec<Project>,
    pub sessions: Vec<AgentSession>,
    pub selected_project: Option<Uuid>,
    /// Projects removed from this client's picker. Keep their daemon-owned
    /// task history so reopening the same folder restores its conversations.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub removed_project_ids: HashSet<Uuid>,
    pub selected_session: Option<Uuid>,
    /// Desktop sidebar pins. Kept separately from daemon-owned transcripts so
    /// pinning a session never needs to hydrate or rewrite its history.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub pinned_session_ids: HashSet<Uuid>,
    pub last_provider: ProviderKind,
    #[serde(default)]
    pub last_runtime_mode: RuntimeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_context_window: Option<String>,
    /// Legacy global selection is only a fallback for projects without history.
    /// Explicit selections update this map instead of the `last_*` fields.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    project_model_selections: HashMap<Uuid, ProjectModelSelection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remembered_model_traits: Vec<RememberedModelTraits>,
    #[serde(default)]
    pub favorite_models: Vec<FavoriteModel>,
    #[serde(default)]
    pub model_picker_favorites_only: bool,
    #[serde(default)]
    pub theme: ThemePreference,
    #[serde(default)]
    pub language: AppLanguage,
    #[serde(default)]
    pub interface_font: Option<String>,
    #[serde(default)]
    pub transcript_font: Option<String>,
    #[serde(default)]
    pub fallback_font: Option<String>,
    #[serde(default, deserialize_with = "lenient_transcript_font_size")]
    pub transcript_font_size: TranscriptFontSize,
    #[serde(default = "default_code_font_size")]
    pub code_font_size: f32,
    #[serde(default = "default_render_math")]
    pub render_math: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_chat_provider: Option<ProviderKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_chat_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_chat_reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_generation_provider: Option<ProviderKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_generation_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_generation_reasoning_effort: Option<String>,
    #[serde(default = "SubagentDefinition::builtins")]
    pub subagents: Vec<SubagentDefinition>,
    #[serde(default = "default_sidebar_visibility")]
    pub sidebar_visible: bool,
    #[serde(default = "default_right_panel_visibility")]
    pub right_panel_visible: bool,
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    #[serde(default)]
    pub sidebar_grouping: SidebarGrouping,
    #[serde(default)]
    pub sidebar_ordering: SidebarOrdering,
    #[serde(default = "default_right_panel_width")]
    pub right_panel_width: f32,
    /// Whether markdown files in the right panel open as a rendered preview
    /// instead of source. One global mode, not per file.
    #[serde(default)]
    pub markdown_preview: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_state: Option<PersistedWindowState>,
    #[serde(default)]
    pub disabled_providers: Vec<ProviderKind>,
    #[serde(default)]
    pub provider_binary_overrides: HashMap<ProviderKind, String>,
    #[serde(skip)]
    daemon_settings_extra: BTreeMap<String, serde_json::Value>,
    #[serde(skip)]
    dirty_sessions: HashSet<Uuid>,
}

impl PersistedState {
    pub fn session_mut(&mut self, id: Uuid) -> Option<&mut AgentSession> {
        let session = self.sessions.iter_mut().find(|session| session.id == id)?;
        self.dirty_sessions.insert(id);
        Some(session)
    }

    pub fn mark_session_dirty(&mut self, id: Uuid) {
        self.dirty_sessions.insert(id);
    }

    pub fn push_session(&mut self, session: AgentSession) {
        self.dirty_sessions.insert(session.id);
        self.sessions.push(session);
    }

    pub fn empty() -> Self {
        Self {
            version: STATE_VERSION,
            analytics_id: Uuid::new_v4(),
            analytics_enabled: false,
            projects: Vec::new(),
            sessions: Vec::new(),
            selected_project: None,
            removed_project_ids: HashSet::new(),
            selected_session: None,
            pinned_session_ids: HashSet::new(),
            last_provider: ProviderKind::Codex,
            last_runtime_mode: RuntimeMode::default(),
            last_model: None,
            last_reasoning_effort: None,
            last_service_tier: None,
            last_context_window: None,
            project_model_selections: HashMap::new(),
            remembered_model_traits: Vec::new(),
            favorite_models: Vec::new(),
            model_picker_favorites_only: false,
            theme: ThemePreference::System,
            language: AppLanguage::default(),
            interface_font: None,
            transcript_font: None,
            fallback_font: None,
            transcript_font_size: TranscriptFontSize::default(),
            code_font_size: DEFAULT_CODE_FONT_SIZE,
            render_math: true,
            backup_directory: None,
            side_chat_provider: Some(ProviderKind::Pi),
            side_chat_model: None,
            side_chat_reasoning_effort: Some("low".into()),
            text_generation_provider: None,
            text_generation_model: None,
            text_generation_reasoning_effort: None,
            subagents: SubagentDefinition::builtins(),
            sidebar_visible: true,
            right_panel_visible: false,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_grouping: SidebarGrouping::Updated,
            sidebar_ordering: SidebarOrdering::Newest,
            right_panel_width: DEFAULT_RIGHT_PANEL_WIDTH,
            markdown_preview: false,
            window_state: None,
            disabled_providers: Vec::new(),
            provider_binary_overrides: HashMap::new(),
            daemon_settings_extra: BTreeMap::new(),
            dirty_sessions: HashSet::new(),
        }
    }

    pub fn fresh(cwd: PathBuf) -> Self {
        let project = Project::from_path(cwd);
        let session = AgentSession::new(project.id, ProviderKind::Codex);
        Self {
            selected_project: Some(project.id),
            selected_session: Some(session.id),
            projects: vec![project],
            sessions: vec![session],
            ..Self::empty()
        }
    }

    fn model_selection_key(&self, project_id: Option<Uuid>) -> Uuid {
        project_id
            .filter(|id| {
                !self
                    .projects
                    .iter()
                    .any(|project| project.id == *id && project.is_projectless())
            })
            .unwrap_or(Uuid::nil())
    }

    pub fn new_session_provider(&self, project_id: Option<Uuid>) -> ProviderKind {
        let provider = self
            .project_model_selections
            .get(&self.model_selection_key(project_id))
            .map_or(self.last_provider, |selection| selection.provider);
        if self.disabled_providers.contains(&provider) {
            ProviderKind::ALL
                .into_iter()
                .find(|provider| !self.disabled_providers.contains(provider))
                .unwrap_or(provider)
        } else {
            provider
        }
    }

    pub fn new_session(&self, project_id: Uuid) -> AgentSession {
        let provider = self.new_session_provider(Some(project_id));
        let mut session = AgentSession::new(project_id, provider);
        session.runtime_mode = self.last_runtime_mode;
        if let Some(selection) = self
            .project_model_selections
            .get(&self.model_selection_key(Some(project_id)))
        {
            if selection.provider == provider {
                selection.apply_to(&mut session);
            }
        } else if provider == self.last_provider {
            session.model.clone_from(&self.last_model);
            session
                .reasoning_effort
                .clone_from(&self.last_reasoning_effort);
            session.service_tier.clone_from(&self.last_service_tier);
            session.context_window.clone_from(&self.last_context_window);
        }
        session
    }

    /// Only an explicit model/options edit changes the next task's selection.
    /// Visiting history must not replace a project's remembered choice.
    pub fn remember_session_model_selection(&mut self, session_id: Uuid) {
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        let key = self.model_selection_key(Some(session.project_id));
        let selection = ProjectModelSelection::for_session(session);
        self.project_model_selections.insert(key, selection);
    }

    /// New Task can reuse an unsent draft; apply preferences when it is opened,
    /// while keeping its text, workspace, access mode and identity intact.
    pub fn refresh_new_session_model_selection(&mut self, session_id: Uuid) {
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id && !session.has_started())
        else {
            return;
        };
        let selection = ProjectModelSelection::for_session(&self.new_session(session.project_id));
        if selection != ProjectModelSelection::for_session(session)
            && let Some(session) = self.session_mut(session_id)
        {
            selection.apply_to(session);
        }
    }

    pub fn replace_new_session_provider(&mut self, provider: ProviderKind, fallback: ProviderKind) {
        if self.last_provider == provider {
            self.last_provider = fallback;
            self.last_model = None;
            self.last_reasoning_effort = None;
            self.last_service_tier = None;
            self.last_context_window = None;
        }
        for selection in self.project_model_selections.values_mut() {
            if selection.provider == provider {
                *selection = ProjectModelSelection::for_provider(fallback);
            }
        }
    }

    pub fn remember_model_traits(
        &mut self,
        provider: ProviderKind,
        model: &str,
        reasoning_effort: Option<String>,
        service_tier: Option<String>,
        context_window: Option<String>,
    ) {
        let existing = self
            .remembered_model_traits
            .iter()
            .position(|traits| traits.provider == provider && traits.model == model);
        if reasoning_effort.is_none() && service_tier.is_none() && context_window.is_none() {
            if let Some(index) = existing {
                self.remembered_model_traits.remove(index);
            }
            return;
        }
        if let Some(index) = existing {
            let traits = &mut self.remembered_model_traits[index];
            traits.reasoning_effort = reasoning_effort;
            traits.service_tier = service_tier;
            traits.context_window = context_window;
        } else {
            self.remembered_model_traits.push(RememberedModelTraits {
                provider,
                model: model.to_owned(),
                reasoning_effort,
                service_tier,
                context_window,
            });
        }
    }

    pub fn model_traits_for(
        &self,
        provider: ProviderKind,
        model: &str,
    ) -> (Option<String>, Option<String>, Option<String>) {
        self.remembered_model_traits
            .iter()
            .find(|traits| traits.provider == provider && traits.model == model)
            .map(|traits| {
                (
                    traits.reasoning_effort.clone(),
                    traits.service_tier.clone(),
                    traits.context_window.clone(),
                )
            })
            .unwrap_or_default()
    }

    pub fn daemon_settings(&self) -> DaemonSettings {
        DaemonSettings {
            disabled_providers: self.disabled_providers.clone(),
            provider_binary_overrides: self.provider_binary_overrides.clone(),
            extra: self.daemon_settings_extra.clone(),
        }
    }

    pub fn apply_daemon_settings(&mut self, settings: DaemonSettings) {
        self.disabled_providers = settings.disabled_providers;
        self.provider_binary_overrides = settings.provider_binary_overrides;
        self.daemon_settings_extra = settings.extra;
    }

    pub fn desktop_backup(&self) -> io::Result<raku_protocol::backup::DesktopBackup> {
        Ok(raku_protocol::backup::DesktopBackup {
            settings: serde_json::to_value(self.app_settings()).map_err(to_io_error)?,
            state: serde_json::to_value(self.app_state()).map_err(to_io_error)?,
        })
    }

    /// A backup must wait for the user's current edits to reach SQLite,
    /// rather than race the ordinary fire-and-forget streaming save.
    fn backup_task_save(&self) -> Command {
        Command::SaveTaskState {
            projects: self.projects.clone(),
            live_session_ids: self.sessions.iter().map(|session| session.id).collect(),
            sessions: self
                .sessions
                .iter()
                .filter(|session| self.dirty_sessions.contains(&session.id))
                .cloned()
                .collect(),
        }
    }

    fn app_settings(&self) -> AppSettings {
        let mut settings = AppSettings {
            analytics_enabled: self.analytics_enabled,
            favorite_models: self.favorite_models.clone(),
            model_picker_favorites_only: self.model_picker_favorites_only,
            theme: self.theme,
            language: self.language,
            interface_font: self.interface_font.clone(),
            transcript_font: self.transcript_font.clone(),
            fallback_font: self.fallback_font.clone(),
            transcript_font_size: self.transcript_font_size,
            code_font_size: self.code_font_size,
            render_math: self.render_math,
            backup_directory: self.backup_directory.clone(),
            side_chat_provider: self.side_chat_provider,
            side_chat_model: self.side_chat_model.clone(),
            side_chat_reasoning_effort: self.side_chat_reasoning_effort.clone(),
            text_generation_provider: self.text_generation_provider,
            text_generation_model: self.text_generation_model.clone(),
            text_generation_reasoning_effort: self.text_generation_reasoning_effort.clone(),
            subagents: self.subagents.clone(),
        };
        settings.normalize_desktop_defaults();
        settings
    }

    fn app_state(&self) -> AppState {
        AppState {
            app_state_version: APP_STATE_VERSION,
            analytics_id: self.analytics_id,
            selected_project: self.selected_project,
            removed_project_ids: self.removed_project_ids.clone(),
            selected_session: self.persistable_selected_session(),
            pinned_session_ids: self.pinned_session_ids.clone(),
            last_provider: self.last_provider,
            last_runtime_mode: self.last_runtime_mode,
            last_model: self.last_model.clone(),
            last_reasoning_effort: self.last_reasoning_effort.clone(),
            last_service_tier: self.last_service_tier.clone(),
            last_context_window: self.last_context_window.clone(),
            project_model_selections: self.project_model_selections.clone(),
            remembered_model_traits: self.remembered_model_traits.clone(),
            sidebar_visible: self.sidebar_visible,
            right_panel_visible: self.right_panel_visible,
            sidebar_width: self.sidebar_width,
            sidebar_grouping: self.sidebar_grouping,
            sidebar_ordering: self.sidebar_ordering,
            right_panel_width: self.right_panel_width,
            markdown_preview: self.markdown_preview,
            window_state: self.window_state,
        }
    }

    fn apply_app_settings(&mut self, mut settings: AppSettings) {
        settings.normalize_desktop_defaults();
        self.analytics_enabled = settings.analytics_enabled;
        self.favorite_models = settings.favorite_models;
        self.model_picker_favorites_only = settings.model_picker_favorites_only;
        self.theme = settings.theme;
        self.language = settings.language;
        self.interface_font = settings.interface_font;
        self.transcript_font = settings.transcript_font;
        self.fallback_font = settings.fallback_font;
        self.transcript_font_size = settings.transcript_font_size;
        self.code_font_size = sanitized_code_font_size(settings.code_font_size);
        self.render_math = settings.render_math;
        self.backup_directory = settings.backup_directory;
        self.side_chat_provider = settings.side_chat_provider;
        self.side_chat_model = settings.side_chat_model;
        self.side_chat_reasoning_effort = settings.side_chat_reasoning_effort;
        self.text_generation_provider = settings.text_generation_provider;
        self.text_generation_model = settings.text_generation_model;
        self.text_generation_reasoning_effort = settings.text_generation_reasoning_effort;
        self.subagents = settings.subagents;
    }

    fn apply_app_state(&mut self, app_state: AppState) {
        self.analytics_id = app_state.analytics_id;
        self.selected_project = app_state.selected_project;
        self.removed_project_ids = app_state.removed_project_ids;
        self.selected_session = app_state.selected_session;
        self.pinned_session_ids = app_state.pinned_session_ids;
        self.last_provider = app_state.last_provider;
        self.last_runtime_mode = app_state.last_runtime_mode;
        self.last_model = app_state.last_model;
        self.last_reasoning_effort = app_state.last_reasoning_effort;
        self.last_service_tier = app_state.last_service_tier;
        self.last_context_window = app_state.last_context_window;
        self.project_model_selections = app_state.project_model_selections;
        self.remembered_model_traits = app_state.remembered_model_traits;
        self.sidebar_visible = app_state.sidebar_visible;
        self.right_panel_visible = app_state.right_panel_visible;
        self.sidebar_width = app_state.sidebar_width;
        self.sidebar_grouping = app_state.sidebar_grouping;
        self.sidebar_ordering = app_state.sidebar_ordering;
        self.right_panel_width = app_state.right_panel_width;
        self.markdown_preview = app_state.markdown_preview;
        self.window_state = app_state.window_state;
    }

    fn persistable_selected_session(&self) -> Option<Uuid> {
        self.selected_session.filter(|selected| {
            self.sessions
                .iter()
                .any(|session| session.id == *selected && session.has_started())
        })
    }

    fn ensure_runtime_session(&mut self) {
        if self
            .selected_session
            .is_some_and(|selected| self.sessions.iter().any(|session| session.id == selected))
        {
            return;
        }
        self.selected_session = None;
        let Some(project_id) = self
            .selected_project
            .filter(|selected| self.projects.iter().any(|project| project.id == *selected))
        else {
            return;
        };
        let session = self.new_session(project_id);
        self.selected_session = Some(session.id);
        self.sessions.push(session);
    }

    fn migrate_loaded(&mut self) {
        let session_ids = self
            .sessions
            .iter()
            .map(|session| session.id)
            .collect::<HashSet<_>>();
        self.pinned_session_ids
            .retain(|id| session_ids.contains(id));
        for session in &mut self.sessions {
            let checkpoint_totals_current = session.turns.iter().all(|turn| {
                turn.checkpoint
                    .as_ref()
                    .is_none_or(raku_protocol::model::Checkpoint::totals_are_current)
            });
            let before = (
                session.turns.len(),
                session.last_reply_at,
                session.provider_cursor.is_some(),
            );
            session.migrate_legacy_state();
            session.backfill_last_reply_at();
            if !checkpoint_totals_current
                || before
                    != (
                        session.turns.len(),
                        session.last_reply_at,
                        session.provider_cursor.is_some(),
                    )
            {
                self.dirty_sessions.insert(session.id);
            }
        }
        self.version = STATE_VERSION;
        self.backfill_project_model_selections();
    }

    fn backfill_project_model_selections(&mut self) {
        let projectless_ids = self
            .projects
            .iter()
            .filter(|project| project.is_projectless())
            .map(|project| project.id)
            .collect::<HashSet<_>>();
        let mut latest = HashMap::<Uuid, &AgentSession>::new();
        for session in &self.sessions {
            let key = if projectless_ids.contains(&session.project_id) {
                Uuid::nil()
            } else {
                session.project_id
            };
            if latest.get(&key).is_none_or(|previous| {
                (session.updated_at, session.created_at)
                    >= (previous.updated_at, previous.created_at)
            }) {
                latest.insert(key, session);
            }
        }
        for (key, session) in latest {
            if self.project_model_selections.contains_key(&key) {
                continue;
            }
            let mut selection = ProjectModelSelection::for_session(session);
            // Startup list projections omit model options. Recover the old
            // defaults without hydrating any transcripts just for migration.
            if !session.detail_loaded {
                let (reasoning_effort, service_tier, context_window) = if selection.provider
                    == self.last_provider
                    && selection.model == self.last_model
                {
                    (
                        self.last_reasoning_effort.clone(),
                        self.last_service_tier.clone(),
                        self.last_context_window.clone(),
                    )
                } else {
                    selection
                        .model
                        .as_deref()
                        .map(|model| self.model_traits_for(selection.provider, model))
                        .unwrap_or_default()
                };
                selection.reasoning_effort = reasoning_effort;
                selection.service_tier = service_tier;
                selection.context_window = context_window;
            }
            self.project_model_selections.insert(key, selection);
        }
    }
}

fn configuration_directory() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".raku")
}

fn default_app_settings_path() -> PathBuf {
    if cfg!(debug_assertions) {
        StateStore::default_path().with_file_name("app.json")
    } else {
        configuration_directory().join("app.json")
    }
}

fn default_app_state_path() -> PathBuf {
    StateStore::default_path().with_file_name("state.json")
}

/// Arm only on a new desktop launch. Rebuilding the child daemon while the
/// desktop is still running must never replace data under its live model.
pub fn prepare_pending_restore() -> io::Result<()> {
    use raku_protocol::backup::{PENDING_RESTORE, READY_RESTORE};
    let directory = StateStore::default_path().parent().unwrap().to_owned();
    let pending = directory.join(PENDING_RESTORE);
    if pending.try_exists()? {
        fs::rename(pending, directory.join(READY_RESTORE))?;
    }
    Ok(())
}

pub fn finish_pending_restore() -> io::Result<()> {
    let path = StateStore::default_path().with_file_name(raku_protocol::backup::RESTORED_DESKTOP);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    restore_desktop_documents(
        &default_app_settings_path(),
        &default_app_state_path(),
        &bytes,
    )?;
    fs::remove_file(path)
}

pub fn take_restore_error() -> io::Result<Option<String>> {
    let path = StateStore::default_path().with_file_name(raku_protocol::backup::RESTORE_ERROR);
    match fs::read(&path) {
        Ok(bytes) => {
            let error = serde_json::from_slice(&bytes).map_err(to_io_error)?;
            fs::remove_file(path)?;
            Ok(Some(error))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn restore_desktop_documents(
    settings_path: &Path,
    state_path: &Path,
    bytes: &[u8],
) -> io::Result<()> {
    let mut desktop: raku_protocol::backup::DesktopBackup =
        serde_json::from_slice(bytes).map_err(to_io_error)?;
    migrate_retired_provider_references(&mut desktop.settings);
    migrate_retired_provider_references(&mut desktop.state);
    let mut settings: AppSettings =
        serde_json::from_value(desktop.settings).map_err(to_io_error)?;
    let state: AppState = serde_json::from_value(desktop.state).map_err(to_io_error)?;
    if state.app_state_version != APP_STATE_VERSION {
        return Err(io::Error::other(
            "unsupported desktop state version in backup",
        ));
    }
    // These locate this installation and must survive moving a backup between
    // machines. Apply the same desktop defaults as a normal preferences load.
    let current: Option<AppSettings> = fs::read(settings_path)
        .ok()
        .and_then(|bytes| from_persisted_json(&bytes).ok());
    settings.backup_directory = current
        .as_ref()
        .and_then(|current| current.backup_directory.clone());
    settings.normalize_desktop_defaults();
    write_json_atomically(settings_path, &settings)?;
    write_json_atomically(state_path, &state)
}

fn read_app_state_file(path: &Path) -> Option<AppState> {
    let bytes = fs::read(path).ok()?;
    let app_state = from_persisted_json::<AppState>(&bytes).ok()?;
    (app_state.app_state_version == APP_STATE_VERSION).then_some(app_state)
}

/// Read the last persisted window frame ahead of the full state load. The
/// main window opens before the daemon connection and `StateStore` exist, and
/// `WindowOptions` needs its frame at `open_window` time.
pub fn load_window_state() -> Option<PersistedWindowState> {
    read_app_state_file(&default_app_state_path())?.window_state
}

fn default_legacy_settings_paths() -> Vec<PathBuf> {
    if cfg!(debug_assertions) {
        vec![StateStore::default_path().with_file_name("settings.json")]
    } else {
        vec![configuration_directory().join("settings.json")]
    }
}

fn read_app_settings_source(
    app_settings_path: &Path,
    legacy_settings_paths: &[PathBuf],
) -> io::Result<Option<(Vec<u8>, bool)>> {
    match fs::read(app_settings_path) {
        Ok(bytes) => return Ok(Some((bytes, true))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for path in legacy_settings_paths {
        match fs::read(path) {
            Ok(bytes) => return Ok(Some((bytes, false))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

/// Load the app-owned launch settings before the managed daemon starts.
/// Missing settings are initialized immediately so its bearer token remains
/// stable between this process launch and the later UI state load.
pub fn load_or_create_app_settings() -> io::Result<AppSettings> {
    let path = default_app_settings_path();
    let source = read_app_settings_source(&path, &default_legacy_settings_paths())?;
    let loaded_from_primary = source.as_ref().is_some_and(|(_, primary)| *primary);
    let mut settings: AppSettings = source
        .map(|(bytes, _)| from_persisted_json::<AppSettings>(&bytes).map_err(to_io_error))
        .transpose()?
        .unwrap_or_default();
    let needs_normalization = settings.analytics_enabled
        || !settings.render_math
        || settings.side_chat_provider.is_none()
        || settings.side_chat_reasoning_effort.is_none();
    settings.normalize_desktop_defaults();
    if !loaded_from_primary || needs_normalization {
        write_json_atomically(&path, &settings)?;
    }
    Ok(settings)
}

/// `app.json` and `state.json`, serialized on the UI thread at one save.
///
/// Capturing is cheap (a few KiB of JSON); landing them is not. An atomic
/// write ends in `File::sync_all`, which is `F_FULLFSYNC` on Apple targets
/// and measured ~5 ms per file on an idle SSD, so the bytes are written by
/// [`DesktopDocumentWriter`] on the background executor instead.
pub struct DesktopDocuments {
    generation: u64,
    settings: Vec<u8>,
    state: Vec<u8>,
}

/// Writes captured [`DesktopDocuments`] off the UI thread.
///
/// Saves run once a second while a provider streams and almost never change
/// either document, so a file whose bytes match the last successful write is
/// skipped. Writes are serialized by the lock and stamped with a capture
/// generation: a superseded capture that runs late is dropped instead of
/// overwriting newer state. A failed write leaves the remembered bytes alone,
/// so the next save retries it.
pub struct DesktopDocumentWriter {
    app_settings_path: PathBuf,
    app_state_path: PathBuf,
    next_generation: AtomicU64,
    written: Mutex<WrittenDesktopDocuments>,
}

#[derive(Default)]
struct WrittenDesktopDocuments {
    generation: u64,
    settings: Option<Vec<u8>>,
    state: Option<Vec<u8>>,
}

impl DesktopDocumentWriter {
    fn new(app_settings_path: PathBuf, app_state_path: PathBuf) -> Self {
        Self {
            app_settings_path,
            app_state_path,
            next_generation: AtomicU64::new(0),
            written: Mutex::new(WrittenDesktopDocuments::default()),
        }
    }

    fn capture(&self, state: &PersistedState) -> io::Result<DesktopDocuments> {
        Ok(DesktopDocuments {
            generation: self.next_generation.fetch_add(1, Ordering::AcqRel) + 1,
            settings: serde_json::to_vec_pretty(&state.app_settings()).map_err(to_io_error)?,
            state: serde_json::to_vec_pretty(&state.app_state()).map_err(to_io_error)?,
        })
    }

    /// Lands `documents` unless a newer capture already has. Blocking file
    /// I/O: call it from a background thread.
    pub fn write(&self, documents: DesktopDocuments) -> io::Result<()> {
        let mut written = self.written.lock();
        if documents.generation <= written.generation {
            return Ok(());
        }
        if written.settings.as_ref() != Some(&documents.settings) {
            write_bytes_atomically(&self.app_settings_path, &documents.settings)?;
            written.settings = Some(documents.settings);
        }
        if written.state.as_ref() != Some(&documents.state) {
            write_bytes_atomically(&self.app_state_path, &documents.state)?;
            written.state = Some(documents.state);
        }
        written.generation = documents.generation;
        Ok(())
    }
}

/// A task-state save already sent to the daemon.
///
/// Saves used to be fire-and-forget, with the dirty set cleared on send: a
/// save the daemon failed to write, or one still queued when the connection
/// dropped, lost those edits silently. Waiting on this reports the outcome.
pub struct PendingTaskStateSave {
    session_ids: Vec<Uuid>,
    response: crate::PendingResponse,
}

impl PendingTaskStateSave {
    /// Sessions this save carried; re-mark them dirty if it fails.
    pub fn session_ids(&self) -> &[Uuid] {
        &self.session_ids
    }

    /// Blocks until the daemon has stored the save or it has failed.
    pub fn wait(self) -> io::Result<()> {
        match self.response.wait().map_err(to_io_error)? {
            ResponsePayload::Ack | ResponsePayload::TaskStateSaved { .. } => Ok(()),
            _ => Err(io::Error::other(
                "Raku daemon returned an invalid task-state save response",
            )),
        }
    }
}

/// Desktop state store: app files stay local, task data crosses RPC.
pub struct StateStore {
    path: PathBuf,
    app_state_path: PathBuf,
    app_settings_path: PathBuf,
    documents: Arc<DesktopDocumentWriter>,
    legacy_settings_paths: Vec<PathBuf>,
    daemon: DaemonSupervisor,
    remote_default_cwd: Mutex<Option<PathBuf>>,
    /// A task snapshot may only be written after this client has successfully
    /// loaded the daemon's authoritative state. Falling back to an empty UI
    /// after a transient RPC failure must never turn the next ordinary save
    /// into a destructive replacement of the daemon database.
    task_state_loaded: AtomicBool,
}

impl StateStore {
    pub fn backup_task_save(&self, state: &PersistedState) -> io::Result<Command> {
        if !self.task_state_loaded.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "task state was not loaded; refusing to overwrite daemon data before backup",
            ));
        }
        Ok(state.backup_task_save())
    }

    pub fn default_path() -> PathBuf {
        if cfg!(debug_assertions) {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(Path::parent)
                .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")))
                .join("temp")
                .join("app.db")
        } else {
            dirs::data_local_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join(DATA_DIRECTORY_NAME)
                .join("app.db")
        }
    }

    pub fn remote(daemon: DaemonSupervisor) -> Self {
        Self {
            app_state_path: default_app_state_path(),
            app_settings_path: default_app_settings_path(),
            documents: Arc::new(DesktopDocumentWriter::new(
                default_app_settings_path(),
                default_app_state_path(),
            )),
            legacy_settings_paths: default_legacy_settings_paths(),
            path: Self::default_path(),
            daemon,
            remote_default_cwd: Mutex::new(None),
            task_state_loaded: AtomicBool::new(false),
        }
    }

    pub fn session_message_search(
        &self,
        query: String,
        limit: usize,
    ) -> impl FnOnce() -> io::Result<Vec<SessionMessageMatch>> + Send + 'static {
        let daemon = self.daemon.clone();
        move || match daemon
            .client()
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SearchSessionMessages { query, limit },
            )
            .map_err(to_io_error)?
        {
            ResponsePayload::SessionMessageMatches { matches } => Ok(matches),
            _ => Err(io::Error::other(
                "Raku daemon returned an invalid message-search response",
            )),
        }
    }

    pub fn provider_sessions(
        &self,
        provider: ProviderKind,
        limit: usize,
    ) -> impl FnOnce() -> io::Result<Vec<ProviderSessionSummary>> + Send + 'static {
        let daemon = self.daemon.clone();
        move || match daemon
            .client()
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::ListProviderSessions { provider, limit },
            )
            .map_err(to_io_error)?
        {
            ResponsePayload::ProviderSessions { sessions } => Ok(sessions),
            _ => Err(io::Error::other(
                "Raku daemon returned an invalid provider-session response",
            )),
        }
    }

    pub fn provider_session_history(
        &self,
        cursor: ProviderResumeCursor,
        cwd: PathBuf,
    ) -> impl FnOnce() -> io::Result<ProviderSessionHistory> + Send + 'static {
        let daemon = self.daemon.clone();
        move || match daemon
            .client()
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::LoadProviderSession { cursor, cwd },
            )
            .map_err(to_io_error)?
        {
            ResponsePayload::ProviderSessionHistory { history } => Ok(history),
            _ => Err(io::Error::other(
                "Raku daemon returned an invalid provider-session history response",
            )),
        }
    }

    pub fn load_or_fresh(&self, cwd: PathBuf) -> PersistedState {
        let mut state = match self.load() {
            Ok(state) => {
                self.task_state_loaded.store(true, Ordering::Release);
                state
            }
            Err(_) if cwd.parent().is_none() => PersistedState::empty(),
            Err(_) => PersistedState::fresh(cwd),
        };
        if state.projects.is_empty()
            && let Some(cwd) = self.remote_default_cwd.lock().clone()
            && cwd.parent().is_some()
        {
            let project = Project::from_path(cwd);
            let project_id = project.id;
            state.projects.push(project);
            let session = state.new_session(project_id);
            state.selected_project = Some(project_id);
            state.selected_session = Some(session.id);
            state.push_session(session);
        }
        state.ensure_runtime_session();
        if let Some(selected) = state.selected_session
            && let Some(session) = state
                .sessions
                .iter_mut()
                .find(|session| session.id == selected)
        {
            let _ = self.hydrate(session);
        }
        state
    }

    pub fn load(&self) -> io::Result<PersistedState> {
        let (projects, mut sessions, default_cwd) = match self
            .daemon
            .client()
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .map_err(to_io_error)?
        {
            ResponsePayload::TaskState {
                projects,
                sessions,
                default_cwd,
                projectless_root,
            } => {
                raku_protocol::projectless::set_workspace_root(projectless_root);
                (projects, sessions, default_cwd)
            }
            _ => {
                return Err(io::Error::other(
                    "Raku daemon returned an invalid task-state response",
                ));
            }
        };
        restore_task_state_skeletons(&mut sessions);
        *self.remote_default_cwd.lock() = Some(default_cwd);
        let mut state = PersistedState::empty();
        state.projects = projects;
        state.sessions = sessions;
        let app_settings_missing = !self.app_settings_path.is_file();
        if let Some(settings) = self.read_app_settings()? {
            state.apply_app_settings(settings);
        }
        let app_state = read_app_state_file(&self.app_state_path);
        let app_state_missing = app_state.is_none();
        if let Some(app_state) = app_state {
            state.apply_app_state(app_state);
        }
        state.migrate_loaded();
        if app_settings_missing {
            let _ = self.write_app_settings(&state.app_settings());
        }
        if app_state_missing {
            let _ = self.write_app_state(&state.app_state());
        }
        Ok(state)
    }

    pub fn hydrate(&self, session: &mut AgentSession) -> io::Result<()> {
        if session.detail_loaded {
            return Ok(());
        }
        match hydrate_session(&self.daemon, session.id)? {
            Some(mut stored) => {
                stored.drop_plan_activities();
                *session = stored;
                Ok(())
            }
            None => {
                session.detail_loaded = true;
                Ok(())
            }
        }
    }

    /// Synchronous save of everything: desktop documents, then task state.
    /// For one-shot paths only — frames use [`Self::capture_desktop_documents`]
    /// plus a background [`DesktopDocumentWriter::write`], then
    /// [`Self::save_task_state`].
    pub fn save(&self, state: &mut PersistedState) -> io::Result<()> {
        self.documents
            .write(self.capture_desktop_documents(state)?)?;
        let pending = self.save_task_state(state)?;
        let session_ids = pending.session_ids().to_vec();
        pending.wait().inspect_err(|_| {
            for session_id in session_ids {
                state.mark_session_dirty(session_id);
            }
        })
    }

    pub fn capture_desktop_documents(
        &self,
        state: &PersistedState,
    ) -> io::Result<DesktopDocuments> {
        self.documents.capture(state)
    }

    pub fn desktop_document_writer(&self) -> Arc<DesktopDocumentWriter> {
        self.documents.clone()
    }

    /// Sends the dirty sessions to the daemon and returns the save to await.
    ///
    /// The send is a channel push (no I/O), so it belongs on the caller's
    /// thread: that fixes the save's order among everything else this client
    /// sends. Await it elsewhere. The dirty set is cleared now; a save that
    /// fails must put [`PendingTaskStateSave::session_ids`] back with
    /// [`PersistedState::mark_session_dirty`], or those edits are never
    /// resent.
    pub fn save_task_state(&self, state: &mut PersistedState) -> io::Result<PendingTaskStateSave> {
        if !self.task_state_loaded.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "task state was not loaded; refusing to overwrite daemon data",
            ));
        }
        let dirty_ids = state.dirty_sessions.clone();
        let sessions = state
            .sessions
            .iter()
            .filter(|session| dirty_ids.contains(&session.id))
            .cloned()
            .collect::<Vec<_>>();
        let session_ids = sessions.iter().map(|session| session.id).collect();
        let live_session_ids = state.sessions.iter().map(|session| session.id).collect();
        let response = self
            .daemon
            .client()
            .begin_request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: state.projects.clone(),
                    live_session_ids,
                    sessions,
                },
            )
            .map_err(to_io_error)?;
        state.dirty_sessions.clear();
        Ok(PendingTaskStateSave {
            session_ids,
            response,
        })
    }

    pub fn remove_session(&self, session_id: Uuid) -> io::Result<()> {
        self.daemon
            .client()
            .notify(session_id, Uuid::nil(), Command::RemoveSession)
            .map_err(to_io_error)
    }

    pub fn blob_sweep(&self) -> impl FnOnce() + Send + 'static {
        let daemon = self.daemon.clone();
        move || {
            let _ = daemon
                .client()
                .request(Uuid::nil(), Uuid::nil(), Command::SweepBlobs);
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read_app_settings(&self) -> io::Result<Option<AppSettings>> {
        let source =
            read_app_settings_source(&self.app_settings_path, &self.legacy_settings_paths)?;
        source
            .map(|(bytes, _)| from_persisted_json(&bytes).map_err(to_io_error))
            .transpose()
    }

    fn write_app_settings(&self, settings: &AppSettings) -> io::Result<()> {
        write_json_atomically(&self.app_settings_path, settings)
    }

    fn write_app_state(&self, state: &AppState) -> io::Result<()> {
        write_json_atomically(&self.app_state_path, state)
    }
}

/// Fetches one daemon-owned session. Remote image materialization is kept
/// separate so presentation code can show the transcript before large blobs
/// finish crossing the daemon boundary.
pub fn hydrate_session(
    daemon: &DaemonSupervisor,
    session_id: Uuid,
) -> io::Result<Option<AgentSession>> {
    match daemon
        .client()
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::HydrateSession { session_id },
        )
        .map_err(to_io_error)?
    {
        ResponsePayload::Session { session } => Ok(session),
        _ => Err(io::Error::other(
            "Raku daemon returned an invalid session-hydration response",
        )),
    }
}

fn write_json_atomically(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let data = serde_json::to_vec_pretty(value).map_err(to_io_error)?;
    write_bytes_atomically(path, &data)
}

fn write_bytes_atomically(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary)?;
    file.write_all(data)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn restore_task_state_skeletons(sessions: &mut [AgentSession]) {
    for session in sessions {
        session.detail_loaded = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_font_size_falls_back_without_rejecting_settings() {
        // The retired free-form UI size is ignored, and the transcript size
        // takes its default.
        let legacy = serde_json::to_vec(&serde_json::json!({
            "theme": "dark",
            "ui_font_size": 17.0
        }))
        .unwrap();
        let settings: AppSettings = from_persisted_json(&legacy).unwrap();
        assert_eq!(settings.theme, ThemePreference::Dark);
        assert_eq!(settings.transcript_font_size, TranscriptFontSize::Medium);

        for value in [serde_json::json!("huge"), serde_json::json!(16.0)] {
            let document = serde_json::to_vec(&serde_json::json!({
                "theme": "dark",
                "transcript_font_size": value
            }))
            .unwrap();
            let settings: AppSettings = from_persisted_json(&document).unwrap();
            assert_eq!(settings.theme, ThemePreference::Dark);
            assert_eq!(settings.transcript_font_size, TranscriptFontSize::Medium);
        }

        let document = serde_json::to_vec(&serde_json::json!({
            "transcript_font_size": "small"
        }))
        .unwrap();
        let settings: AppSettings = from_persisted_json(&document).unwrap();
        assert_eq!(settings.transcript_font_size, TranscriptFontSize::Small);
        assert_eq!(TranscriptFontSize::Small.px(), 15.0);
        assert_eq!(TranscriptFontSize::Medium.px(), 16.0);
        assert_eq!(TranscriptFontSize::Large.px(), 17.0);
    }

    #[test]
    fn retired_provider_settings_do_not_block_launch_settings() {
        for provider in [
            "deepSeek",
            "ohMyPi",
            "openCode",
            "openCode2",
            "antigravity",
            "agy",
        ] {
            let document = serde_json::to_vec(&serde_json::json!({
                "theme": "dark",
                "language": "simplified-chinese",
                "transcript_font_size": "large",
                "model_picker_favorites_only": true,
                "favorite_models": [
                    {"provider": provider, "model": "retired"},
                    {"provider": "codex", "model": "gpt-5.4"},
                    {"provider": "pi", "model": "deepseek/deepseek-flash"}
                ],
                // Retired with the sub-agent system; must not block loading.
                "sub_agents": [
                    {"id": Uuid::new_v4(), "name": "reviewer", "provider": "claude",
                     "modelId": "sonnet", "instructions": "Review the diff"}
                ]
            }))
            .unwrap();

            let settings: AppSettings = from_persisted_json(&document).unwrap();
            assert_eq!(settings.theme, ThemePreference::Dark);
            assert_eq!(settings.language, AppLanguage::SimplifiedChinese);
            assert_eq!(settings.transcript_font_size, TranscriptFontSize::Large);
            assert!(settings.model_picker_favorites_only);
            assert_eq!(
                settings.favorite_models,
                vec![
                    FavoriteModel {
                        provider: ProviderKind::Codex,
                        model: "gpt-5.4".into(),
                    },
                    FavoriteModel {
                        provider: ProviderKind::Pi,
                        model: "deepseek/deepseek-flash".into(),
                    },
                ]
            );
        }
    }

    #[test]
    fn desktop_document_writes_skip_superseded_and_unchanged_captures() {
        let directory = std::env::temp_dir().join(format!("raku-documents-{}", Uuid::new_v4()));
        let settings_path = directory.join("app.json");
        let state_path = directory.join("state.json");
        let writer = DesktopDocumentWriter::new(settings_path.clone(), state_path.clone());
        let documents = |generation, settings: &str, state: &str| DesktopDocuments {
            generation,
            settings: settings.as_bytes().to_vec(),
            state: state.as_bytes().to_vec(),
        };

        writer.write(documents(2, "settings-2", "state-2")).unwrap();
        assert_eq!(fs::read_to_string(&settings_path).unwrap(), "settings-2");
        assert_eq!(fs::read_to_string(&state_path).unwrap(), "state-2");

        // A capture taken before the one already on disk must not regress it.
        writer.write(documents(1, "settings-1", "state-1")).unwrap();
        assert_eq!(fs::read_to_string(&settings_path).unwrap(), "settings-2");

        // Unchanged bytes are not rewritten: an external edit survives.
        fs::write(&settings_path, "external").unwrap();
        writer.write(documents(3, "settings-2", "state-3")).unwrap();
        assert_eq!(fs::read_to_string(&settings_path).unwrap(), "external");
        assert_eq!(fs::read_to_string(&state_path).unwrap(), "state-3");

        // A failed write is retried by the next capture.
        fs::remove_file(&state_path).unwrap();
        fs::create_dir(&state_path).unwrap();
        assert!(writer.write(documents(4, "settings-2", "state-4")).is_err());
        fs::remove_dir(&state_path).unwrap();
        writer.write(documents(5, "settings-2", "state-4")).unwrap();
        assert_eq!(fs::read_to_string(&state_path).unwrap(), "state-4");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn retired_project_model_selections_preserve_other_desktop_state() {
        let retired_project = Uuid::new_v4();
        let active_project = Uuid::new_v4();
        for provider in [
            "deepSeek",
            "ohMyPi",
            "openCode",
            "openCode2",
            "antigravity",
            "agy",
        ] {
            let document = serde_json::to_vec(&serde_json::json!({
                "app_state_version": 1,
                "selected_project": active_project,
                "last_provider": "claude",
                "last_model": "claude-sonnet-4-6",
                "sidebar_width": 318.0,
                "project_model_selections": {
                    retired_project.to_string(): {"provider": provider, "model": "retired"},
                    active_project.to_string(): {"provider": "pi", "model": "active"}
                }
            }))
            .unwrap();

            let state: AppState = from_persisted_json(&document).unwrap();
            assert_eq!(state.selected_project, Some(active_project));
            assert_eq!(state.last_provider, ProviderKind::Claude);
            assert_eq!(state.last_model.as_deref(), Some("claude-sonnet-4-6"));
            assert_eq!(state.sidebar_width, 318.0);
            assert_eq!(state.project_model_selections.len(), 1);
            assert!(
                !state
                    .project_model_selections
                    .contains_key(&retired_project)
            );
            assert_eq!(
                state.project_model_selections[&active_project].provider,
                ProviderKind::Pi
            );
            assert_eq!(
                state.project_model_selections[&active_project]
                    .model
                    .as_deref(),
                Some("active")
            );
        }
    }

    #[test]
    fn retired_provider_selections_do_not_reset_desktop_state() {
        let directory = std::env::temp_dir().join(format!("raku-state-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("state.json");
        let project_id = Uuid::new_v4();
        for provider in ["ohMyPi", "antigravity", "agy"] {
            fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({
                    "app_state_version": 1,
                    "selected_project": project_id,
                    "last_provider": provider,
                    "last_model": "deepseek-official/deepseek-chat",
                    "last_reasoning_effort": "high",
                    "last_service_tier": "fast",
                    "last_context_window": "1m",
                    "sidebar_width": 318.0,
                    "remembered_model_traits": [
                        {"provider": "deepSeek", "model": "retired"},
                        {"provider": "ohMyPi", "model": "retired"},
                        {"provider": "openCode", "model": "retired"},
                        {"provider": "openCode2", "model": "retired"},
                        {"provider": "antigravity", "model": "retired"},
                        {"provider": "agy", "model": "retired"},
                        {"provider": "pi", "model": "deepseek/deepseek-flash",
                         "reasoning_effort": "low"}
                    ]
                }))
                .unwrap(),
            )
            .unwrap();

            let mut state = PersistedState::empty();
            state.apply_app_state(read_app_state_file(&path).unwrap());
            assert_eq!(state.selected_project, Some(project_id));
            assert_eq!(state.sidebar_width, 318.0);
            let draft = state.new_session(project_id);
            assert_eq!(draft.provider, ProviderKind::Codex);
            assert_eq!(draft.model, None);
            assert_eq!(draft.reasoning_effort, None);
            assert_eq!(draft.service_tier, None);
            assert_eq!(draft.context_window, None);
            assert_eq!(
                state.model_traits_for(ProviderKind::Pi, "deepseek/deepseek-flash"),
                (Some("low".into()), None, None)
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn backup_directory_is_optional_and_round_trips_in_desktop_settings() {
        let defaults: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(defaults.backup_directory.is_none());
        let mut state = PersistedState::empty();
        state.backup_directory = Some(PathBuf::from("/backups/Raku"));
        let documents = state.desktop_backup().unwrap();
        let mut restored = PersistedState::empty();
        restored.apply_app_settings(serde_json::from_value(documents.settings).unwrap());
        assert_eq!(restored.backup_directory, state.backup_directory);
    }

    #[test]
    fn restore_preferences_and_navigation_but_keep_local_backup_path_and_token() {
        let directory =
            std::env::temp_dir().join(format!("raku-desktop-restore-{}", Uuid::new_v4()));
        let settings_path = directory.join("app.json");
        let state_path = directory.join("state.json");
        let mut current = AppSettings::default();
        current.backup_directory = Some(PathBuf::from("/local/backups"));
        write_json_atomically(&settings_path, &current).unwrap();
        let mut state = PersistedState::empty();
        state.theme = ThemePreference::Dark;
        state.language = AppLanguage::SimplifiedChinese;
        state.transcript_font_size = TranscriptFontSize::Large;
        state.interface_font = Some("Avenir Next".into());
        state.transcript_font = Some("Georgia".into());
        state.backup_directory = Some(PathBuf::from("/old-machine/backups"));
        state.selected_project = Some(Uuid::new_v4());
        let documents = state.desktop_backup().unwrap();
        restore_desktop_documents(
            &settings_path,
            &state_path,
            &serde_json::to_vec(&documents).unwrap(),
        )
        .unwrap();
        let settings: AppSettings =
            serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
        assert_eq!(settings.theme, ThemePreference::Dark);
        assert_eq!(settings.language, state.language);
        assert_eq!(settings.transcript_font_size, TranscriptFontSize::Large);
        assert_eq!(settings.interface_font, state.interface_font);
        assert_eq!(settings.transcript_font, state.transcript_font);
        assert_eq!(settings.backup_directory, current.backup_directory);
        let restored: AppState = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(restored.selected_project, state.selected_project);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_desktop_backup_does_not_replace_existing_documents() {
        let directory =
            std::env::temp_dir().join(format!("raku-desktop-restore-{}", Uuid::new_v4()));
        let settings_path = directory.join("app.json");
        let state_path = directory.join("state.json");
        write_json_atomically(&settings_path, &AppSettings::default()).unwrap();
        let original = fs::read(&settings_path).unwrap();
        let mut documents = PersistedState::empty().desktop_backup().unwrap();
        documents.state["app_state_version"] = serde_json::json!(999);
        assert!(
            restore_desktop_documents(
                &settings_path,
                &state_path,
                &serde_json::to_vec(&documents).unwrap()
            )
            .is_err()
        );
        assert_eq!(fs::read(&settings_path).unwrap(), original);
        assert!(!state_path.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn font_families_default_to_system_and_persist_independently() {
        let defaults: AppSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.interface_font, None);
        assert_eq!(defaults.transcript_font, None);
        assert_eq!(defaults.fallback_font, None);

        let mut state = PersistedState::empty();
        state.interface_font = Some("Avenir Next".into());
        state.transcript_font = Some("Georgia".into());
        state.fallback_font = Some("Hiragino Sans".into());
        let settings = serde_json::to_string(&state.app_settings()).unwrap();
        let mut restored = PersistedState::empty();
        restored.apply_app_settings(serde_json::from_str(&settings).unwrap());
        assert_eq!(restored.interface_font, state.interface_font);
        assert_eq!(restored.transcript_font, state.transcript_font);
        assert_eq!(restored.fallback_font, state.fallback_font);

        restored.transcript_font = None;
        let settings = serde_json::to_string(&restored.app_settings()).unwrap();
        restored.apply_app_settings(serde_json::from_str(&settings).unwrap());
        assert_eq!(restored.interface_font, state.interface_font);
        assert_eq!(restored.transcript_font, None);
        assert_eq!(restored.fallback_font, state.fallback_font);
    }

    #[test]
    fn model_picker_favorites_filter_defaults_off_and_persists() {
        let defaults: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(!defaults.model_picker_favorites_only);
        let mut state = PersistedState::empty();
        state.model_picker_favorites_only = true;
        let settings = serde_json::to_string(&state.app_settings()).unwrap();
        let mut restored = PersistedState::empty();
        restored.apply_app_settings(serde_json::from_str(&settings).unwrap());
        assert!(restored.model_picker_favorites_only);
    }

    #[test]
    fn side_chat_model_settings_default_and_persist_together() {
        let defaults: AppSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.side_chat_provider, Some(ProviderKind::Pi));
        assert_eq!(defaults.side_chat_reasoning_effort.as_deref(), Some("low"));
        let mut state = PersistedState::empty();
        state.side_chat_provider = Some(ProviderKind::Pi);
        state.side_chat_model = Some("provider/reasoner".into());
        state.side_chat_reasoning_effort = Some("high".into());
        let settings = serde_json::to_string(&state.app_settings()).unwrap();
        let mut restored = PersistedState::empty();
        restored.apply_app_settings(serde_json::from_str(&settings).unwrap());
        assert_eq!(restored.side_chat_provider, state.side_chat_provider);
        assert_eq!(restored.side_chat_model, state.side_chat_model);
        assert_eq!(
            restored.side_chat_reasoning_effort,
            state.side_chat_reasoning_effort
        );
    }

    #[test]
    fn text_generation_is_off_by_default_and_persists_its_choice() {
        let defaults: AppSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.text_generation_provider, None);
        let mut state = PersistedState::empty();
        state.text_generation_provider = Some(ProviderKind::Claude);
        state.text_generation_model = Some("claude-haiku-4-5".into());
        state.text_generation_reasoning_effort = Some("low".into());
        let settings = serde_json::to_string(&state.app_settings()).unwrap();
        let mut restored = PersistedState::empty();
        restored.apply_app_settings(serde_json::from_str(&settings).unwrap());
        assert_eq!(
            restored.text_generation_provider,
            Some(ProviderKind::Claude)
        );
        assert_eq!(
            restored.text_generation_model.as_deref(),
            Some("claude-haiku-4-5")
        );
        assert_eq!(
            restored.text_generation_reasoning_effort.as_deref(),
            Some("low")
        );
    }

    #[test]
    fn legacy_automatic_side_chat_settings_become_pi_with_low_effort() {
        let legacy: AppSettings = serde_json::from_str(
            r#"{
                "side_chat_provider": null,
                "side_chat_model": null,
                "side_chat_reasoning_effort": null
            }"#,
        )
        .unwrap();
        let mut restored = PersistedState::empty();
        restored.apply_app_settings(legacy);
        assert_eq!(restored.side_chat_provider, Some(ProviderKind::Pi));
        assert_eq!(restored.side_chat_reasoning_effort.as_deref(), Some("low"));
        let settings = serde_json::to_value(restored.app_settings()).unwrap();
        assert_eq!(settings["side_chat_provider"], "pi");
        assert_eq!(settings["side_chat_reasoning_effort"], "low");
    }

    #[test]
    fn retired_desktop_preferences_cannot_be_restored_from_older_settings() {
        let mut defaults: AppSettings = serde_json::from_str(
            r#"{
            "analytics_enabled": true,
            "render_math": false
        }"#,
        )
        .unwrap();
        defaults.normalize_desktop_defaults();
        assert!(!defaults.analytics_enabled);
        assert!(defaults.render_math);

        let mut restored = PersistedState::empty();
        restored.apply_app_settings(defaults);
        assert!(!restored.analytics_enabled);
        assert!(restored.render_math);
        // Saving any other setting also writes the fixed desktop defaults.
        restored.analytics_enabled = true;
        restored.render_math = false;
        let settings = restored.app_settings();
        assert!(!settings.analytics_enabled);
        assert!(settings.render_math);
    }

    #[test]
    fn desktop_settings_paths_are_build_specific() {
        let app_settings_path = default_app_settings_path();
        let legacy_settings_paths = default_legacy_settings_paths();

        #[cfg(debug_assertions)]
        {
            let state_path = StateStore::default_path();
            assert_eq!(app_settings_path, state_path.with_file_name("app.json"));
            assert_eq!(
                legacy_settings_paths,
                [state_path.with_file_name("settings.json")]
            );
        }

        #[cfg(not(debug_assertions))]
        {
            assert_eq!(
                app_settings_path,
                configuration_directory().join("app.json")
            );
            assert_eq!(
                legacy_settings_paths,
                [configuration_directory().join("settings.json")]
            );
        }
    }

    #[test]
    fn legacy_app_state_defaults_sidebar_presentation() {
        let state: AppState = serde_json::from_str(r#"{"app_state_version":1}"#).unwrap();

        assert_eq!(state.sidebar_grouping, SidebarGrouping::Updated);
        assert_eq!(state.sidebar_ordering, SidebarOrdering::Newest);
        assert_eq!(state.last_runtime_mode, RuntimeMode::FullAccess);
        assert!(state.removed_project_ids.is_empty());
        assert!(state.pinned_session_ids.is_empty());
    }

    #[test]
    fn sidebar_pins_survive_reload_without_hydrating_sessions_and_drop_deleted_ids() {
        let mut state = PersistedState::fresh(PathBuf::from("/tmp/project"));
        state.sessions[0].push_message(raku_protocol::model::MessageRole::User, "Started task");
        let session_id = state.sessions[0].id;
        let deleted_id = Uuid::new_v4();
        state.pinned_session_ids = HashSet::from([session_id, deleted_id]);
        let encoded = serde_json::to_vec(&state.app_state()).unwrap();

        let mut restored = PersistedState::empty();
        restored.sessions = state
            .sessions
            .iter()
            .map(AgentSession::list_projection)
            .collect();
        restored.apply_app_state(serde_json::from_slice(&encoded).unwrap());
        restored.migrate_loaded();
        assert_eq!(restored.pinned_session_ids, HashSet::from([session_id]));
        assert!(!restored.sessions[0].detail_loaded);
        assert!(restored.sessions[0].messages.is_empty());

        restored.pinned_session_ids.remove(&session_id);
        let encoded = serde_json::to_vec(&restored.app_state()).unwrap();
        state.apply_app_state(serde_json::from_slice(&encoded).unwrap());
        assert!(state.pinned_session_ids.is_empty());
    }

    #[test]
    fn removed_projects_survive_app_state_reload_without_deleting_history() {
        let mut state = PersistedState::fresh(PathBuf::from("/tmp/project"));
        let project_id = state.projects[0].id;
        let session_id = state.sessions[0].id;
        state.removed_project_ids.insert(project_id);
        let encoded = serde_json::to_vec(&state.app_state()).unwrap();
        state.removed_project_ids.clear();
        state.apply_app_state(serde_json::from_slice(&encoded).unwrap());

        assert!(state.removed_project_ids.contains(&project_id));
        assert_eq!(state.projects[0].id, project_id);
        assert_eq!(state.sessions[0].id, session_id);
    }

    #[test]
    fn new_tasks_inherit_the_remembered_access_mode() {
        let mut state = PersistedState::fresh(PathBuf::from("/tmp/project"));
        state.last_runtime_mode = RuntimeMode::Ask;

        let session = state.new_session(state.projects[0].id);

        assert_eq!(session.runtime_mode, RuntimeMode::Ask);
        assert_eq!(state.app_state().last_runtime_mode, RuntimeMode::Ask);
    }

    #[test]
    fn project_model_choices_only_affect_that_projects_future_tasks() {
        let mut state = PersistedState::empty();
        for provider in ProviderKind::ALL {
            let project = Project::from_path(PathBuf::from(format!("/tmp/{}", provider.id())));
            let mut session = AgentSession::new(project.id, provider);
            session.model = Some(format!("{}-model", provider.id()));
            session.reasoning_effort = Some("high".into());
            let session_id = session.id;
            state.projects.push(project);
            state.push_session(session);
            state.remember_session_model_selection(session_id);
        }
        let before = state
            .projects
            .iter()
            .map(|project| ProjectModelSelection::for_session(&state.new_session(project.id)))
            .collect::<Vec<_>>();
        let existing_task = serde_json::to_value(&state.sessions[1]).unwrap();
        let changed_id = state.sessions[0].id;
        state.session_mut(changed_id).unwrap().provider = ProviderKind::Pi;
        state.session_mut(changed_id).unwrap().model = Some("another-model".into());
        state.remember_session_model_selection(changed_id);

        assert_eq!(
            state.new_session(state.projects[0].id).provider,
            ProviderKind::Pi
        );
        assert_eq!(
            state.new_session(state.projects[0].id).model.as_deref(),
            Some("another-model")
        );
        for (project, expected) in state.projects.iter().zip(before).skip(1) {
            assert_eq!(
                ProjectModelSelection::for_session(&state.new_session(project.id)),
                expected
            );
        }
        assert_eq!(
            serde_json::to_value(&state.sessions[1]).unwrap(),
            existing_task
        );
        let untouched = Project::from_path(PathBuf::from("/tmp/untouched-project"));
        assert_eq!(
            state.new_session(untouched.id).provider,
            ProviderKind::Codex
        );
        assert_eq!(state.new_session(untouched.id).model, None);
    }

    #[test]
    fn no_project_model_choice_survives_workspace_replacement_and_reload() {
        let root = raku_protocol::projectless::workspace_root().unwrap();
        let first = Project::from_path(root.join("2026-11-02/first"));
        let second = Project::from_path(root.join("2026-11-02/second"));
        let mut state = PersistedState::empty();
        let mut session = AgentSession::new(first.id, ProviderKind::Pi);
        session.model = Some("anthropic/claude-opus".into());
        session.reasoning_effort = Some("medium".into());
        let session_id = session.id;
        state.projects.push(first);
        state.push_session(session);
        state.remember_session_model_selection(session_id);
        // Removing the last workspace must not remove the No Project preference.
        state.projects.clear();
        state.sessions.clear();
        let encoded = serde_json::to_vec(&state.app_state()).unwrap();
        let mut restored = PersistedState::empty();
        restored.apply_app_state(serde_json::from_slice(&encoded).unwrap());
        let project_id = second.id;
        restored.projects.push(second);

        let next = restored.new_session(project_id);
        assert_eq!(next.provider, ProviderKind::Pi);
        assert_eq!(next.model.as_deref(), Some("anthropic/claude-opus"));
        assert_eq!(next.reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(restored.new_session_provider(None), ProviderKind::Pi);
        assert_eq!(
            restored.new_session(Uuid::new_v4()).provider,
            ProviderKind::Codex
        );
    }

    #[test]
    fn project_model_choices_survive_history_navigation_and_restart() {
        let mut state = PersistedState::fresh(PathBuf::from("/tmp/project"));
        let project_id = state.projects[0].id;
        let old_id = state.sessions[0].id;
        let mut current = AgentSession::new(project_id, ProviderKind::Claude);
        current.model = Some("claude-sonnet-5".into());
        current.reasoning_effort = Some("high".into());
        current.service_tier = Some("fast".into());
        current.context_window = Some("1m".into());
        let current_id = current.id;
        state.push_session(current);
        state.remember_session_model_selection(current_id);
        state.selected_session = Some(old_id);
        let encoded = serde_json::to_vec(&state.app_state()).unwrap();
        let mut restored = PersistedState::empty();
        restored.projects = state.projects;
        restored.sessions = state.sessions;
        // Even newer history cannot overwrite an explicitly saved preference.
        restored.sessions[0].updated_at = u64::MAX;
        restored.apply_app_state(serde_json::from_slice(&encoded).unwrap());
        restored.migrate_loaded();
        restored.selected_session = None;
        restored.ensure_runtime_session();
        let next = restored.sessions.last().unwrap();

        assert_eq!(next.provider, ProviderKind::Claude);
        assert_eq!(next.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(next.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(next.service_tier.as_deref(), Some("fast"));
        assert_eq!(next.context_window.as_deref(), Some("1m"));
        assert_eq!(restored.sessions[0].provider, ProviderKind::Codex);
    }

    #[test]
    fn legacy_preferences_are_seeded_from_each_projects_latest_task() {
        let root = raku_protocol::projectless::workspace_root().unwrap();
        let projects = vec![
            Project::from_path(PathBuf::from("/tmp/project-a")),
            Project::from_path(PathBuf::from("/tmp/project-b")),
            Project::from_path(root.join("2026-11-01/first")),
            Project::from_path(root.join("2026-11-02/second")),
        ];
        let mut state = PersistedState::empty();
        state.apply_app_state(
            serde_json::from_str(
                r#"{
                    "app_state_version":1,
                    "last_provider":"claude",
                    "last_model":"model-4",
                    "last_reasoning_effort":"high",
                    "last_service_tier":"fast",
                    "last_context_window":"1m",
                    "remembered_model_traits":[
                        {"provider":"pi","model":"model-3","reasoning_effort":"medium"}
                    ]
                }"#,
            )
            .unwrap(),
        );
        for (index, provider) in [
            ProviderKind::Claude,
            ProviderKind::Codex,
            ProviderKind::Codex,
            ProviderKind::Pi,
            ProviderKind::Claude,
        ]
        .into_iter()
        .enumerate()
        {
            let mut session = AgentSession::new(projects[index % projects.len()].id, provider);
            session.model = Some(format!("model-{index}"));
            session.updated_at = index as u64;
            state.push_session(session.list_projection());
        }
        state.projects = projects;
        state.migrate_loaded();

        let first = state.new_session(state.projects[0].id);
        assert_eq!(first.provider, ProviderKind::Claude);
        assert_eq!(first.model.as_deref(), Some("model-4"));
        assert_eq!(first.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(first.service_tier.as_deref(), Some("fast"));
        assert_eq!(first.context_window.as_deref(), Some("1m"));
        let second = state.new_session(state.projects[1].id);
        assert_eq!(second.provider, ProviderKind::Codex);
        assert_eq!(second.model.as_deref(), Some("model-1"));
        for project in &state.projects[2..] {
            let next = state.new_session(project.id);
            assert_eq!(next.provider, ProviderKind::Pi);
            assert_eq!(next.model.as_deref(), Some("model-3"));
            assert_eq!(next.reasoning_effort.as_deref(), Some("medium"));
        }
    }

    #[test]
    fn reused_drafts_pick_up_project_preferences_without_changing_started_tasks() {
        let mut state = PersistedState::fresh(PathBuf::from("/tmp/project"));
        let project_id = state.projects[0].id;
        let draft_id = state.sessions[0].id;
        state.sessions[0].runtime_mode = RuntimeMode::Ask;
        state.sessions[0].workspace = raku_protocol::model::SessionWorkspace::NewWorktree {
            base_branch: Some("main".into()),
        };
        let workspace = state.sessions[0].workspace.clone();
        let mut started = AgentSession::new(project_id, ProviderKind::Codex);
        started.model = Some("existing-model".into());
        started.push_message(raku_protocol::model::MessageRole::User, "Keep working");
        let started_id = started.id;
        state.push_session(started);
        let mut choice = AgentSession::new(project_id, ProviderKind::Pi);
        choice.model = Some("provider/model".into());
        let choice_id = choice.id;
        state.push_session(choice);
        state.remember_session_model_selection(choice_id);

        state.refresh_new_session_model_selection(draft_id);
        state.refresh_new_session_model_selection(started_id);

        let draft = &state.sessions[0];
        assert_eq!(draft.id, draft_id);
        assert_eq!(draft.provider, ProviderKind::Pi);
        assert_eq!(draft.model.as_deref(), Some("provider/model"));
        assert_eq!(draft.runtime_mode, RuntimeMode::Ask);
        assert_eq!(draft.workspace, workspace);
        assert_eq!(state.sessions[1].provider, ProviderKind::Codex);
        assert_eq!(state.sessions[1].model.as_deref(), Some("existing-model"));
    }

    #[test]
    fn disabled_providers_fall_back_without_reusing_their_models_or_changing_history() {
        let mut state = PersistedState::fresh(PathBuf::from("/tmp/project"));
        let project_id = state.projects[0].id;
        let session_id = state.sessions[0].id;
        state.sessions[0].provider = ProviderKind::Pi;
        state.sessions[0].model = Some("provider/model".into());
        state.sessions[0].reasoning_effort = Some("high".into());
        state.remember_session_model_selection(session_id);
        state.disabled_providers.push(ProviderKind::Pi);

        let next = state.new_session(project_id);
        assert_ne!(next.provider, ProviderKind::Pi);
        assert_eq!(next.model, None);
        assert_eq!(next.reasoning_effort, None);
        state.replace_new_session_provider(ProviderKind::Pi, ProviderKind::Claude);
        let next = state.new_session(project_id);
        assert_eq!(next.provider, ProviderKind::Claude);
        assert_eq!(next.model, None);
        assert_eq!(next.reasoning_effort, None);
        assert_eq!(state.sessions[0].provider, ProviderKind::Pi);
        assert_eq!(state.sessions[0].model.as_deref(), Some("provider/model"));
    }

    #[test]
    fn daemon_task_state_becomes_list_only_after_crossing_the_client_boundary() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        session.detail_loaded = false;
        assert!(
            session.has_started(),
            "a stored skeleton is visible history"
        );

        let encoded = serde_json::to_vec(&ResponsePayload::TaskState {
            projects: Vec::new(),
            sessions: vec![session],
            default_cwd: PathBuf::from("/daemon/project"),
            projectless_root: None,
        })
        .unwrap();
        let ResponsePayload::TaskState { mut sessions, .. } =
            serde_json::from_slice(&encoded).unwrap()
        else {
            panic!("task state response changed shape");
        };

        // The skeleton marker crosses the wire, so a skeleton the client saves
        // back is still a skeleton to the daemon, not an empty session.
        assert!(!sessions[0].detail_loaded);
        assert!(sessions[0].has_started());
        restore_task_state_skeletons(&mut sessions);
        assert!(!sessions[0].detail_loaded);
        assert!(sessions[0].has_started());
    }
}

/// Reads one daemon-owned binary payload. Clients decide how to present the
/// bytes; this transport layer deliberately creates no client-side files.
pub fn read_remote_reference(
    reference: &str,
    daemon_path: Option<&Path>,
    daemon: &DaemonSupervisor,
) -> Option<Vec<u8>> {
    let command = if raku_protocol::blob::is_reference(reference) {
        Command::ReadBlob {
            reference: reference.to_owned(),
        }
    } else if reference.starts_with(raku_protocol::attachments::ATTACHMENT_SCHEME) {
        let path = daemon_path?;
        Command::ReadAttachment {
            reference: reference.to_owned(),
            path: path.to_owned(),
        }
    } else {
        return None;
    };
    let Ok(ResponsePayload::BlobData { bytes }) =
        daemon.client().request(Uuid::nil(), Uuid::nil(), command)
    else {
        return None;
    };
    Some(bytes)
}

fn to_io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
