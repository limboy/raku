use serde::{Deserialize, Serialize};

use crate::model::{ProviderKind, ProviderModel, ProviderModelOption};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum CommandScope {
    Project,
    User,
    Skill,
    Builtin,
    Waku,
}

impl CommandScope {
    /// Presentation order in the composer picker. Resolution precedence is
    /// separate: project and user commands can still override less-specific
    /// commands with the same name.
    pub const fn display_rank(self) -> u8 {
        match self {
            Self::Builtin | Self::Waku => 0,
            Self::Project => 1,
            Self::User => 2,
            Self::Skill => 3,
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Project => tr!("command_scope.project"),
            Self::User => tr!("command_scope.user"),
            Self::Skill => tr!("command_scope.skill"),
            Self::Builtin => tr!("command_scope.builtin"),
            Self::Waku => tr!("command_scope.waku"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    pub scope: CommandScope,
    pub argument_hint: Option<String>,
    pub template: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileEntry {
    pub path: String,
    pub is_dir: bool,
}

// ── Trigger detection ──────────────────────────────────────────────────────
//
// Shared by the daemon and every client so the composer's `/`, `@` and `^`
// rules cannot drift apart.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriggerKind {
    Command,
    File,
    Model,
}

/// An autocompletion site under the caret: the token being typed, and the byte
/// range an accepted choice replaces (including the `/`, `@` or `^` sigil).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Trigger {
    pub kind: TriggerKind,
    pub query: String,
    pub range: std::ops::Range<usize>,
}

/// The trigger at `cursor`, if the text under it is one.
///
/// A slash command must be the first token of its line — `/` mid-sentence is
/// prose. `@` mentions and `^` model references start at any whitespace
/// boundary, so `see @src/` and `ask ^gemini` still complete, while
/// `user@host` and `x^2` do not: their sigil is inside a token.
pub fn detect_trigger(text: &str, cursor: usize) -> Option<Trigger> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let line_start = text[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let line_prefix = &text[line_start..cursor];
    if let Some(query) = line_prefix.strip_prefix('/') {
        if !query.chars().any(char::is_whitespace) {
            return Some(Trigger {
                kind: TriggerKind::Command,
                query: query.to_owned(),
                range: line_start..cursor,
            });
        }
        return None;
    }

    let token_start = text[..cursor]
        .rfind(char::is_whitespace)
        .map_or(0, |index| {
            index + text[index..].chars().next().unwrap().len_utf8()
        });
    let token = &text[token_start..cursor];
    if let Some(query) = token.strip_prefix('@') {
        return Some(Trigger {
            kind: TriggerKind::File,
            query: query.to_owned(),
            range: token_start..cursor,
        });
    }
    let query = token.strip_prefix('^')?;
    Some(Trigger {
        kind: TriggerKind::Model,
        query: query.to_owned(),
        range: token_start..cursor,
    })
}

// ── Delegation model candidates ─────────────────────────────────────────────

/// One selectable delegation target: a provider model at one reasoning level.
///
/// Enumerating (model × reasoning) pairs is what makes `^Gemini … high`
/// expressible without inventing a second picker step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCandidate {
    pub provider: ProviderKind,
    pub model_id: String,
    pub model_name: String,
    pub sub_provider: Option<String>,
    pub reasoning: Option<ProviderModelOption>,
    pub marker_label: String,
}

impl ModelCandidate {
    /// Text the fuzzy filter searches: what the user would type to mean it.
    pub fn searchable(&self) -> String {
        let mut text = format!(
            "{} {} {}",
            self.model_name,
            self.model_id,
            self.provider.short_name()
        );
        if let Some(sub_provider) = self.sub_provider.as_deref() {
            text.push(' ');
            text.push_str(sub_provider);
        }
        if let Some(reasoning) = self.reasoning.as_ref() {
            text.push(' ');
            text.push_str(&reasoning.label);
        }
        text.to_ascii_lowercase()
    }

    pub fn label(&self) -> String {
        match self.reasoning.as_ref() {
            Some(reasoning) => format!("{} · {}", self.model_name, reasoning.label),
            None => self.model_name.clone(),
        }
    }

    /// The structured reference a delegated subtask is addressed by.
    pub fn reference(&self, reference_id: impl Into<String>) -> crate::delegation::ModelReference {
        let mut reference = crate::delegation::ModelReference::new(
            reference_id,
            self.provider,
            self.model_id.clone(),
            self.label(),
        );
        reference.reasoning_effort = self
            .reasoning
            .as_ref()
            .map(|reasoning| reasoning.id.clone());
        reference.sub_provider = self.sub_provider.clone();
        reference.marker_label = Some(self.marker_label.clone());
        reference
    }
}

/// Expand a provider catalogue into delegation candidates.
///
/// A model with reasoning options contributes one candidate per option, plus
/// one at the provider default when it does not name it.
pub fn model_candidates(models: &[(ProviderKind, ProviderModel)]) -> Vec<ModelCandidate> {
    let mut candidates = Vec::new();
    for (provider, model) in models {
        if model.reasoning_efforts.is_empty() {
            candidates.push(ModelCandidate {
                provider: *provider,
                model_id: model.id.clone(),
                model_name: model.name.clone(),
                sub_provider: model.sub_provider.clone(),
                reasoning: None,
                marker_label: String::new(),
            });
            continue;
        }
        for option in &model.reasoning_efforts {
            candidates.push(ModelCandidate {
                provider: *provider,
                model_id: model.id.clone(),
                model_name: model.name.clone(),
                sub_provider: model.sub_provider.clone(),
                reasoning: Some(option.clone()),
                marker_label: String::new(),
            });
        }
    }
    // Only collisions need extra text. Include the route first, and the exact
    // model ID if a route itself exposes two models with the same name.
    let labels = candidates
        .iter()
        .map(ModelCandidate::label)
        .collect::<Vec<_>>();
    let counts = labels
        .iter()
        .fold(std::collections::HashMap::new(), |mut counts, label| {
            *counts.entry(label.as_str()).or_insert(0usize) += 1;
            counts
        });
    for (index, candidate) in candidates.iter_mut().enumerate() {
        candidate.marker_label = if counts[labels[index].as_str()] > 1 {
            match candidate.sub_provider.as_deref() {
                Some(channel) => format!(
                    "{} · {} / {channel}",
                    labels[index],
                    candidate.provider.short_name()
                ),
                None => format!("{} · {}", labels[index], candidate.provider.short_name()),
            }
        } else {
            labels[index].clone()
        };
    }
    let labels = candidates
        .iter()
        .map(|candidate| candidate.marker_label.clone())
        .collect::<Vec<_>>();
    let counts = labels
        .iter()
        .fold(std::collections::HashMap::new(), |mut counts, label| {
            *counts.entry(label.as_str()).or_insert(0usize) += 1;
            counts
        });
    for (index, candidate) in candidates.iter_mut().enumerate() {
        if counts[labels[index].as_str()] > 1 {
            candidate.marker_label = format!("{} · {}", labels[index], candidate.model_id);
        }
    }
    candidates
}

/// Replace an accepted `^` token with a readable inline marker. Selected
/// references retain the structured identity behind this display label.
pub fn model_reference_text(label: &str) -> String {
    format!("^[{label}] ")
}

/// Resolve only complete, non-nested `^[label]` markers present in `text`.
/// A matching selected reference takes precedence over the current catalogue,
/// preserving its provider, model, sub-provider and reasoning identity.
pub fn extract_model_references(
    text: &str,
    candidates: &[ModelCandidate],
    existing: &[crate::delegation::ModelReference],
) -> Vec<crate::delegation::ModelReference> {
    let mut known = existing.to_vec();
    let mut result = Vec::new();
    for (_, label) in model_reference_markers(text) {
        if let Ok(references) =
            resolve_model_references(&model_reference_text(label), candidates, &known)
        {
            for reference in references {
                if !result.contains(&reference) {
                    if !known.contains(&reference) {
                        known.push(reference.clone());
                    }
                    result.push(reference);
                }
            }
        }
    }
    result
}

/// An unresolved marker is reported to the composer instead of silently
/// changing a user's pinned provider or dropping a requested target.
pub fn resolve_model_references(
    text: &str,
    candidates: &[ModelCandidate],
    existing: &[crate::delegation::ModelReference],
) -> Result<Vec<crate::delegation::ModelReference>, String> {
    let mut result: Vec<crate::delegation::ModelReference> = Vec::new();
    for (_, label) in model_reference_markers(text) {
        let selected = existing
            .iter()
            .filter(|reference| {
                reference
                    .marker_label
                    .as_deref()
                    .unwrap_or(&reference.display_label)
                    .eq_ignore_ascii_case(label)
                    || reference.model_id.eq_ignore_ascii_case(label)
            })
            .collect::<Vec<_>>();
        if selected.len() > 1 {
            return Err(label.to_owned());
        }
        if let Some(selected) = selected.first() {
            if !result.iter().any(|target| {
                target.provider == selected.provider
                    && target.model_id == selected.model_id
                    && target.sub_provider == selected.sub_provider
                    && target.reasoning_effort == selected.reasoning_effort
            }) {
                result.push((*selected).clone());
            }
            continue;
        }
        let exact = candidates
            .iter()
            .filter(|candidate| candidate.marker_label.eq_ignore_ascii_case(label))
            .collect::<Vec<_>>();
        let matches = if exact.is_empty() {
            candidates
                .iter()
                .filter(|candidate| {
                    candidate.label().eq_ignore_ascii_case(label)
                        || candidate.model_name.eq_ignore_ascii_case(label)
                        || candidate.model_id.eq_ignore_ascii_case(label)
                })
                .collect::<Vec<_>>()
        } else {
            exact
        };
        let Some(candidate) = matches.first().copied() else {
            return Err(label.to_owned());
        };
        if matches.iter().any(|other| {
            other.provider != candidate.provider
                || other.model_id != candidate.model_id
                || other.sub_provider != candidate.sub_provider
        }) {
            return Err(label.to_owned());
        }
        let reference = {
            if result.iter().any(|target| {
                target.provider == candidate.provider
                    && target.model_id == candidate.model_id
                    && target.sub_provider == candidate.sub_provider
                    && target.reasoning_effort.as_deref()
                        == candidate
                            .reasoning
                            .as_ref()
                            .map(|reasoning| reasoning.id.as_str())
            }) {
                continue;
            }
            if let Some(selected) = existing.iter().find(|target| {
                target.provider == candidate.provider
                    && target.model_id == candidate.model_id
                    && target.sub_provider == candidate.sub_provider
                    && target.reasoning_effort.as_deref()
                        == candidate
                            .reasoning
                            .as_ref()
                            .map(|reasoning| reasoning.id.as_str())
            }) {
                result.push(selected.clone());
                continue;
            }
            let mut suffix = 1;
            let reference_id = loop {
                let id = format!("target-{suffix}");
                if !existing
                    .iter()
                    .chain(&result)
                    .any(|reference| reference.reference_id == id)
                {
                    break id;
                }
                suffix += 1;
            };
            candidate.reference(reference_id)
        };
        result.push(reference);
    }
    Ok(result)
}

/// The complete model marker immediately before a byte-offset caret.
/// This is a grammar match, not catalogue validation: pasted and restored
/// markers remain atomic even when their provider is currently unavailable.
pub fn model_reference_range_before_caret(
    text: &str,
    cursor: usize,
) -> Option<std::ops::Range<usize>> {
    if !text.is_char_boundary(cursor) || text.get(..cursor)?.as_bytes().last() != Some(&b']') {
        return None;
    }
    model_reference_markers(text).find_map(|(range, _)| (range.end == cursor).then_some(range))
}

fn model_reference_markers(text: &str) -> impl Iterator<Item = (std::ops::Range<usize>, &str)> {
    let mut offset = 0;
    std::iter::from_fn(move || {
        while let Some(relative_start) = text[offset..].find("^[") {
            let start = offset + relative_start;
            let label_start = start + 2;
            let mut depth = 1;
            let mut nested = false;
            let mut closing = None;
            for (relative_index, byte) in text.as_bytes()[label_start..].iter().enumerate() {
                match byte {
                    b'[' => {
                        depth += 1;
                        nested = true;
                    }
                    b']' => {
                        depth -= 1;
                        if depth == 0 {
                            closing = Some(label_start + relative_index);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(end) = closing else {
                offset = text.len();
                return None;
            };
            offset = end + 1;
            let raw_label = &text[label_start..end];
            let label = raw_label.trim();
            if !nested && !label.is_empty() && !raw_label.contains(['\n', '\r']) {
                return Some((start..offset, label));
            }
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, name: &str, efforts: &[(&str, &str)]) -> ProviderModel {
        let mut model = ProviderModel::new(id, name);
        model.reasoning_efforts = efforts
            .iter()
            .map(|(id, label)| ProviderModelOption::new(*id, *label))
            .collect();
        model
    }

    #[test]
    fn caret_opens_a_model_trigger_only_at_a_token_start() {
        let trigger = detect_trigger("review with ^gem", 16).expect("caret opens the popup");
        assert_eq!(trigger.kind, TriggerKind::Model);
        assert_eq!(trigger.query, "gem");
        assert_eq!(trigger.range, 12..16);

        // A caret that is part of an expression is not a reference.
        assert!(detect_trigger("x^2", 3).is_none());
        assert!(detect_trigger("set n = a^b", 11).is_none());
        // Nor is one in the middle of a word.
        assert!(detect_trigger("foo^bar", 7).is_none());
    }

    #[test]
    fn existing_triggers_keep_their_rules() {
        assert_eq!(
            detect_trigger("/res", 4).unwrap().kind,
            TriggerKind::Command
        );
        assert_eq!(
            detect_trigger("see @src/", 9).unwrap().kind,
            TriggerKind::File
        );
        assert!(detect_trigger("user@host", 9).is_none());
        assert!(detect_trigger("/com", 3).is_some());
    }

    #[test]
    fn caret_trigger_respects_utf8_boundaries() {
        // Never split a multi-byte character.
        assert!(detect_trigger("评审^gem", 1).is_none());
        let trigger = detect_trigger("评审 ^gem", "评审 ^gem".len()).unwrap();
        assert_eq!(trigger.query, "gem");
        assert_eq!(trigger.range, "评审 ".len().."评审 ^gem".len());
    }

    #[test]
    fn reasoning_levels_become_separate_candidates() {
        let models = vec![(
            ProviderKind::Codex,
            model(
                "github-copilot/gemini-3.8-flash",
                "Gemini 3.8 Flash",
                &[("high", "High"), ("low", "Low")],
            ),
        )];
        let candidates = model_candidates(&models);
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0].searchable().contains("high"));

        let reference = candidates[0].reference("target-1");
        assert_eq!(reference.model_id, "github-copilot/gemini-3.8-flash");
        assert_eq!(reference.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(reference.display_label, "Gemini 3.8 Flash · High");
        assert_ne!(
            reference.reasoning_effort,
            candidates[1].reference("target-2").reasoning_effort
        );
    }

    #[test]
    fn models_without_reasoning_levels_yield_one_candidate() {
        let models = vec![(
            ProviderKind::Claude,
            model("claude-opus-5", "Claude Opus 5", &[]),
        )];
        let candidates = model_candidates(&models);
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].reasoning.is_none());
        assert_eq!(candidates[0].label(), "Claude Opus 5");
    }

    #[test]
    fn accepted_reference_text_stays_readable() {
        assert_eq!(
            model_reference_text("Gemini 3.8 Flash · High"),
            "^[Gemini 3.8 Flash · High] "
        );
    }

    #[test]
    fn complete_markers_match_at_the_closing_bracket_only() {
        let text = "评审 ^[模型 · High] suffix";
        let start = "评审 ".len();
        let end = text.find(']').unwrap() + 1;
        assert_eq!(
            model_reference_range_before_caret(text, end),
            Some(start..end)
        );
        assert!(model_reference_range_before_caret(text, end - 1).is_none());
        assert!(model_reference_range_before_caret(text, end + 1).is_none());
        assert!(model_reference_range_before_caret(text, 1).is_none());
        assert!(model_reference_range_before_caret(text, text.len() + 1).is_none());
        // Grammar, rather than availability in a changing model catalogue,
        // determines atomic editing. Matching is not whitespace-boundary-only.
        assert_eq!(
            model_reference_range_before_caret("x^[Unknown]", 11),
            Some(1..11)
        );
    }

    #[test]
    fn malformed_or_nested_markers_are_not_atoms_or_targets() {
        let candidates = model_candidates(&[(ProviderKind::Codex, model("model", "Model", &[]))]);
        for text in [
            "ordinary ]",
            "^[",
            "^[Model",
            "^[]",
            "^[   ]",
            "^[Model\n]",
            "^[Model\r]",
            "^[outer ^[Model]]",
            "^[outer ^[Model]",
            "^[outer [text] ^[Model]]",
        ] {
            assert!(
                model_reference_range_before_caret(text, text.len()).is_none(),
                "{text:?}"
            );
            assert!(
                extract_model_references(text, &candidates, &[]).is_empty(),
                "{text:?}"
            );
        }
        let text = "^[outer ^[Model]] ^[Model]";
        assert_eq!(extract_model_references(text, &candidates, &[]).len(), 1);
        assert_eq!(
            model_reference_range_before_caret(text, text.len()),
            Some("^[outer ^[Model]] ".len()..text.len())
        );
    }

    #[test]
    fn deleted_and_edited_markers_cannot_keep_selected_targets_alive() {
        let candidates = model_candidates(&[(
            ProviderKind::Codex,
            model("model", "Model", &[("high", "High")]),
        )]);
        let selected = candidates[0].reference("chosen");
        for text in [
            "plain text",
            "^[Model · High",
            "^[Different]",
            "^[Model · Low]",
        ] {
            assert!(extract_model_references(text, &candidates, &[selected.clone()]).is_empty());
        }
        let marker = model_reference_text(&selected.display_label);
        let duplicated = format!("{marker}{marker}");
        assert_eq!(
            extract_model_references(&duplicated, &candidates, &[selected.clone()]),
            vec![selected.clone()]
        );
        // One surviving copy still binds; removing both removes the target.
        assert_eq!(
            extract_model_references(&marker, &[], &[selected.clone()]),
            vec![selected]
        );
        assert!(extract_model_references("", &candidates, &[]).is_empty());
    }

    #[test]
    fn restored_selected_identity_wins_over_catalogue_label_collisions() {
        let candidates =
            model_candidates(&[(ProviderKind::Codex, model("new-model", "Model", &[]))]);
        let mut selected = crate::delegation::ModelReference::new(
            "chosen",
            ProviderKind::Claude,
            "provider/old-model",
            "Model",
        );
        selected.sub_provider = Some("pinned-provider".into());
        selected.reasoning_effort = Some("high".into());
        assert_eq!(
            extract_model_references("^[model]", &candidates, &[selected.clone()]),
            vec![selected.clone()]
        );
    }

    #[test]
    fn pasted_and_text_restored_markers_resolve_aliases_without_duplicate_targets() {
        let candidates = model_candidates(&[(
            ProviderKind::Codex,
            model("model-id", "Model", &[("high", "High")]),
        )]);
        let text = "^[ Model · High ] ^[model-id] ^[MODEL]";
        let references = extract_model_references(text, &candidates, &[]);
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].model_id, "model-id");
        assert_eq!(references[0].reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            extract_model_references(text, &candidates, &references),
            references
        );
    }

    #[test]
    fn retained_reference_ids_cannot_collide_with_new_pasted_targets() {
        let candidates = model_candidates(&[
            (ProviderKind::Codex, model("first", "First", &[])),
            (ProviderKind::Codex, model("second", "Second", &[])),
        ]);
        let selected = candidates[0].reference("target-2");
        let references =
            extract_model_references("^[First] ^[Second]", &candidates, &[selected.clone()]);
        assert_eq!(references.len(), 2);
        assert_eq!(references[0], selected);
        assert_ne!(references[0].reference_id, references[1].reference_id);
    }

    #[test]
    fn same_named_routes_survive_draft_restore_and_independent_deletion() {
        let candidates = model_candidates(&[
            (ProviderKind::Codex, model("a", "Shared", &[])),
            (ProviderKind::Claude, model("channel/b", "Shared", &[])),
        ]);
        let first = candidates[0].reference("first");
        let second = candidates[1].reference("second");
        assert_ne!(first.marker_label, second.marker_label);
        let marker = model_reference_text(second.marker_label.as_deref().unwrap());
        assert_eq!(
            resolve_model_references(&marker, &candidates, &[first, second.clone()]).unwrap(),
            vec![second.clone()]
        );

        let draft = crate::persistence::ComposerDraft {
            text: marker,
            model_references: vec![second.clone()],
            ..Default::default()
        };
        let saved = serde_json::to_string(&draft).unwrap();
        let restored: crate::persistence::ComposerDraft = serde_json::from_str(&saved).unwrap();
        // The selected model still resolves with a cold or changed catalogue.
        assert_eq!(
            resolve_model_references(&restored.text, &[], &restored.model_references).unwrap(),
            vec![second]
        );
        assert!(resolve_model_references("^[Shared]", &candidates, &[]).is_err());
    }

    #[test]
    fn an_unresolved_marker_cannot_erase_other_draft_identities() {
        let candidates = model_candidates(&[(ProviderKind::Codex, model("a", "Model", &[]))]);
        let selected = candidates[0].reference("selected");
        assert_eq!(
            extract_model_references("^[Unknown] ^[Model]", &[], &[selected.clone()]),
            vec![selected]
        );
        let legacy: crate::persistence::ComposerDraft =
            serde_json::from_str(r#"{"text":"old draft"}"#).unwrap();
        assert!(legacy.model_references.is_empty());
    }
}
