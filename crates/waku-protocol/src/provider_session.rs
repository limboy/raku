use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::model::ProviderResumeCursor;

/// Daemon-host native-session operation used when no live driver can fork.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "provider", rename_all = "camelCase")]
pub enum ProviderSessionForkRequest {
    Antigravity {
        session_id: String,
        turn_count: usize,
        title: String,
    },
    Claude {
        session_id: String,
        resume_at: Option<String>,
        turn_count: usize,
        title: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSessionFork {
    pub cursor: ProviderResumeCursor,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub message_ids: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_resume_at: Option<String>,
}
