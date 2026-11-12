//! Pure client-side composer matching over daemon-provided command/file lists.

use std::ops::Range;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
pub use raku_protocol::composer::{
    CommandScope, FileEntry, PromptToken, PromptTokenKind, SlashCommand, Trigger, TriggerKind,
    detect_trigger, resolved_skill_submission,
};
use raku_protocol::model::{ProviderKind, ProviderModelOption, ReportedCommand, SubagentToolAgent};

pub const FILTER_CAP: usize = 64;
pub const FILE_INDEX_CAP: usize = 50_000;

pub fn merge_reported_commands(
    discovered: &[SlashCommand],
    reported: &[ReportedCommand],
) -> Vec<SlashCommand> {
    let mut merged = discovered.to_vec();
    for report in reported {
        if let Some(known) = merged
            .iter_mut()
            .find(|command| command.name == report.name)
        {
            if known.description.is_empty() {
                known.description = report.description.clone();
            }
        } else {
            merged.push(SlashCommand {
                name: report.name.clone(),
                description: report.description.clone(),
                scope: CommandScope::Builtin,
                argument_hint: None,
                template: None,
            });
        }
    }
    merged
        .sort_by(|a, b| (a.scope.display_rank(), &a.name).cmp(&(b.scope.display_rank(), &b.name)));
    merged
}

/// The composer spelling of an autocomplete entry accepted under `sigil`:
/// `/name` from the `/` list, or `@name` for a referenced skill.
pub fn command_composer_text(command: &SlashCommand, sigil: char) -> String {
    format!("{sigil}{}", command.name)
}

/// What kind of token accepting `command` inserts.
pub fn command_token_kind(command: &SlashCommand) -> PromptTokenKind {
    if command.scope == CommandScope::Skill {
        PromptTokenKind::Skill
    } else {
        PromptTokenKind::Command
    }
}

/// Exact typed references become tokens at submission too. Files take
/// precedence over inferred skills; a selected token always keeps its kind.
pub fn reference_tokens(
    prompt: &str,
    tokens: &[PromptToken],
    commands: &[SlashCommand],
    files: &[FileEntry],
    agents: &[SubagentToolAgent],
) -> Vec<PromptToken> {
    let mut tokens = PromptToken::normalized(tokens.to_vec(), prompt);
    for range in raku_protocol::composer::typed_prompt_ranges(prompt) {
        let Some(name) = prompt[range.clone()].strip_prefix('@') else {
            continue;
        };
        if tokens
            .iter()
            .any(|token| token.start < range.end && range.start < token.end)
        {
            continue;
        }
        let kind = if files.iter().any(|file| file.path == name) {
            PromptTokenKind::File
        } else if agents.iter().any(|agent| agent.name == name) {
            PromptTokenKind::Agent
        } else {
            continue;
        };
        tokens.push(PromptToken {
            start: range.start,
            end: range.end,
            kind,
            name: name.to_owned(),
        });
    }
    prompt_tokens(prompt, &tokens, commands)
}

/// Whether the composer submitted Raku's global terminal-session picker.
/// The command is reserved by daemon-side discovery, so it is intentionally
/// provider-neutral and never crosses into a provider transport.
pub fn is_resume_submission(prompt: &str) -> bool {
    prompt.trim() == "/resume"
}

/// Where the prompt of a `/plan <prompt>` submission starts, as a byte offset
/// into `prompt`. A bare `/plan` only toggles plan mode, so it has no prompt
/// and returns `None`, as does any text that merely begins with `/plan`.
pub fn plan_mode_prompt_start(prompt: &str) -> Option<usize> {
    let leading = prompt.len() - prompt.trim_start().len();
    let after = prompt[leading..].strip_prefix("/plan")?;
    if !after.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = after.trim_start();
    (!rest.trim_end().is_empty()).then(|| prompt.len() - rest.len())
}

/// Whether the submitted text resolves to Codex's native fast-mode command,
/// which Raku bridges to the provider's service-tier control. Checking the
/// resolved entry preserves project/user command precedence when one of them
/// intentionally owns `/fast`.
pub fn is_fast_mode_toggle_submission(
    provider: ProviderKind,
    prompt: &str,
    commands: &[SlashCommand],
) -> bool {
    provider == ProviderKind::Codex
        && prompt.trim() == "/fast"
        && commands.iter().any(|command| {
            command.name == "fast"
                && command.scope == CommandScope::Builtin
                && command.template.is_none()
        })
}

/// Resolve the next concrete service-tier ID for Codex's Fast toggle. Model
/// metadata may expose the Fast tier as `fast` or as `priority`; the display
/// label is the stable product vocabulary, while the ID is provider-owned.
pub fn toggled_fast_service_tier(
    current: Option<&str>,
    service_tiers: &[ProviderModelOption],
) -> Option<String> {
    let fast = service_tiers.iter().find(|tier| {
        matches!(tier.id.as_str(), "fast" | "priority") || tier.label.eq_ignore_ascii_case("fast")
    })?;
    Some(if current == Some(fast.id.as_str()) {
        "default".to_owned()
    } else {
        fast.id.clone()
    })
}

/// A `/goal` composer submission parsed into its intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalCommand {
    /// Bare `/goal` — show the current goal (and offer to create one).
    Show,
    Edit,
    Pause,
    Resume,
    Clear,
    /// `/goal <objective>` — start or replace the goal with this objective.
    Set(String),
}

/// Parse the submitted text as Codex's native `/goal` command, which Raku
/// bridges to `thread/goal/*`. `None` when it is not one — wrong provider,
/// other text, or a project/user command that deliberately owns `/goal`
/// (resolution precedence stands).
pub fn parse_goal_submission(
    provider: ProviderKind,
    prompt: &str,
    commands: &[SlashCommand],
) -> Option<GoalCommand> {
    if !provider.capabilities().goals {
        return None;
    }
    let invocation = prompt.trim().strip_prefix('/')?;
    let (name, arguments) = invocation
        .split_once(char::is_whitespace)
        .map_or((invocation, ""), |(name, arguments)| {
            (name, arguments.trim())
        });
    if name != "goal" {
        return None;
    }
    let goal_is_codex_builtin = commands.iter().any(|command| {
        command.name == "goal"
            && command.scope == CommandScope::Builtin
            && command.template.is_none()
    });
    if !goal_is_codex_builtin {
        return None;
    }
    Some(match arguments {
        "" => GoalCommand::Show,
        "edit" => GoalCommand::Edit,
        "pause" => GoalCommand::Pause,
        "resume" => GoalCommand::Resume,
        "clear" => GoalCommand::Clear,
        objective => GoalCommand::Set(objective.to_owned()),
    })
}

pub use raku_protocol::composer::{expand_command_template, prompt_tokens, resolved_submission};

pub fn matcher() -> Matcher {
    Matcher::new(nucleo_matcher::Config::DEFAULT.match_paths())
}

#[derive(Clone, Debug)]
pub struct Scored<T> {
    pub item: T,
    pub positions: Vec<u32>,
}

fn filter_scored(
    haystack: &[&str],
    query: &str,
    matcher: &mut Matcher,
    cap: usize,
) -> Vec<(usize, Vec<u32>)> {
    if query.trim().is_empty() {
        return (0..haystack.len().min(cap))
            .map(|index| (index, Vec::new()))
            .collect();
    }
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored = Vec::new();
    for (index, text) in haystack.iter().enumerate() {
        if let Some(score) = pattern.score(Utf32Str::new(text, &mut buf), matcher) {
            scored.push((score, index));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(cap);
    scored
        .into_iter()
        .map(|(_, index)| {
            let mut positions = Vec::new();
            pattern.indices(
                Utf32Str::new(haystack[index], &mut buf),
                matcher,
                &mut positions,
            );
            positions.sort_unstable();
            positions.dedup();
            (index, positions)
        })
        .collect()
}

/// The `/` list: every command, prompt template and skill, in that order
/// with no query — the catalogue is sorted by scope — and by fuzzy score
/// with one. Each row's badge names its kind, so one list stays legible.
pub fn filter_commands(
    commands: &[SlashCommand],
    query: &str,
    matcher: &mut Matcher,
) -> Vec<Scored<SlashCommand>> {
    let mut rows = Vec::new();
    // Bound each section separately so a large command catalog cannot hide
    // every skill or prompt from the unfiltered list.
    for group in 0..3 {
        let entries = commands
            .iter()
            .filter(|command| !command.is_app_action() && command.composer_group() == group)
            .collect::<Vec<_>>();
        let names = entries
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>();
        rows.extend(
            filter_scored(&names, query, matcher, FILTER_CAP / 3)
                .into_iter()
                .map(|(index, positions)| Scored {
                    item: entries[index].clone(),
                    positions,
                }),
        );
    }
    rows
}

/// The skills `@` offers: names containing `query`, case-insensitively.
///
/// Substring rather than fuzzy matching keeps skills from crowding out the
/// files a path query is after.
pub fn filter_skills(commands: &[SlashCommand], query: &str) -> Vec<Scored<SlashCommand>> {
    let needle = query.to_lowercase();
    let mut matches = commands
        .iter()
        .filter(|command| command.scope == CommandScope::Skill)
        .filter_map(|command| {
            let haystack = command.name.to_lowercase();
            let start = haystack.find(&needle)?;
            // Positions are char indices into the original name. Lowercasing
            // can change a name's length; highlight nothing rather than the
            // wrong characters when it does.
            let positions = if haystack.chars().count() == command.name.chars().count() {
                let first = haystack[..start].chars().count() as u32;
                (first..first + needle.chars().count() as u32).collect()
            } else {
                Vec::new()
            };
            Some((
                start,
                Scored {
                    item: command.clone(),
                    positions,
                },
            ))
        })
        .collect::<Vec<_>>();
    // Names that start with the query lead; the stable sort keeps catalogue
    // order otherwise.
    matches.sort_by_key(|(start, _)| *start != 0);
    matches
        .into_iter()
        .take(FILTER_CAP)
        .map(|(_, scored)| scored)
        .collect()
}

pub fn filter_files(
    files: &[FileEntry],
    query: &str,
    matcher: &mut Matcher,
) -> Vec<Scored<FileEntry>> {
    let paths = files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    filter_scored(&paths, query, matcher, FILTER_CAP)
        .into_iter()
        .map(|(index, positions)| Scored {
            item: files[index].clone(),
            positions,
        })
        .collect()
}

/// The `@` agent list: enabled subagents matched by name, placed ahead of
/// workspace files when the task's provider supports delegation.
pub fn filter_subagents(
    agents: &[SubagentToolAgent],
    query: &str,
    matcher: &mut Matcher,
) -> Vec<Scored<SubagentToolAgent>> {
    let names = agents
        .iter()
        .map(|agent| agent.name.as_str())
        .collect::<Vec<_>>();
    filter_scored(&names, query, matcher, FILTER_CAP)
        .into_iter()
        .map(|(index, positions)| Scored {
            item: agents[index].clone(),
            positions,
        })
        .collect()
}

pub fn highlight_byte_ranges(
    text: &str,
    positions: &[u32],
    char_offset: usize,
) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (char_index, (byte_index, character)) in (char_offset..).zip(text.char_indices()) {
        if positions.binary_search(&(char_index as u32)).is_ok() {
            let byte_end = byte_index + character.len_utf8();
            match ranges.last_mut() {
                Some(last) if last.end == byte_index => last.end = byte_end,
                _ => ranges.push(byte_index..byte_end),
            }
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_command_picker_puts_builtins_first_and_skills_last() {
        let discovered = vec![
            command("deploy", CommandScope::Skill),
            command("format", CommandScope::User),
            command("lint", CommandScope::Project),
            command("review", CommandScope::Builtin),
            command("resume", CommandScope::Raku),
        ];
        let reported = vec![ReportedCommand {
            name: "compact".into(),
            description: "Free up context".into(),
        }];

        let merged = merge_reported_commands(&discovered, &reported);
        assert_eq!(
            merged
                .iter()
                .map(|command| (command.scope, command.name.as_str()))
                .collect::<Vec<_>>(),
            [
                (CommandScope::Builtin, "compact"),
                (CommandScope::Raku, "resume"),
                (CommandScope::Builtin, "review"),
                (CommandScope::Project, "lint"),
                (CommandScope::User, "format"),
                (CommandScope::Skill, "deploy"),
            ]
        );
    }

    #[test]
    fn fast_toggle_is_codex_only_and_respects_command_overrides() {
        let builtin = command("fast", CommandScope::Builtin);
        assert!(is_fast_mode_toggle_submission(
            ProviderKind::Codex,
            "/fast ",
            std::slice::from_ref(&builtin),
        ));
        assert!(!is_fast_mode_toggle_submission(
            ProviderKind::Claude,
            "/fast",
            std::slice::from_ref(&builtin),
        ));
        assert!(!is_fast_mode_toggle_submission(
            ProviderKind::Codex,
            "/fast now",
            std::slice::from_ref(&builtin),
        ));
        assert!(!is_fast_mode_toggle_submission(
            ProviderKind::Codex,
            "/fast",
            &[command("fast", CommandScope::Project)],
        ));
    }

    #[test]
    fn resume_is_an_exact_provider_neutral_local_command() {
        assert!(is_resume_submission("/resume"));
        assert!(is_resume_submission("  /resume  "));
        assert!(!is_resume_submission("/resume latest"));
        assert!(!is_resume_submission("please /resume"));
    }

    #[test]
    fn plan_prefix_yields_the_prompt_that_follows_it() {
        let prompt = "  /plan  add a context breakdown\n";
        let start = plan_mode_prompt_start(prompt).unwrap();
        assert_eq!(&prompt[start..], "add a context breakdown\n");
        assert_eq!(plan_mode_prompt_start("/plan\nfix it"), Some(6));
        assert_eq!(plan_mode_prompt_start("/plan"), None);
        assert_eq!(plan_mode_prompt_start("/plan   "), None);
        assert_eq!(plan_mode_prompt_start("/planner do it"), None);
        assert_eq!(plan_mode_prompt_start("please /plan it"), None);
    }

    #[test]
    fn fast_toggle_uses_the_models_concrete_service_tier_id() {
        let tiers = [ProviderModelOption::new("priority", "Fast")];
        assert_eq!(
            toggled_fast_service_tier(Some("default"), &tiers).as_deref(),
            Some("priority")
        );
        assert_eq!(
            toggled_fast_service_tier(Some("priority"), &tiers).as_deref(),
            Some("default")
        );
        assert_eq!(toggled_fast_service_tier(None, &[]), None);
    }

    #[test]
    fn accepted_entries_keep_the_sigil_they_were_picked_under() {
        let skill = command("mattpocock-skills:to-spec", CommandScope::Skill);
        assert_eq!(
            command_composer_text(&skill, '/'),
            "/mattpocock-skills:to-spec"
        );
        assert_eq!(
            command_composer_text(&skill, '@'),
            "@mattpocock-skills:to-spec"
        );
        assert_eq!(command_token_kind(&skill), PromptTokenKind::Skill);
        let fast = command("fast", CommandScope::Builtin);
        assert_eq!(command_composer_text(&fast, '/'), "/fast");
        assert_eq!(command_token_kind(&fast), PromptTokenKind::Command);
    }

    #[test]
    fn slash_lists_everything_and_skill_lists_match_by_substring() {
        let commands = [
            command("review", CommandScope::Builtin),
            command("deploy", CommandScope::Skill),
            command("redeploy-docs", CommandScope::Skill),
        ];
        let mut matcher = matcher();
        let names = |rows: Vec<Scored<SlashCommand>>| {
            rows.into_iter()
                .map(|row| row.item.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(filter_commands(&commands, "", &mut matcher)),
            ["review", "deploy", "redeploy-docs"]
        );
        assert_eq!(
            names(filter_skills(&commands, "")),
            ["deploy", "redeploy-docs"]
        );
        // A prefix match leads an inner one; case does not matter.
        assert_eq!(
            names(filter_skills(&commands, "DEPLOY")),
            ["deploy", "redeploy-docs"]
        );
        let docs = filter_skills(&commands, "docs");
        assert_eq!(docs[0].positions, [9, 10, 11, 12]);
        // Scattered letters are not a match for a file-oriented query.
        assert!(filter_skills(&commands, "dpy").is_empty());
    }

    #[test]
    fn typed_references_disambiguate_files_and_selected_skills() {
        let commands = [command("deploy", CommandScope::Skill)];
        let files = [FileEntry {
            path: "deploy".into(),
            is_dir: false,
        }];
        let text = "use @deploy";
        let inferred = reference_tokens(text, &[], &commands, &files, &[]);
        assert_eq!(inferred[0].kind, PromptTokenKind::File);
        assert!(reference_tokens("```sh\n@deploy\n```", &[], &commands, &files, &[]).is_empty());
        let selected = PromptToken {
            start: 4,
            end: text.len(),
            kind: PromptTokenKind::Skill,
            name: "deploy".into(),
        };
        assert_eq!(
            reference_tokens(
                text,
                std::slice::from_ref(&selected),
                &commands,
                &files,
                &[]
            ),
            [selected]
        );
    }

    #[test]
    fn slash_groups_prompts_and_skills_and_leaves_app_actions_in_the_palette() {
        let mut prompt = command("fix", CommandScope::Project);
        prompt.template = Some("Fix this".into());
        let commands = [
            command("deploy", CommandScope::Skill),
            command("model", CommandScope::Builtin),
            command("compact", CommandScope::Builtin),
            prompt,
            command("settings", CommandScope::Skill),
        ];
        let names = filter_commands(&commands, "", &mut matcher())
            .into_iter()
            .map(|row| row.item.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["compact", "fix", "deploy", "settings"]);
    }

    #[test]
    fn submissions_resolve_skills_before_templates() {
        let skill = command("mattpocock-skills:to-spec", CommandScope::Skill);
        let mut template = command("fix", CommandScope::Project);
        template.template = Some("Fix: $ARGUMENTS".into());
        let commands = [skill, template];
        let resolve = |provider, prompt| resolved_submission(provider, prompt, &[], &commands);
        let typed = "@mattpocock-skills:to-spec carefully";
        assert_eq!(
            resolve(ProviderKind::Codex, typed).as_deref(),
            Some("$mattpocock-skills:to-spec carefully")
        );
        assert_eq!(
            resolve(ProviderKind::Claude, typed).as_deref(),
            Some("/mattpocock-skills:to-spec carefully")
        );
        assert_eq!(
            resolve(ProviderKind::Claude, "/fix the tests").as_deref(),
            Some("Fix: the tests")
        );
        for provider in ProviderKind::ALL {
            assert_eq!(resolve(provider, "$HOME is unset"), None);
        }
    }

    fn command(name: &str, scope: CommandScope) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            description: String::new(),
            scope,
            argument_hint: None,
            template: None,
        }
    }

    #[test]
    fn goal_submissions_parse_into_their_intent() {
        let builtin = command("goal", CommandScope::Builtin);
        let commands = std::slice::from_ref(&builtin);
        let parse = |prompt: &str| parse_goal_submission(ProviderKind::Codex, prompt, commands);

        assert_eq!(parse("/goal"), Some(GoalCommand::Show));
        assert_eq!(parse("/goal "), Some(GoalCommand::Show));
        assert_eq!(parse("/goal edit"), Some(GoalCommand::Edit));
        assert_eq!(parse("/goal pause"), Some(GoalCommand::Pause));
        assert_eq!(parse("/goal resume"), Some(GoalCommand::Resume));
        assert_eq!(parse("/goal clear"), Some(GoalCommand::Clear));
        assert_eq!(
            parse("/goal improve benchmark coverage"),
            Some(GoalCommand::Set("improve benchmark coverage".into()))
        );
        assert_eq!(parse("/goals"), None);
        assert_eq!(parse("ship /goal"), None);
    }

    #[test]
    fn goal_command_is_codex_only_and_respects_overrides() {
        let builtin = command("goal", CommandScope::Builtin);
        assert_eq!(
            parse_goal_submission(
                ProviderKind::Claude,
                "/goal",
                std::slice::from_ref(&builtin)
            ),
            None
        );
        // A project command deliberately owning /goal wins the collision.
        let mut project = command("goal", CommandScope::Project);
        project.template = Some("do project things".into());
        assert_eq!(
            parse_goal_submission(ProviderKind::Codex, "/goal", std::slice::from_ref(&project)),
            None
        );
    }
}
