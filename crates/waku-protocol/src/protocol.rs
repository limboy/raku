use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::attachments::{AttachmentUpload, StoredAttachment};
use crate::backup::{BackupInfo, DesktopBackup};
use crate::computer_use::ComputerPermissions;
use crate::model::{
    AgentSession, GoalOperation, Project, ProviderKind, ProviderProbe, ProviderResumeCursor,
    ProviderSessionHistory, ProviderSessionSummary, UserInputAnswer,
};
use crate::persistence::{ComposerDraftChange, ComposerDrafts, SessionMessageMatch};
use crate::provider_session::{ProviderSessionFork, ProviderSessionForkRequest};
use crate::settings::DaemonSettings;
use crate::skills::SkillsCatalog;
use crate::usage::PlanUsage;
use crate::workspace::{WorkspaceOperation, WorkspaceResult};

pub const PROTOCOL_VERSION: u32 = 13;
pub const MAX_WIRE_MESSAGE_BYTES: usize = 48 * 1024 * 1024;
pub const DAEMON_TOKEN_ENV: &str = "WAKU_DAEMON_TOKEN";
pub const DAEMON_ADDRESS_ENV: &str = "WAKU_DAEMON_ADDRESS";
pub const APP_EXECUTABLE_ENV: &str = "WAKU_APP_EXECUTABLE";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonReady {
    pub address: String,
    pub protocol_version: u32,
    pub pid: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
        token: String,
        #[serde(default)]
        resume_from: Vec<ReplayCursor>,
    },
    Request(Request),
    Shutdown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub request_id: Uuid,
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    pub command: Command,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayCursor {
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    /// Identifies the daemon process that assigned `sequence`.
    pub epoch: Uuid,
    pub sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    /// Resolve the daemon-owned provider runtime for an existing task.
    ///
    /// Clients use this after reconnecting or opening the same daemon from a
    /// second app. It observes the session actor without starting, replacing,
    /// or otherwise mutating the provider process.
    AttachSession,
    Start {
        options: WireDriverStartOptions,
    },
    Prompt {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delegation_context: Option<crate::delegation::PromptDelegationContext>,
    },
    Steer {
        prompt: String,
    },
    Cancel,
    CancelComputerUse,
    Compact {
        request: crate::compaction::CompactionRequest,
    },
    RefreshBackgroundWork,
    StopBackgroundWork {
        key: Value,
        control_id: String,
    },
    Respond {
        request_id: String,
        option_id: String,
    },
    RespondUserInput {
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    /// Ask the live provider runtime to read or mutate its persisted thread
    /// goal. Fire-and-forget: the outcome arrives as a `goalUpdated` driver
    /// event, or an `error` event when the provider refuses.
    Goal {
        operation: GoalOperation,
    },
    RunComputerTool {
        request: WireComputerToolRequest,
    },
    RejectComputerTool {
        request: WireComputerToolRequest,
        reason: String,
    },
    ApplyOptions {
        options: WireSessionOptions,
    },
    Rollback {
        turns: usize,
    },
    Fork {
        turns_to_remove: usize,
    },
    GetSettings,
    UpdateSettings {
        settings: DaemonSettings,
    },
    ListBackups {
        directory: PathBuf,
    },
    CreateBackup {
        directory: PathBuf,
        desktop: DesktopBackup,
    },
    CreateAutomaticBackup {
        directory: PathBuf,
        desktop: DesktopBackup,
    },
    /// Validates and copies a backup for application at the next desktop launch.
    StageRestore {
        path: PathBuf,
    },
    CancelRestore,
    ProbeProvider {
        provider: ProviderKind,
        binary_override: Option<String>,
        discover_models: bool,
        probe_version: bool,
    },
    FetchPlanUsage {
        provider: ProviderKind,
        binary_override: Option<String>,
        cli_version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_file: Option<PathBuf>,
    },
    ProbeComputerPermissions {
        prompt: bool,
    },
    LoadSkills {
        projects: Vec<(String, PathBuf)>,
    },
    SetSkillsEnabled {
        dirs: Vec<PathBuf>,
        enabled: bool,
    },
    TrashSkills {
        dirs: Vec<PathBuf>,
    },
    LoadTaskState,
    SaveTaskState {
        projects: Vec<Project>,
        live_session_ids: Vec<Uuid>,
        sessions: Vec<AgentSession>,
    },
    /// Explicitly remove one daemon-owned task. Ordinary state saves are
    /// merge-only so a stale client snapshot cannot delete tasks another
    /// client just created.
    RemoveSession,
    HydrateSession {
        session_id: Uuid,
    },
    SearchSessionMessages {
        query: String,
        limit: usize,
    },
    /// List one provider's resumable CLI conversations on the daemon host.
    ListProviderSessions {
        provider: ProviderKind,
        limit: usize,
    },
    /// Load the user-visible transcript for one provider-native conversation.
    LoadProviderSession {
        cursor: ProviderResumeCursor,
        cwd: PathBuf,
    },
    LoadComposerDrafts,
    SaveComposerDrafts {
        drafts: ComposerDrafts,
        generation: u64,
    },
    ApplyComposerDraftChanges {
        changes: Vec<ComposerDraftChange>,
    },
    StoreBlob {
        mime_type: String,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    ImportAttachment {
        name: String,
        upload: AttachmentUpload,
    },
    ImportPathAttachment {
        path: PathBuf,
    },
    ReadBlob {
        reference: String,
    },
    ReadAttachment {
        reference: String,
        path: PathBuf,
    },
    SweepBlobs,
    /// Fork a persisted task through one completed provider turn.
    ///
    /// This is intentionally a daemon-owned operation: provider-native
    /// conversation state, Git checkpoint refs, and SQLite all live on the
    /// daemon host and must move together for remote clients.
    ForkSessionFromResponse {
        turn_count: usize,
    },
    /// Restore a task and its provider conversation to immediately before a
    /// prior user message. The client can then submit the edited replacement
    /// as an ordinary new turn.
    RewindSessionToMessage {
        turn_count: usize,
    },
    ForkProviderSession {
        request: ProviderSessionForkRequest,
    },
    Workspace {
        operation: WorkspaceOperation,
    },
    CloseSession,
    GetDelegationGroup {
        group_id: Uuid,
    },
    ListSessionDelegations {
        session_id: Uuid,
    },
    ReadDelegatedTaskEvents {
        session_id: Uuid,
        group_id: Uuid,
        task_id: Uuid,
        after_sequence: u64,
    },
    CancelDelegation {
        group_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<Uuid>,
    },
    RetryDelegatedTask {
        group_id: Uuid,
        task_id: Uuid,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireDriverStartOptions {
    pub provider: String,
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub mode: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub context_window: Option<String>,
    pub agent_preset: Option<String>,
    pub computer_use_enabled: bool,
    pub provider_cursor: Option<Value>,
    #[serde(default)]
    pub execution_policy: crate::model::ExecutionPolicy,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireSessionOptions {
    pub mode: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub context_window: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireComputerToolRequest {
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireDriverEvent {
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
}

impl WireDriverEvent {
    pub fn new(kind: impl Into<String>, payload: Value) -> Self {
        Self {
            kind: kind.into(),
            payload,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SequencedEvent {
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    /// Changes whenever the daemon restarts, so a reused runtime id can begin
    /// again at sequence one without being mistaken for an old event.
    pub epoch: Uuid,
    pub sequence: u64,
    pub event: WireDriverEvent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    Hello {
        protocol_version: u32,
        daemon_version: String,
    },
    Rejected {
        message: String,
    },
    Response {
        request_id: Uuid,
        outcome: ResponseOutcome,
    },
    Event(SequencedEvent),
    /// The daemon-owned project/task catalog changed through another client.
    /// Clients should invalidate their lightweight task-state snapshot; live
    /// runtime events continue through [`Self::Event`].
    TaskStateChanged {
        revision: u64,
    },
    ShuttingDown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ResponseOutcome {
    Ok { payload: ResponsePayload },
    Error { error: RpcError },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ResponsePayload {
    Ack,
    SessionRuntime {
        runtime_id: Option<Uuid>,
        supports_steer: bool,
    },
    Started {
        supports_steer: bool,
    },
    OptionsApplied {
        applied: bool,
    },
    Cursor {
        cursor: Option<Value>,
    },
    Settings {
        settings: DaemonSettings,
    },
    Backups {
        backups: Vec<BackupInfo>,
    },
    BackupCreated {
        backup: BackupInfo,
    },
    AutomaticBackup {
        backup: Option<BackupInfo>,
        next_backup_at: u64,
    },
    RestoreStaged {
        backup: BackupInfo,
    },
    ProviderProbe {
        probe: ProviderProbe,
        version: Option<String>,
    },
    PlanUsage {
        usage: Option<PlanUsage>,
    },
    ComputerPermissions {
        permissions: ComputerPermissions,
    },
    SkillsCatalog {
        catalog: SkillsCatalog,
    },
    TaskState {
        projects: Vec<Project>,
        sessions: Vec<AgentSession>,
        default_cwd: PathBuf,
        projectless_root: Option<PathBuf>,
    },
    TaskStateSaved {
        sessions: Vec<AgentSession>,
    },
    Session {
        session: Option<AgentSession>,
    },
    SessionMessageMatches {
        matches: Vec<SessionMessageMatch>,
    },
    ProviderSessions {
        sessions: Vec<ProviderSessionSummary>,
    },
    ProviderSessionHistory {
        history: ProviderSessionHistory,
    },
    ComposerDrafts {
        drafts: ComposerDrafts,
    },
    BlobStored {
        reference: String,
        path: PathBuf,
    },
    AttachmentStored {
        attachment: StoredAttachment,
    },
    DelegationGroup {
        group: Option<crate::delegation::DelegationGroupInfo>,
    },
    SessionDelegations {
        groups: Vec<crate::delegation::DelegationGroupInfo>,
    },
    DelegatedTaskEvents {
        page: crate::delegation::DelegatedTaskEvents,
    },
    BlobData {
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    ProviderSessionForked {
        result: ProviderSessionFork,
    },
    SessionForked {
        session: AgentSession,
        checkpoint_warning: Option<String>,
    },
    SessionRewound {
        session: AgentSession,
        cleanup_warning: Option<String>,
    },
    Workspace {
        result: WorkspaceResult,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcError {
    pub message: String,
}

impl From<anyhow::Error> for RpcError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_payloads_use_base64_json_strings() {
        let payload = ResponsePayload::BlobData {
            bytes: vec![0, 1, 2, 255],
        };
        let json = serde_json::to_value(&payload).unwrap();

        assert_eq!(json["bytes"], "AAEC/w==");
        let ResponsePayload::BlobData { bytes } = serde_json::from_value(json).unwrap() else {
            panic!("unexpected payload variant");
        };
        assert_eq!(bytes, vec![0, 1, 2, 255]);
    }

    #[test]
    fn response_fork_command_uses_stable_camel_case_fields() {
        let json =
            serde_json::to_value(Command::ForkSessionFromResponse { turn_count: 7 }).unwrap();

        assert_eq!(json["type"], "forkSessionFromResponse");
        assert_eq!(json["turnCount"], 7);
    }

    #[test]
    fn message_rewind_command_uses_stable_camel_case_fields() {
        let json = serde_json::to_value(Command::RewindSessionToMessage { turn_count: 4 }).unwrap();

        assert_eq!(json["type"], "rewindSessionToMessage");
        assert_eq!(json["turnCount"], 4);
    }

    #[test]
    fn provider_session_commands_use_stable_wire_fields() {
        let list = serde_json::to_value(Command::ListProviderSessions {
            provider: ProviderKind::Codex,
            limit: 250,
        })
        .unwrap();
        assert_eq!(list["type"], "listProviderSessions");
        assert_eq!(list["provider"], "codex");
        assert_eq!(list["limit"], 250);

        let load = serde_json::to_value(Command::LoadProviderSession {
            cursor: ProviderResumeCursor::Codex {
                thread_id: "01900000-0000-7000-8000-000000000001".into(),
            },
            cwd: PathBuf::from("/tmp/project"),
        })
        .unwrap();
        assert_eq!(load["type"], "loadProviderSession");
        assert_eq!(load["cursor"]["provider"], "codex");
        assert_eq!(
            load["cursor"]["threadId"],
            "01900000-0000-7000-8000-000000000001"
        );
        assert_eq!(load["cwd"], "/tmp/project");
    }

    #[test]
    fn handshake_and_replay_field_names_are_stable() {
        let session_id = Uuid::nil();
        let runtime_id = Uuid::from_u128(1);
        let message = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            token: "secret".into(),
            resume_from: vec![ReplayCursor {
                session_id,
                runtime_id,
                epoch: Uuid::from_u128(3),
                sequence: 9,
            }],
        };
        let json = serde_json::to_value(message).unwrap();

        assert_eq!(json["type"], "hello");
        assert_eq!(json["resumeFrom"][0]["sessionId"], session_id.to_string());
        assert_eq!(json["resumeFrom"][0]["runtimeId"], runtime_id.to_string());
        assert_eq!(
            json["resumeFrom"][0]["epoch"],
            Uuid::from_u128(3).to_string()
        );
        assert!(json.get("protocol_version").is_none());
    }

    #[test]
    fn prompt_delegation_targets_survive_the_wire() {
        use crate::delegation::{ModelReference, PromptDelegationContext};
        use crate::model::ProviderKind;

        let mut target = ModelReference::new(
            "target-1",
            ProviderKind::OpenCode,
            "github-copilot/gemini-3.8-flash",
            "Gemini 3.8 Flash · High",
        );
        target.reasoning_effort = Some("high".into());
        target.sub_provider = Some("Github Copilot".into());

        let command = Command::Prompt {
            prompt: "review this diff".into(),
            delegation_context: Some(PromptDelegationContext {
                turn_id: Some("turn-1".into()),
                model_references: vec![target.clone()],
                review_source: None,
            }),
        };
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["type"], "prompt");
        assert_eq!(
            json["delegationContext"]["modelReferences"][0]["reasoningEffort"],
            "high"
        );
        assert_eq!(
            json["delegationContext"]["modelReferences"][0]["subProvider"],
            "Github Copilot"
        );

        let decoded: Command = serde_json::from_value(json).unwrap();
        let Command::Prompt {
            delegation_context: Some(context),
            ..
        } = decoded
        else {
            panic!("delegation context must round-trip");
        };
        assert_eq!(context.model_references, vec![target]);
    }

    #[test]
    fn a_plain_prompt_stays_backwards_compatible() {
        // Older clients omit the field entirely; the daemon must not reject
        // their prompts or invent a delegation context.
        let json = serde_json::json!({"type": "prompt", "prompt": "hi"});
        let decoded: Command = serde_json::from_value(json).unwrap();
        let Command::Prompt {
            delegation_context, ..
        } = decoded
        else {
            panic!("expected a prompt");
        };
        assert!(delegation_context.is_none());
    }

    #[test]
    fn composer_draft_changes_have_stable_wire_keys() {
        let project_id = Uuid::from_u128(7);
        let command = Command::ApplyComposerDraftChanges {
            changes: vec![ComposerDraftChange {
                target: crate::persistence::ComposerDraftTarget::NewSession { project_id },
                draft: Some(crate::persistence::ComposerDraft {
                    text: "unfinished".into(),
                    attachments: Vec::new(),
                    ..Default::default()
                }),
            }],
        };
        let json = serde_json::to_value(command).unwrap();

        assert_eq!(json["type"], "applyComposerDraftChanges");
        assert_eq!(json["changes"][0]["target"]["type"], "newSession");
        assert_eq!(
            json["changes"][0]["target"]["projectId"],
            project_id.to_string()
        );
        assert_eq!(json["changes"][0]["draft"]["text"], "unfinished");
    }
}
