//! Shared wire protocol for cross-model delegation and subtask execution.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::ProviderKind;
use crate::workspace::ReviewDiffSource;

/// A pinned model destination selected by the user for delegation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelReference {
    /// Stable target reference ID within the current turn (e.g. "target-1" or a UUID).
    pub reference_id: String,
    pub provider: ProviderKind,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_preset: Option<String>,
    /// The calling channel behind a provider, so two same-named models stay distinguishable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_provider: Option<String>,
    pub display_label: String,
    /// The unambiguous inline spelling chosen from the model catalogue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_label: Option<String>,
    /// The configured sub-agent this target was picked as, if any. Two agents
    /// may share a model route yet stay distinct targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<Uuid>,
    /// Standing instructions from the sub-agent's configuration, snapshotted
    /// when the agent was picked so a later settings edit cannot rewrite a
    /// turn that already ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_instructions: Option<String>,
}

impl ModelReference {
    pub fn new(
        reference_id: impl Into<String>,
        provider: ProviderKind,
        model_id: impl Into<String>,
        display_label: impl Into<String>,
    ) -> Self {
        Self {
            reference_id: reference_id.into(),
            provider,
            model_id: model_id.into(),
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
            agent_preset: None,
            sub_provider: None,
            display_label: display_label.into(),
            marker_label: None,
            agent_id: None,
            agent_instructions: None,
        }
    }

    /// Whether two references address the same destination: one route at one
    /// reasoning level, picked as the same sub-agent (or as none).
    pub fn same_target(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.model_id == other.model_id
            && self.sub_provider == other.sub_provider
            && self.reasoning_effort == other.reasoning_effort
            && self.agent_id == other.agent_id
    }
}

/// A user-configured delegation target: a named model route with optional
/// standing instructions, offered by the composer's `^` picker.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubAgent {
    pub id: Uuid,
    /// The inline marker spelling, `^[name]`. Unique, case-insensitively,
    /// across the configured agents.
    pub name: String,
    pub provider: ProviderKind,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instructions: String,
}

/// Why a sub-agent name cannot be used as an inline `^[name]` marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubAgentNameError {
    Empty,
    /// Brackets and line breaks would end or split the marker.
    InvalidCharacter,
    Duplicate,
}

/// Validate `name` for an agent, ignoring the agent being edited (`editing`).
pub fn validate_sub_agent_name(
    name: &str,
    agents: &[SubAgent],
    editing: Option<Uuid>,
) -> Result<(), SubAgentNameError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(SubAgentNameError::Empty);
    }
    if name.contains(['[', ']', '\n', '\r']) {
        return Err(SubAgentNameError::InvalidCharacter);
    }
    if agents
        .iter()
        .any(|agent| Some(agent.id) != editing && agent.name.trim().eq_ignore_ascii_case(name))
    {
        return Err(SubAgentNameError::Duplicate);
    }
    Ok(())
}

/// Delegation context attached to a prompt, specifying available targets and code diff source.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptDelegationContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub model_references: Vec<ModelReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_source: Option<ReviewDiffSource>,
}

/// Specification for one subtask in a batch delegation request.
/// Agent-tool envelopes use snake_case, matching their MCP and Pi schemas.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DelegationTaskSpec {
    /// Nonempty key unique across the entire batch, including different targets.
    /// The response's `task_ids` map is keyed by these exact values.
    pub task_key: String,
    pub target_ref: String,
    pub instructions: String,
}

/// Parameters for the `delegate_batch` tool.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DelegationBatchRequest {
    /// Optional idempotency key. Models do omit it despite the schema; the
    /// daemon then derives one from the turn and the tasks, so an identical
    /// retry still reuses its group instead of failing the whole batch.
    #[serde(default)]
    pub request_key: String,
    pub tasks: Vec<DelegationTaskSpec>,
}

/// Response returned by the `delegate_batch` tool.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DelegationBatchResponse {
    pub group_id: Uuid,
    pub task_ids: HashMap<String, Uuid>,
    pub context_id: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DelegationGroupStatus {
    Preparing,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl DelegationGroupStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DelegatedTaskStatus {
    Queued,
    Starting,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    Interrupted,
}

impl DelegatedTaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Interrupted
        )
    }
}

/// Summary of a single delegated task.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegatedTaskSummary {
    pub task_id: Uuid,
    /// Changes on every explicit retry; clients must discard an older replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<Uuid>,
    pub task_key: String,
    pub target: ModelReference,
    pub status: DelegatedTaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_activity: Option<String>,
}

/// Response returned by the `wait_delegates` tool.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WaitDelegatesResponse {
    pub group_id: Uuid,
    pub status: DelegationGroupStatus,
    pub tasks: Vec<DelegatedTaskSummary>,
}

/// Full projection of a delegation group for UI rendering.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationGroupInfo {
    pub id: Uuid,
    pub parent_session_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_turn_id: Option<String>,
    pub status: DelegationGroupStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_source: Option<ReviewDiffSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_summary: Option<String>,
    pub tasks: Vec<DelegatedTaskSummary>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// One persisted child event. `sequence` is an opaque, increasing storage
/// cursor, including across retries of the same delegated task.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegatedTaskEvent {
    pub sequence: u64,
    /// Daemon receipt time from the persisted event, at storage precision.
    pub created_at_ms: u64,
    pub event: crate::WireDriverEvent,
}

/// A bounded, ordered page of a delegated task's original provider events.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegatedTaskEvents {
    pub events: Vec<DelegatedTaskEvent>,
    pub next_sequence: u64,
    pub has_more: bool,
}
