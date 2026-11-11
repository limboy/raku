use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum CommandScope {
    Project,
    User,
    Skill,
    Builtin,
    Raku,
}

impl CommandScope {
    /// Presentation order in the composer picker. Resolution precedence is
    /// separate: project and user commands can still override less-specific
    /// commands with the same name.
    pub const fn display_rank(self) -> u8 {
        match self {
            Self::Builtin | Self::Raku => 0,
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
            Self::Raku => tr!("command_scope.raku"),
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

impl SlashCommand {
    /// App navigation and preferences live in the command palette. Custom
    /// prompts and skills may still use these names for conversation work.
    pub fn is_app_action(&self) -> bool {
        matches!(self.scope, CommandScope::Builtin | CommandScope::Raku)
            && matches!(
                self.name.as_str(),
                "model" | "models" | "settings" | "config" | "theme" | "resume"
            )
    }

    pub fn composer_group(&self) -> u8 {
        if self.scope == CommandScope::Skill {
            2
        } else if self.template.is_some() {
            1
        } else {
            0
        }
    }

    pub fn composer_group_label(&self) -> String {
        tr!(match self.composer_group() {
            2 => "composer.skills",
            1 => "composer.prompts",
            _ => "composer.commands",
        })
    }

    pub fn composer_badge(&self) -> String {
        tr!(if self.scope == CommandScope::Skill {
            "composer.skill"
        } else if self.template.is_some() {
            "composer.prompt"
        } else if self.scope == CommandScope::Raku
            || (self.scope == CommandScope::Builtin
                && matches!(self.name.as_str(), "compact" | "fast"))
        {
            "composer.runs_now"
        } else {
            "composer.command"
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileEntry {
    pub path: String,
    pub is_dir: bool,
}

// ── Prompt tokens ──────────────────────────────────────────────────────────
//
// A completion accepted in the composer is an atomic token, not text to be
// parsed again: it carries what was picked, so the transport never has to
// guess which reference was chosen or where a skill may appear.

/// What a [`PromptToken`] refers to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptTokenKind {
    /// A command or prompt template; only ever the first word of a prompt.
    Command,
    Skill,
    File,
    Agent,
}

/// An atomic span of a prompt and the item it stands for. `start..end` are
/// byte offsets into the text the user sees (`display_content`, else
/// `content`), which always prefixes what is sent.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct PromptToken {
    pub start: usize,
    pub end: usize,
    pub kind: PromptTokenKind,
    /// The command or skill name, file path, or agent name.
    pub name: String,
}

impl PromptToken {
    pub fn range(&self) -> std::ops::Range<usize> {
        self.start..self.end
    }

    /// Whether the token still describes `text`: in bounds, on character
    /// boundaries, and non-empty.
    pub fn fits(&self, text: &str) -> bool {
        self.start < self.end
            && self.end <= text.len()
            && text.is_char_boundary(self.start)
            && text.is_char_boundary(self.end)
    }

    /// Tokens shifted by `delta` bytes, keeping only those that still fit
    /// `text` — used when a prompt is trimmed or prefixed.
    pub fn rebased(tokens: &[PromptToken], text: &str, delta: isize) -> Vec<PromptToken> {
        tokens
            .iter()
            .filter_map(|token| {
                let start = token.start.checked_add_signed(delta)?;
                let end = token.end.checked_add_signed(delta)?;
                let token = PromptToken {
                    start,
                    end,
                    ..token.clone()
                };
                token.fits(text).then_some(token)
            })
            .collect()
    }

    /// Validate persisted or pasted spans before editing or slicing text.
    pub fn normalized(mut tokens: Vec<Self>, text: &str) -> Vec<Self> {
        tokens.retain(|token| token.fits(text) && !token.name.is_empty());
        tokens.sort_by_key(|token| token.start);
        let mut end = 0;
        tokens.retain(|token| {
            if token.start < end {
                return false;
            }
            end = token.end;
            true
        });
        tokens
    }
}

/// Unquoted word-start sigils eligible for an exact typed completion.
/// Shared by skill, file, and agent inference so code stays literal.
pub fn typed_prompt_ranges(prompt: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    let mut code_delimiter = 0;
    let mut tilde_fence = false;
    for word in prompt.split_inclusive(char::is_whitespace) {
        let start = offset;
        offset += word.len();
        let word = word.trim_end();
        if word.starts_with("~~~") {
            tilde_fence = !tilde_fence;
            continue;
        }
        if tilde_fence {
            continue;
        }
        let in_code = code_delimiter != 0 || word.contains('`');
        let mut ticks = word.match_indices('`').peekable();
        while let Some((index, _)) = ticks.next() {
            let mut count = 1;
            while ticks.peek().is_some_and(|(next, _)| *next == index + count) {
                ticks.next();
                count += 1;
            }
            if code_delimiter == 0 {
                code_delimiter = count;
            } else if code_delimiter == count {
                code_delimiter = 0;
            }
        }
        if !in_code && (word.starts_with('@') || (start == 0 && word.starts_with('/'))) {
            ranges.push(start..start + word.len());
        }
    }
    ranges
}

/// Preserve accepted identities and recognize exact, manually typed names.
/// Explicit file/agent tokens own their spans even if a skill shares a name.
pub fn prompt_tokens(
    prompt: &str,
    tokens: &[PromptToken],
    commands: &[SlashCommand],
) -> Vec<PromptToken> {
    let mut tokens = PromptToken::normalized(tokens.to_vec(), prompt);
    for range in typed_prompt_ranges(prompt) {
        if tokens
            .iter()
            .any(|token| token.start < range.end && range.start < token.end)
        {
            continue;
        }
        let word = &prompt[range.clone()];
        let name = &word[1..];
        let skill_only = !word.starts_with('/');
        if let Some(command) = commands.iter().find(|command| {
            command.name == name && (!skill_only || command.scope == CommandScope::Skill)
        }) {
            tokens.push(PromptToken {
                start: range.start,
                end: range.end,
                kind: if command.scope == CommandScope::Skill {
                    PromptTokenKind::Skill
                } else {
                    PromptTokenKind::Command
                },
                name: command.name.clone(),
            });
        }
    }
    PromptToken::normalized(tokens, prompt)
}

/// The prompt as `provider` should receive it, with every skill written in
/// that provider's native form. `None` when nothing changes.
///
/// Codex reads `$skill` anywhere. Claude (`/skill`) and Pi (`/skill:skill`)
/// only invoke a skill that opens the message, so the first skill becomes
/// that leading invocation with the whole request as its arguments; the rest
/// of the request names skills in backticks, and any further skills are
/// asked for on a closing line. A leading command keeps position zero, so
/// then every skill is asked for that way.
pub fn resolved_skill_submission(
    provider: crate::model::ProviderKind,
    prompt: &str,
    tokens: &[PromptToken],
    commands: &[SlashCommand],
) -> Option<String> {
    use crate::model::ProviderKind;

    let tokens = prompt_tokens(prompt, tokens, commands);
    let spans = tokens
        .iter()
        .filter(|token| token.kind == PromptTokenKind::Skill)
        .map(|token| (token.range(), token.name.clone()))
        .collect::<Vec<_>>();
    if spans.is_empty() {
        return None;
    }
    let rewrite = |inline: &dyn Fn(&str) -> String, skip: Option<usize>| {
        let mut text = String::with_capacity(prompt.len() + 16);
        let mut cursor = 0;
        for (index, (range, name)) in spans.iter().enumerate() {
            text.push_str(&prompt[cursor..range.start]);
            if Some(index) != skip {
                text.push_str(&inline(name));
            }
            cursor = range.end;
        }
        text.push_str(&prompt[cursor..]);
        text
    };
    let resolved = match provider {
        ProviderKind::Codex => rewrite(&|name| format!("${name}"), None),
        ProviderKind::Claude | ProviderKind::Pi => {
            let invoke = |name: &str| match provider {
                ProviderKind::Pi => format!("/skill:{name}"),
                _ => format!("/{name}"),
            };
            let backticked = |name: &str| format!("`{name}`");
            let command_leads = (prompt.starts_with('/') && spans[0].0.start != 0)
                || tokens
                    .iter()
                    .any(|token| token.start == 0 && token.kind == PromptTokenKind::Command);
            let (lead, rest) = if command_leads {
                (None, &spans[..])
            } else {
                (Some(&spans[0]), &spans[1..])
            };
            let mut text = match lead {
                // The lead skill opens the message; a span already there is
                // replaced in place, so its arguments read as before.
                Some((range, name)) if range.start == 0 => {
                    let body = rewrite(&backticked, Some(0));
                    format!("{}{body}", invoke(name))
                }
                Some((_, name)) => format!("{} {}", invoke(name), rewrite(&backticked, None)),
                None => rewrite(&backticked, None),
            };
            let mut extra = Vec::<&str>::new();
            for (_, name) in rest {
                if lead.is_none_or(|(_, lead)| lead != name) && !extra.contains(&name.as_str()) {
                    extra.push(name);
                }
            }
            if !extra.is_empty() {
                let list = extra
                    .iter()
                    .map(|name| backticked(name))
                    .collect::<Vec<_>>()
                    .join(", ");
                let noun = if extra.len() == 1 { "skill" } else { "skills" };
                text.push_str(&format!("\n\nAlso use the {list} {noun}."));
            }
            text
        }
    };
    (resolved != prompt).then_some(resolved)
}

/// Expand a prompt template without losing accepted identities in its arguments.
fn expand_template(
    template: &str,
    args: &str,
    tokens: &[PromptToken],
) -> (String, Vec<PromptToken>) {
    let mut text = String::with_capacity(template.len() + args.len());
    let mut expanded_tokens = Vec::new();
    let append = |text: &mut String, out: &mut Vec<PromptToken>, range: std::ops::Range<usize>| {
        let start = text.len();
        text.push_str(&args[range.clone()]);
        out.extend(
            tokens
                .iter()
                .filter(|token| token.start >= range.start && token.end <= range.end)
                .map(|token| PromptToken {
                    start: start + token.start - range.start,
                    end: start + token.end - range.start,
                    ..token.clone()
                }),
        );
    };
    let mut positional = Vec::new();
    let mut offset = 0;
    for word in args.split_inclusive(char::is_whitespace) {
        let end = offset + word.trim_end().len();
        if offset < end {
            positional.push(offset..end);
        }
        offset += word.len();
    }
    let mut consumed = false;
    let mut rest = template;
    while let Some(index) = rest.find('$') {
        text.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        if let Some(tail) = after
            .strip_prefix("ARGUMENTS")
            .or_else(|| after.strip_prefix('@'))
        {
            append(&mut text, &mut expanded_tokens, 0..args.len());
            consumed = true;
            rest = tail;
        } else if let Some(digit) = after
            .as_bytes()
            .first()
            .copied()
            .filter(|digit| (b'1'..=b'9').contains(digit))
        {
            if let Some(range) = positional.get((digit - b'1') as usize) {
                append(&mut text, &mut expanded_tokens, range.clone());
            }
            consumed = true;
            rest = &after[1..];
        } else {
            text.push('$');
            rest = after;
        }
    }
    text.push_str(rest);
    if !consumed && !args.is_empty() {
        text.push_str("\n\n");
        append(&mut text, &mut expanded_tokens, 0..args.len());
    }
    (text, expanded_tokens)
}

pub fn expand_command_template(template: &str, args: &str) -> String {
    expand_template(template, args, &[]).0
}

/// Resolve templates and skill identities together at the transport boundary.
pub fn resolved_submission(
    provider: crate::model::ProviderKind,
    prompt: &str,
    tokens: &[PromptToken],
    commands: &[SlashCommand],
) -> Option<String> {
    let tokens = prompt_tokens(prompt, tokens, commands);
    let template = tokens
        .first()
        .filter(|token| token.start == 0 && token.kind == PromptTokenKind::Command)
        .and_then(|token| {
            commands
                .iter()
                .find(|command| command.name == token.name)
                .and_then(|command| command.template.as_deref())
                .map(|template| (token.end, template))
        });
    let Some((end, template)) = template else {
        return resolved_skill_submission(provider, prompt, &tokens, commands);
    };
    let args = prompt[end..].trim();
    let args_start = prompt.len() - prompt[end..].trim_start().len();
    let args_tokens = PromptToken::rebased(&tokens, args, -(args_start as isize));
    let (mut expanded, mut expanded_tokens) = expand_template(template, args, &args_tokens);
    // Positional templates can omit arguments. An explicitly chosen skill
    // still applies to the request even when its argument was not interpolated.
    for token in tokens
        .iter()
        .filter(|token| token.kind == PromptTokenKind::Skill)
    {
        if !expanded_tokens
            .iter()
            .any(|expanded| expanded.kind == PromptTokenKind::Skill && expanded.name == token.name)
        {
            expanded.push_str("\n\n");
            let start = expanded.len();
            expanded.push('$');
            expanded.push_str(&token.name);
            expanded_tokens.push(PromptToken {
                start,
                end: expanded.len(),
                ..token.clone()
            });
        }
    }
    Some(
        resolved_skill_submission(provider, &expanded, &expanded_tokens, commands)
            .unwrap_or(expanded),
    )
}

// ── Trigger detection ──────────────────────────────────────────────────────
//
// Shared by the daemon and every client so the composer's `/` and `@`
// rules cannot drift apart.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriggerKind {
    /// `/name`: every command, prompt template and skill, grouped. Only the
    /// first word of the prompt.
    Command,
    /// `@name`: references — subagents, skills, then files. Any word start.
    File,
}

/// An autocompletion site under the caret: the token being typed, and the byte
/// range an accepted choice replaces (including the `/` or `@` sigil).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Trigger {
    pub kind: TriggerKind,
    pub query: String,
    pub range: std::ops::Range<usize>,
}

/// The trigger at `cursor`, if the text under it is one.
///
/// A slash command must be the first token of the prompt — `/` mid-sentence is
/// prose. `@` references start at any whitespace boundary, so `see @src/`
/// completes while `user@host` does not. Dollar text never opens suggestions.
pub fn detect_trigger(text: &str, cursor: usize) -> Option<Trigger> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let prefix = &text[..cursor];
    let command_start = prefix.len() - prefix.trim_start().len();
    if let Some(query) = prefix[command_start..].strip_prefix('/')
        && !query.chars().any(char::is_whitespace)
    {
        return Some(Trigger {
            kind: TriggerKind::Command,
            query: query.to_owned(),
            range: command_start..cursor,
        });
    }

    let token_start = text[..cursor]
        .rfind(char::is_whitespace)
        .map_or(0, |index| {
            index + text[index..].chars().next().unwrap().len_utf8()
        });
    let token = &text[token_start..cursor];
    let query = token.strip_prefix('@')?;
    Some(Trigger {
        kind: TriggerKind::File,
        query: query.to_owned(),
        range: token_start..cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str) -> SlashCommand {
        SlashCommand {
            name: name.to_owned(),
            description: String::new(),
            scope: CommandScope::Skill,
            argument_hint: None,
            template: None,
        }
    }

    fn token(text: &str, needle: &str, kind: PromptTokenKind, name: &str) -> PromptToken {
        let start = text.find(needle).expect("needle in text");
        PromptToken {
            start,
            end: start + needle.len(),
            kind,
            name: name.to_owned(),
        }
    }

    #[test]
    fn skill_tokens_take_each_providers_native_form_anywhere() {
        use crate::model::ProviderKind::{Claude, Codex, Pi};
        let commands = [skill("deploy"), skill("docs")];
        let resolve = |provider, prompt: &str, tokens: &[PromptToken]| {
            resolved_skill_submission(provider, prompt, tokens, &commands)
        };

        // Leading: the invocation replaces the token in place.
        let text = "/deploy staging";
        let lead = [token(text, "/deploy", PromptTokenKind::Skill, "deploy")];
        assert_eq!(resolve(Claude, text, &lead), None);
        assert_eq!(
            resolve(Pi, text, &lead).as_deref(),
            Some("/skill:deploy staging")
        );
        assert_eq!(
            resolve(Codex, text, &lead).as_deref(),
            Some("$deploy staging")
        );

        // Mid-message: Claude and Pi lead with the invocation and keep the
        // request as its arguments; Codex keeps it in place.
        let text = "ship it with @deploy now";
        let mid = [token(text, "@deploy", PromptTokenKind::Skill, "deploy")];
        assert_eq!(
            resolve(Claude, text, &mid).as_deref(),
            Some("/deploy ship it with `deploy` now")
        );
        assert_eq!(
            resolve(Pi, text, &mid).as_deref(),
            Some("/skill:deploy ship it with `deploy` now")
        );
        assert_eq!(
            resolve(Codex, text, &mid).as_deref(),
            Some("ship it with $deploy now")
        );

        // A second skill is asked for on a closing line.
        let text = "use $deploy then @docs";
        let two = [
            token(text, "$deploy", PromptTokenKind::Skill, "deploy"),
            token(text, "@docs", PromptTokenKind::Skill, "docs"),
        ];
        assert_eq!(
            resolve(Claude, text, &two).as_deref(),
            Some("/deploy use `deploy` then `docs`\n\nAlso use the `docs` skill.")
        );

        // A leading command keeps position zero.
        let text = "/review with $deploy";
        let tokens = [
            token(text, "/review", PromptTokenKind::Command, "review"),
            token(text, "$deploy", PromptTokenKind::Skill, "deploy"),
        ];
        assert_eq!(
            resolve(Claude, text, &tokens).as_deref(),
            Some("/review with `deploy`\n\nAlso use the `deploy` skill.")
        );
    }

    #[test]
    fn typed_skill_names_resolve_only_when_exact() {
        use crate::model::ProviderKind::{Claude, Codex, Pi};
        let commands = [skill("deploy")];
        let resolve =
            |provider, prompt: &str| resolved_skill_submission(provider, prompt, &[], &commands);
        // Leading invocation and inline reference spellings.
        assert_eq!(
            resolve(Claude, "@deploy now").as_deref(),
            Some("/deploy now")
        );
        assert_eq!(
            resolve(Codex, "/deploy now").as_deref(),
            Some("$deploy now")
        );
        assert_eq!(resolve(Pi, "/deploy").as_deref(), Some("/skill:deploy"));
        // Prose stays prose.
        assert_eq!(resolve(Claude, "echo $HOME"), None);
        assert_eq!(resolve(Claude, "costs $5 to /deploy"), None);
        assert_eq!(resolve(Claude, "$deployer"), None);
    }

    #[test]
    fn rebased_tokens_drop_what_no_longer_fits() {
        let text = "  $deploy now";
        let tokens = [token(text, "$deploy", PromptTokenKind::Skill, "deploy")];
        let trimmed = text.trim();
        let rebased = PromptToken::rebased(&tokens, trimmed, -2);
        assert_eq!(rebased[0].range(), 0..7);
        assert!(PromptToken::rebased(&tokens, "x", -2).is_empty());
    }

    #[test]
    fn explicit_identities_override_inference_and_survive_catalog_refreshes() {
        use crate::model::ProviderKind::{Claude, Codex};
        let text = "use @deploy";
        let file = token(text, "@deploy", PromptTokenKind::File, "deploy");
        assert_eq!(
            resolved_skill_submission(Claude, text, &[file], &[skill("deploy")]),
            None
        );
        let selected = token(text, "@deploy", PromptTokenKind::Skill, "plugin:deploy");
        assert_eq!(
            resolved_skill_submission(Codex, text, &[selected], &[]).as_deref(),
            Some("use $plugin:deploy")
        );
        assert_eq!(
            resolved_skill_submission(Claude, text, &[], &[skill("deploy")]).as_deref(),
            Some("/deploy use `deploy`")
        );
    }

    #[test]
    fn templates_expand_and_preserve_selected_skills_and_files() {
        use crate::model::ProviderKind::{Claude, Codex, Pi};
        let mut fix = skill("fix");
        fix.scope = CommandScope::Project;
        fix.template = Some("Fix: $ARGUMENTS".into());
        let commands = [fix, skill("deploy")];
        let text = "/fix tests with @deploy";
        let tokens = [token(text, "@deploy", PromptTokenKind::Skill, "deploy")];
        for (provider, expected) in [
            (Codex, "Fix: tests with $deploy"),
            (Claude, "/deploy Fix: tests with `deploy`"),
            (Pi, "/skill:deploy Fix: tests with `deploy`"),
        ] {
            assert_eq!(
                resolved_submission(provider, text, &tokens, &commands).as_deref(),
                Some(expected)
            );
        }
        let file = token(text, "@deploy", PromptTokenKind::File, "deploy");
        assert_eq!(
            resolved_submission(Claude, text, &[file], &commands).as_deref(),
            Some("Fix: tests with @deploy")
        );
        let mut commands = commands;
        commands[0].template = Some("$2 then $1".into());
        let text = "/fix @deploy tests";
        let selected = token(text, "@deploy", PromptTokenKind::Skill, "deploy");
        assert_eq!(
            resolved_submission(Codex, text, &[selected], &commands).as_deref(),
            Some("tests then $deploy")
        );
    }

    #[test]
    fn literal_code_and_unknown_commands_keep_their_meaning() {
        use crate::model::ProviderKind::Claude;
        let commands = [skill("deploy")];
        for text in [
            "`use @deploy`",
            "```sh\nuse $deploy\n```",
            "costs $5",
            "email a@deploy",
        ] {
            assert!(prompt_tokens(text, &[], &commands).is_empty(), "{text}");
        }
        assert_eq!(
            resolved_skill_submission(Claude, "/unknown with @deploy", &[], &commands).as_deref(),
            Some("/unknown with `deploy`\n\nAlso use the `deploy` skill.")
        );
    }

    #[test]
    fn invalid_and_overlapping_tokens_never_slice_invalid_text() {
        use crate::model::ProviderKind::Claude;
        let text = "部署 @deploy";
        let valid = token(text, "@deploy", PromptTokenKind::Skill, "deploy");
        let mut invalid = valid.clone();
        invalid.start = 1;
        let mut overlap = valid.clone();
        overlap.start += 1;
        let tokens = [invalid, overlap, valid];
        assert_eq!(prompt_tokens(text, &tokens, &[]).len(), 1);
        assert_eq!(
            resolved_skill_submission(Claude, text, &tokens, &[]).as_deref(),
            Some("/deploy 部署 `deploy`")
        );
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
        // `^` no longer opens anything.
        assert!(detect_trigger("review with ^gem", 16).is_none());
    }

    #[test]
    fn slash_invokes_at_the_start_and_references_work_in_command_arguments() {
        assert!(detect_trigger("explain this\n/review", 20).is_none());
        let text = "/review with @deploy";
        let trigger = detect_trigger(text, text.len()).unwrap();
        assert_eq!(trigger.kind, TriggerKind::File);
        assert_eq!(trigger.range, 13..20);
        let text = "/review with $deploy";
        assert!(detect_trigger(text, text.len()).is_none());
    }

    #[test]
    fn dollar_text_neither_opens_suggestions_nor_becomes_a_typed_token() {
        let commands = [skill("deploy")];
        for text in [
            "$",
            "$deploy",
            "use $dep",
            "$plugin:sk",
            "a$b",
            "$deploy now",
            "costs $5",
            "$1",
            "部署 $dep",
        ] {
            assert!(detect_trigger(text, text.len()).is_none(), "{text}");
            assert!(prompt_tokens(text, &[], &commands).is_empty(), "{text}");
            for provider in crate::model::ProviderKind::ALL {
                assert_eq!(
                    resolved_skill_submission(provider, text, &[], &commands),
                    None
                );
            }
        }
        // Never split a multi-byte character.
        let text = "部署 @dep";
        assert_eq!(
            detect_trigger(text, text.len()).unwrap().range,
            "部署 ".len()..text.len()
        );
    }
}
