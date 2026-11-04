use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{AgentSession, MessageRole};

// Persisted provider tags (including OpenCode's old beta alias) must stay
// recognizable after their variants are removed from ProviderKind.
const RETIRED_PROVIDERS: &[&str] = &[
    "deepSeek",
    "ohMyPi",
    "openCode",
    "openCode2",
    "antigravity",
    "agy",
];

fn is_retired_provider(value: &serde_json::Value) -> bool {
    value
        .as_str()
        .is_some_and(|provider| RETIRED_PROVIDERS.contains(&provider))
}

/// Read persisted data, removing references to retired providers
/// only when the ordinary typed decode fails. Active data keeps the direct
/// deserialization path; malformed or otherwise unsupported data still fails.
pub fn from_persisted_json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
    let error = match serde_json::from_slice(bytes) {
        Ok(value) => return Ok(value),
        Err(error) if error.is_data() => error,
        Err(error) => return Err(error),
    };
    let mut value = serde_json::from_slice(bytes)?;
    if !migrate_retired_provider_references(&mut value) {
        return Err(error);
    }
    serde_json::from_value(value)
}

/// Drop retired provider selections without changing another provider's
/// settings or reassigning its sessions. Raw session rows remain untouched.
pub fn migrate_retired_provider_references(value: &mut serde_json::Value) -> bool {
    let mut changed = false;
    match value {
        serde_json::Value::Object(fields) => {
            if fields.get("last_provider").is_some_and(is_retired_provider) {
                for key in [
                    "last_provider",
                    "last_model",
                    "last_reasoning_effort",
                    "last_service_tier",
                    "last_context_window",
                ] {
                    fields.remove(key);
                }
                changed = true;
            }
            for (key, value) in fields {
                match key.as_str() {
                    "disabled_providers" => {
                        if let Some(providers) = value.as_array_mut() {
                            let previous = providers.len();
                            providers.retain(|provider| !is_retired_provider(provider));
                            changed |= providers.len() != previous;
                        }
                    }
                    "provider_binary_overrides" => {
                        if let Some(overrides) = value.as_object_mut() {
                            for provider in RETIRED_PROVIDERS {
                                changed |= overrides.remove(*provider).is_some();
                            }
                        }
                    }
                    "project_model_selections" => {
                        if let Some(selections) = value.as_object_mut() {
                            let previous = selections.len();
                            selections.retain(|_, selection| {
                                !selection.get("provider").is_some_and(is_retired_provider)
                            });
                            changed |= selections.len() != previous;
                        }
                    }
                    "favorite_models"
                    | "remembered_model_traits"
                    | "model_references"
                    | "modelReferences"
                    | "sub_agents" => {
                        if let Some(entries) = value.as_array_mut() {
                            let previous = entries.len();
                            entries.retain(|entry| {
                                !entry.get("provider").is_some_and(is_retired_provider)
                            });
                            changed |= entries.len() != previous;
                        }
                    }
                    _ => {}
                }
                changed |= migrate_retired_provider_references(value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                changed |= migrate_retired_provider_references(value);
            }
        }
        _ => {}
    }
    changed
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComposerDraftAttachment {
    pub path: PathBuf,
    pub mention: String,
    pub name: String,
    pub is_dir: bool,
    pub is_image: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pasted_text: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComposerDraft {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ComposerDraftAttachment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_references: Vec<crate::delegation::ModelReference>,
}

impl ComposerDraft {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.attachments.is_empty()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComposerDrafts {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub new_sessions: HashMap<Uuid, ComposerDraft>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub sessions: HashMap<Uuid, ComposerDraft>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComposerDraftKey {
    NewSession(Uuid),
    Session(Uuid),
}

/// Wire-safe identity for one independently persisted composer draft.
///
/// Draft updates are keyed so multiple connected clients cannot overwrite
/// unrelated drafts by sending stale whole-file snapshots.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ComposerDraftTarget {
    NewSession { project_id: Uuid },
    Session { session_id: Uuid },
}

impl From<ComposerDraftKey> for ComposerDraftTarget {
    fn from(key: ComposerDraftKey) -> Self {
        match key {
            ComposerDraftKey::NewSession(project_id) => Self::NewSession { project_id },
            ComposerDraftKey::Session(session_id) => Self::Session { session_id },
        }
    }
}

impl From<ComposerDraftTarget> for ComposerDraftKey {
    fn from(target: ComposerDraftTarget) -> Self {
        match target {
            ComposerDraftTarget::NewSession { project_id } => Self::NewSession(project_id),
            ComposerDraftTarget::Session { session_id } => Self::Session(session_id),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComposerDraftChange {
    pub target: ComposerDraftTarget,
    /// `None` removes the target. Empty drafts are normalized to removal too.
    pub draft: Option<ComposerDraft>,
}

impl ComposerDraftKey {
    pub fn for_session(session: &AgentSession) -> Self {
        if session.has_started() {
            Self::Session(session.id)
        } else {
            Self::NewSession(session.project_id)
        }
    }
}

impl ComposerDrafts {
    pub fn get_for(&self, session: &AgentSession) -> Option<&ComposerDraft> {
        self.get(ComposerDraftKey::for_session(session))
    }

    pub fn get(&self, key: ComposerDraftKey) -> Option<&ComposerDraft> {
        match key {
            ComposerDraftKey::NewSession(project_id) => self.new_sessions.get(&project_id),
            ComposerDraftKey::Session(session_id) => self.sessions.get(&session_id),
        }
    }

    pub fn set(&mut self, key: ComposerDraftKey, draft: ComposerDraft) -> bool {
        let (drafts, id) = match key {
            ComposerDraftKey::NewSession(project_id) => (&mut self.new_sessions, project_id),
            ComposerDraftKey::Session(session_id) => (&mut self.sessions, session_id),
        };
        if draft.is_empty() {
            drafts.remove(&id).is_some()
        } else if drafts.get(&id) == Some(&draft) {
            false
        } else {
            drafts.insert(id, draft);
            true
        }
    }

    pub fn remove(&mut self, key: ComposerDraftKey) -> bool {
        match key {
            ComposerDraftKey::NewSession(project_id) => {
                self.new_sessions.remove(&project_id).is_some()
            }
            ComposerDraftKey::Session(session_id) => self.sessions.remove(&session_id).is_some(),
        }
    }

    /// Add one attachment to a draft's staging row, creating the draft when it
    /// does not exist yet. Returns whether the stored draft changed — an
    /// attachment-only draft counts as content, so it is kept rather than
    /// normalized away.
    pub fn append_attachment(
        &mut self,
        key: ComposerDraftKey,
        attachment: ComposerDraftAttachment,
    ) -> bool {
        let mut draft = self.get(key).cloned().unwrap_or_default();
        draft.attachments.push(attachment);
        self.set(key, draft)
    }

    pub fn move_to_empty(
        &mut self,
        source: ComposerDraftKey,
        destination: ComposerDraftKey,
    ) -> bool {
        if source == destination || self.get(destination).is_some_and(|draft| !draft.is_empty()) {
            return false;
        }
        let Some(draft) = self.get(source).cloned() else {
            return false;
        };
        self.remove(source);
        self.set(destination, draft)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionMessageMatch {
    pub session_id: Uuid,
    pub source: MessageRole,
    pub snippet: String,
}

#[cfg(test)]
mod tests {
    use super::{ComposerDraftAttachment, ComposerDraftKey, ComposerDrafts};
    use uuid::Uuid;

    #[test]
    fn retired_model_references_do_not_discard_the_rest_of_a_draft() {
        let project_id = Uuid::new_v4();
        let document = serde_json::to_vec(&serde_json::json!({
            "new_sessions": {
                project_id.to_string(): {
                    "text": "Keep this unfinished prompt",
                    "model_references": [
                        {"referenceId": "retired", "provider": "deepSeek",
                         "modelId": "deepseek-chat", "displayLabel": "Harness"},
                        {"referenceId": "omp", "provider": "ohMyPi",
                         "modelId": "retired", "displayLabel": "Oh My Pi"},
                        {"referenceId": "opencode", "provider": "openCode",
                         "modelId": "retired", "displayLabel": "OpenCode"},
                        {"referenceId": "opencode2", "provider": "openCode2",
                         "modelId": "retired", "displayLabel": "OpenCode 2"},
                        {"referenceId": "antigravity", "provider": "antigravity",
                         "modelId": "retired", "displayLabel": "Antigravity"},
                        {"referenceId": "agy", "provider": "agy",
                         "modelId": "retired", "displayLabel": "Antigravity alias"},
                        {"referenceId": "active", "provider": "pi",
                         "modelId": "deepseek/deepseek-flash", "displayLabel": "DeepSeek"}
                    ]
                }
            }
        }))
        .unwrap();
        let drafts: ComposerDrafts = super::from_persisted_json(&document).unwrap();
        let draft = drafts
            .get(ComposerDraftKey::NewSession(project_id))
            .unwrap();
        assert_eq!(draft.text, "Keep this unfinished prompt");
        assert_eq!(
            draft.model_references,
            vec![crate::delegation::ModelReference::new(
                "active",
                crate::model::ProviderKind::Pi,
                "deepseek/deepseek-flash",
                "DeepSeek",
            )]
        );
        let saved = serde_json::to_vec(&drafts).unwrap();
        assert_eq!(
            serde_json::from_slice::<ComposerDrafts>(&saved).unwrap(),
            drafts
        );
        assert!(
            super::from_persisted_json::<crate::settings::DaemonSettings>(
                br#"{"disabled_providers":["not-a-provider","deepSeek"]}"#
            )
            .is_err()
        );
    }

    fn snippet(text: &str) -> ComposerDraftAttachment {
        ComposerDraftAttachment {
            path: "/attachments/snippet.txt".into(),
            mention: "/attachments/snippet.txt".into(),
            name: "Pasted text".into(),
            is_dir: false,
            is_image: false,
            blob_reference: Some("waku-attachment:snippet".into()),
            pasted_text: Some(text.into()),
        }
    }

    #[test]
    fn an_appended_attachment_creates_its_draft_and_keeps_earlier_content() {
        let mut drafts = ComposerDrafts::default();
        let key = ComposerDraftKey::Session(Uuid::new_v4());

        // A snippet that lands after the user navigated away has no draft to
        // join yet; it must still be kept rather than normalized away.
        assert!(drafts.append_attachment(key, snippet("first")));
        let draft = drafts.get(key).expect("the attachment keeps the draft");
        assert!(draft.text.is_empty());
        assert_eq!(draft.attachments, vec![snippet("first")]);

        drafts.set(
            key,
            super::ComposerDraft {
                text: "follow up".into(),
                attachments: vec![snippet("first")],
                ..Default::default()
            },
        );
        assert!(drafts.append_attachment(key, snippet("second")));
        let draft = drafts.get(key).expect("the draft stays");
        assert_eq!(draft.text, "follow up");
        assert_eq!(draft.attachments, vec![snippet("first"), snippet("second")]);
    }
}
