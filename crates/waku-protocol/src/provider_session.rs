use std::collections::HashMap;
use std::path::PathBuf;

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
    /// OpenCode sessions carry their own `location`, so there is no server
    /// working directory to fork against; `binary` only lets the cold path
    /// reach the adopted background service.
    #[serde(alias = "openCode2")]
    OpenCode {
        binary: PathBuf,
        session_id: String,
        turn_count: usize,
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
