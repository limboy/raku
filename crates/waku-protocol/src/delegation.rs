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
    /// The calling channel behind a provider, e.g. `GitHub Copilot` for an
    /// OpenCode model, so two same-named models stay distinguishable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_provider: Option<String>,
    pub display_label: String,
    /// The unambiguous inline spelling chosen from the model catalogue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_label: Option<String>,
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
        }
    }
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
