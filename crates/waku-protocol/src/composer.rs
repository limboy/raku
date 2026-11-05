use serde::{Deserialize, Serialize};

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
// Shared by the daemon and every client so the composer's `/` and `@` rules
// cannot drift apart.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriggerKind {
    Command,
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
/// A slash command must be the first token of its line — `/` mid-sentence is
/// prose. `@` mentions start at any whitespace boundary, so `see @src/` still
/// completes while `user@host` does not: its sigil is inside a token.
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
}
