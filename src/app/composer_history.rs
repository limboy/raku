//! Prompt history in the composer: `up` in an empty composer recalls the
//! selected session's previous prompts, newest first, and `down` walks back
//! toward the empty draft.
//!
//! Browsing lasts exactly as long as the field still holds the recalled
//! prompt. The first edit turns it into an ordinary draft — the header goes
//! away and the arrows move the caret again — so there is no mode to leave
//! explicitly. While browsing, the composer card declares the
//! `ComposerHistory` key context and the arrows reach it as actions that
//! outrank the field's own bindings, the same split the autocomplete uses.

use gpui::{App, KeyBinding, actions};

use super::*;

actions!(composer_history, [HistoryPrevious, HistoryNext]);

const HISTORY_CONTEXT: &str = "ComposerHistory > TextInput";

/// Bind the history keys. Must run after [`crate::input::init`] so these win
/// the tie with the field's own arrow bindings.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", HistoryPrevious, Some(HISTORY_CONTEXT)),
        KeyBinding::new("down", HistoryNext, Some(HISTORY_CONTEXT)),
        KeyBinding::new("ctrl-p", HistoryPrevious, Some(HISTORY_CONTEXT)),
        KeyBinding::new("ctrl-n", HistoryNext, Some(HISTORY_CONTEXT)),
    ]);
}

/// The prompt currently recalled into the composer.
pub(super) struct ComposerHistoryRecall {
    session_id: Uuid,
    /// 1-based position counted from the newest prompt.
    index: usize,
    total: usize,
    /// What was placed in the field; any divergence means the user edited it.
    text: String,
}

/// The session's prompts, newest first, with consecutive repeats collapsed
/// the way a shell history does.
fn history_entries(session: &AgentSession) -> Vec<&str> {
    let mut entries: Vec<&str> = Vec::new();
    for message in session.messages.iter().rev() {
        if message.role != MessageRole::User {
            continue;
        }
        let text = message
            .display_content
            .as_deref()
            .unwrap_or(&message.content)
            .trim();
        if text.is_empty() || entries.last() == Some(&text) {
            continue;
        }
        entries.push(text);
    }
    entries
}

impl Waku {
    /// `(index, total)` while the composer still shows a recalled prompt.
    pub(super) fn composer_history_position(&self, cx: &App) -> Option<(usize, usize)> {
        let recall = self.composer_history.as_ref()?;
        (self.state.selected_session == Some(recall.session_id)
            && self.composer.read(cx).content(cx) == recall.text)
            .then_some((recall.index, recall.total))
    }

    /// Whether the arrows should browse history rather than move the caret:
    /// already browsing, or an empty composer in a session with prompts.
    pub(super) fn composer_history_available(&self, cx: &App) -> bool {
        if self.composer_history_position(cx).is_some() {
            return true;
        }
        self.composer.read(cx).content(cx).is_empty()
            && self.composer_attachments.is_empty()
            && self.selected_session().is_some_and(|session| {
                session
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::User)
            })
    }

    pub(super) fn navigate_composer_history(&mut self, older: bool, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let session_id = session.id;
        let entries = history_entries(session);
        let current = self
            .composer_history_position(cx)
            .map_or(0, |(index, _)| index);
        let next = if older {
            (current + 1).min(entries.len())
        } else {
            current.saturating_sub(1)
        };
        if next == current {
            return;
        }
        if next == 0 {
            self.composer_history = None;
            self.composer.update(cx, |input, cx| input.clear(cx));
        } else {
            let text = entries[next - 1].to_owned();
            self.composer_history = Some(ComposerHistoryRecall {
                session_id,
                index: next,
                total: entries.len(),
                text: text.clone(),
            });
            self.composer
                .update(cx, |input, cx| input.set_content(text, cx));
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_newest_first_without_repeats_or_replies() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        for (role, text) in [
            (MessageRole::User, "first"),
            (MessageRole::Assistant, "reply"),
            (MessageRole::User, "second"),
            (MessageRole::User, "second"),
            (MessageRole::User, "  "),
        ] {
            session.messages.push(Message::new(role, text));
        }
        let mut shown = Message::new(MessageRole::User, "third\n@src/lib.rs");
        shown.display_content = Some("third".into());
        session.messages.push(shown);

        assert_eq!(history_entries(&session), ["third", "second", "first"]);
    }
}
