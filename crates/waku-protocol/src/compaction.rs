//! Provider-owned compaction lifecycle. Summaries and token counts are optional.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;

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

    /// Snapcompact's summary is an instruction envelope for the model; the
    /// archived transcript lives separately in the native provider session.
    /// Keep that envelope in storage, but never present it as a readable recap.
    /// Recognizing it here also covers snapshots saved by older Waku versions.
    pub fn summary_for_display(&self) -> Option<Cow<'_, str>> {
        let summary = self.summary.as_deref()?;
        if summary
            .starts_with("Resume prior conversation. Earlier turns archived under HISTORY below,")
        {
            Some(Cow::Owned(tr!("compaction.history_archived")))
        } else {
            Some(Cow::Borrowed(summary))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompactionRequest {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompactionCapabilities {
    pub manual: bool,
    pub instructions: bool,
    pub lifecycle: bool,
    pub summary: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archived_history_envelopes_are_hidden_including_restored_snapshots() {
        let raw = "Resume prior conversation. Earlier turns archived under HISTORY below, oldest→newest.\n\nArchived transcript scopes:\n- `¶user:`, `¶think:`, `¶ai:`, `¶call:`\n\nHISTORY\n===================";
        let mut item = ContextCompaction::new("archive", None, 0);
        item.summary = Some(raw.into());
        let restored: ContextCompaction =
            serde_json::from_str(&serde_json::to_string(&item).unwrap()).unwrap();
        let display = restored.summary_for_display().unwrap();
        assert!(!display.contains("HISTORY"));
        assert!(!display.contains("¶"));
        assert_eq!(restored.summary.as_deref(), Some(raw));

        item.summary = Some("## Remaining work\nKeep the API stable.".into());
        assert_eq!(
            item.summary_for_display().as_deref(),
            item.summary.as_deref()
        );
    }
}
