//! Provider-owned compaction lifecycle. Summaries and token counts are optional.

use crate::model::ProviderKind;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CompactionStatus {
    Running,
    Completed,
    /// The command returned successfully without a native compaction boundary.
    Unconfirmed,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CompactionTrigger {
    Auto,
    Manual,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ContextCompaction {
    pub id: String,
    pub status: CompactionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<CompactionTrigger>,
    /// Zero means the start was not observed, for example an imported boundary.
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_tokens: Option<u64>,
    #[serde(default)]
    pub after_tokens_estimated: bool,
}

impl ContextCompaction {
    pub fn new(id: impl Into<String>, trigger: Option<CompactionTrigger>, now: u64) -> Self {
        Self {
            id: id.into(),
            status: CompactionStatus::Running,
            trigger,
            started_at_ms: now,
            completed_at_ms: None,
            summary: None,
            error: None,
            notice: None,
            before_tokens: None,
            after_tokens: None,
            after_tokens_estimated: false,
        }
    }
    pub fn finish(&mut self, status: CompactionStatus, now: u64) {
        self.status = status;
        self.completed_at_ms = Some(now);
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompactionRequest {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CompactionCapabilities {
    pub manual: bool,
    pub instructions: bool,
    pub lifecycle: bool,
    pub summary: bool,
}

impl ProviderKind {
    pub fn compaction_capabilities(self) -> CompactionCapabilities {
        match self {
            Self::Antigravity | Self::Codex | Self::OpenCode => CompactionCapabilities {
                manual: true,
                instructions: false,
                lifecycle: true,
                summary: self == Self::OpenCode,
            },
            Self::Pi | Self::OhMyPi | Self::Claude => CompactionCapabilities {
                manual: true,
                instructions: true,
                lifecycle: true,
                summary: self != Self::Claude,
            },
            _ => CompactionCapabilities::default(),
        }
    }
}
