//! Decisions the agent hands back at the end of a reply (`follow-up` fences).
//!
//! When a reply leaves the user a next step to choose, the agent ends it with
//! a fence whose info string is `follow-up`: the question on its first lines,
//! then one `- Label` line per answer, the recommended answer first. The
//! fence never renders as code. Under the latest settled reply it becomes a
//! card right under the reply whose buttons send their label as the user's
//! reply; anywhere else it is simply hidden.

use super::*;

/// More answers than this stop being a quick decision.
const MAX_ACTIONS: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FollowUp {
    pub question: String,
    /// Reply labels, the recommended one first.
    pub actions: Vec<String>,
}

/// Split a trailing `follow-up` fence off an assistant reply: the text to
/// render, and the decision when the fence holds at least one answer. An
/// unclosed fence (still streaming) is hidden too, so its raw lines never
/// flash in as a code block.
pub(super) fn split_follow_up(content: &str) -> (&str, Option<FollowUp>) {
    // Rows call this every frame; a reply without the marker costs one
    // substring search.
    if !content.contains("follow") {
        return (content, None);
    }
    struct Open {
        marker: u8,
        len: usize,
        start: usize,
        body_start: usize,
        follow_up: bool,
    }
    let mut open: Option<Open> = None;
    // The last follow-up fence with nothing but blank lines after it.
    let mut trailing: Option<(usize, Range<usize>)> = None;
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let text = line.trim_end_matches(['\n', '\r']);
        let indent = text.len() - text.trim_start_matches(' ').len();
        let fence = (indent <= 3).then(|| fence_marker(&text[indent..])).flatten();
        if let Some(current) = &open {
            if let Some((marker, len, info)) = fence
                && marker == current.marker
                && len >= current.len
                && info.trim().is_empty()
            {
                if current.follow_up {
                    trailing = Some((current.start, current.body_start..line_start));
                }
                open = None;
            }
            continue;
        }
        if let Some((marker, len, info)) = fence {
            trailing = None;
            open = Some(Open {
                marker,
                len,
                start: line_start,
                body_start: offset,
                follow_up: info
                    .split_whitespace()
                    .next()
                    .is_some_and(is_follow_up_language),
            });
            continue;
        }
        if !text.trim().is_empty() {
            trailing = None;
        }
    }
    if let Some(current) = open {
        trailing = current
            .follow_up
            .then(|| (current.start, current.body_start.min(content.len())..content.len()));
    }
    let Some((start, body)) = trailing else {
        return (content, None);
    };
    (content[..start].trim_end(), parse_body(&content[body]))
}

fn is_follow_up_language(language: &str) -> bool {
    language.eq_ignore_ascii_case("follow-up") || language.eq_ignore_ascii_case("followup")
}

/// A fence line's marker byte, run length, and info string.
fn fence_marker(text: &str) -> Option<(u8, usize, &str)> {
    let marker = *text.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let len = text.bytes().take_while(|byte| *byte == marker).count();
    if len < 3 {
        return None;
    }
    let info = &text[len..];
    (marker != b'`' || !info.contains('`')).then_some((marker, len, info))
}

fn parse_body(body: &str) -> Option<FollowUp> {
    let mut question = String::new();
    let mut actions = Vec::new();
    for line in body.lines().map(str::trim) {
        if let Some(label) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
        {
            let label = label.trim().trim_matches('*').trim();
            if !label.is_empty() && actions.len() < MAX_ACTIONS {
                actions.push(label.to_owned());
            }
        } else if !line.is_empty() && actions.is_empty() {
            if !question.is_empty() {
                question.push(' ');
            }
            question.push_str(line);
        }
    }
    (!actions.is_empty()).then_some(FollowUp { question, actions })
}

impl Raku {
    /// The decision a session's latest reply left open, when `message_index`
    /// is that reply, nothing else is underway, and it was not dismissed.
    fn pending_follow_up(&self, message_index: usize) -> Option<(Uuid, FollowUp)> {
        let session = self.selected_session()?;
        if message_index + 1 != session.messages.len()
            || session.is_busy()
            || !session.queued_messages.is_empty()
            || self.submission_preparations.contains(&session.id)
        {
            return None;
        }
        let message = session.messages.get(message_index)?;
        if message.role != MessageRole::Assistant
            || message.streaming
            || message.error.is_some()
            || self.dismissed_follow_ups.contains(&message.id)
        {
            return None;
        }
        let (_, follow_up) = split_follow_up(message.visible_content());
        Some((message.id, follow_up?))
    }

    /// The response footer row that should carry a card this frame.
    fn follow_up_row(&self) -> Option<TranscriptRowKind> {
        let session = self.selected_session()?;
        let index = session.messages.len().checked_sub(1)?;
        let turn_id = session.messages[index].turn_id?;
        self.pending_follow_up(index)?;
        Some(TranscriptRowKind::ResponseFooter(turn_id, index))
    }

    /// The card grows and shrinks a row whose height the list caches, and it
    /// comes and goes with state that never touches that row: a reply typed
    /// in the composer, a queue drain, a run continuing on its own. Compare
    /// once per frame and remeasure the rows that gained or lost it. Only the
    /// last message is inspected, so this stays cheap on long transcripts.
    pub(super) fn sync_follow_up_row(&self) {
        let current = self.follow_up_row();
        let previous = self.follow_up_card_row.replace(current);
        if previous == current {
            return;
        }
        for row in [previous, current].into_iter().flatten() {
            self.remeasure_transcript_row(row);
        }
    }

    fn answer_follow_up(&mut self, message_id: Uuid, label: String, cx: &mut Context<Self>) {
        self.dismiss_follow_up(message_id, cx);
        self.submit_composer_submission(ComposerSubmission::plain(label), cx);
    }

    fn dismiss_follow_up(&mut self, message_id: Uuid, cx: &mut Context<Self>) {
        self.dismissed_follow_ups.insert(message_id);
        cx.notify();
    }

    /// The card between the latest reply and its footer. Its buttons answer the question the
    /// way typing the label would; replying in the composer works as well.
    pub(super) fn render_follow_up(
        &self,
        message_index: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (message_id, follow_up) = self.pending_follow_up(message_index)?;
        let mut buttons = div()
            .mt(px(12.0))
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.0));
        for (index, label) in follow_up.actions.into_iter().enumerate() {
            let primary = index == 0;
            let id = format!("follow-up-{message_id}-{index}");
            let focus = self.transcript_control_focus(id.clone(), cx);
            let click_label = label.clone();
            let key_label = label.clone();
            buttons = buttons.child(
                div()
                    .id(SharedString::from(id))
                    .track_focus(&focus)
                    .tab_index(0)
                    .tab_stop(true)
                    .h(px(28.0))
                    .px(px(13.0))
                    .rounded(px(7.0))
                    .border_1()
                    .flex()
                    .items_center()
                    .cursor_default()
                    .text_size(sp(SMALL_UI_FONT_SIZE))
                    .font_weight(FontWeight::MEDIUM)
                    .when(primary, |button| {
                        button
                            .border_color(theme.inverse)
                            .bg(theme.inverse)
                            .text_color(theme.on_inverse)
                            .hover(|button| button.opacity(0.9))
                    })
                    .when(!primary, |button| {
                        button
                            .border_color(theme.overlay)
                            .bg(theme.overlay)
                            .text_color(theme.text)
                            .hover(|button| button.bg(theme.overlay_strong))
                    })
                    .active(|button| button.opacity(0.8))
                    .focus_visible(|button| button.border_color(theme.accent))
                    .child(SharedString::from(label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.answer_follow_up(message_id, click_label.clone(), cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.answer_follow_up(message_id, key_label.clone(), cx);
                            cx.stop_propagation();
                        }
                    })),
            );
        }
        let dismiss_id = format!("follow-up-{message_id}-dismiss");
        let dismiss_focus = self.transcript_control_focus(dismiss_id.clone(), cx);
        let dismiss = div()
            .id(SharedString::from(dismiss_id))
            .track_focus(&dismiss_focus)
            .tab_index(0)
            .tab_stop(true)
            .flex_none()
            .size(px(24.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border.opacity(0.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .hover(|button| button.bg(theme.overlay))
            .active(|button| button.opacity(0.8))
            .focus_visible(|button| button.border_color(theme.accent))
            .child(icon("icons/x.svg", 12.0, theme.text_tertiary))
            .tooltip(Tooltip::text(tr!("user_input.dismiss")))
            .on_click(cx.listener(move |this, _, _, cx| this.dismiss_follow_up(message_id, cx)))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space" | "escape") {
                    this.dismiss_follow_up(message_id, cx);
                    cx.stop_propagation();
                }
            }));
        Some(
            div()
                .id(SharedString::from(format!("follow-up-{message_id}")))
                .w_full()
                .mt(px(4.0))
                .mb(px(9.0))
                .p(px(14.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(12.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .pt(px(3.0))
                                .text_size(sp(SMALL_UI_FONT_SIZE))
                                .line_height(sp(18.0))
                                .text_color(theme.text)
                                .whitespace_normal()
                                .child(SharedString::from(follow_up.question)),
                        )
                        .child(dismiss),
                )
                .child(buttons)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn follow_up(question: &str, actions: &[&str]) -> Option<FollowUp> {
        Some(FollowUp {
            question: question.into(),
            actions: actions.iter().map(|label| (*label).into()).collect(),
        })
    }

    #[test]
    fn trailing_fence_becomes_a_decision() {
        let content = "Done.\n\n```follow-up\nShould I add retry?\n- Yes, fix it\n- Not now\n```\n";
        assert_eq!(
            split_follow_up(content),
            ("Done.", follow_up("Should I add retry?", &["Yes, fix it", "Not now"]))
        );
    }

    #[test]
    fn streaming_fence_is_hidden_before_it_closes() {
        let content = "Done.\n\n```follow-up\nShould I";
        assert_eq!(split_follow_up(content), ("Done.", None));
    }

    #[test]
    fn fence_followed_by_prose_is_left_alone() {
        let content = "```follow-up\nQ?\n- Yes\n```\n\nMore text.";
        assert_eq!(split_follow_up(content), (content, None));
    }

    #[test]
    fn fence_inside_another_fence_is_code() {
        let content = "````md\n```follow-up\nQ?\n- Yes\n```\n````";
        assert_eq!(split_follow_up(content), (content, None));
    }

    #[test]
    fn fence_without_answers_is_hidden_without_a_card() {
        let content = "Done.\n```followup\nJust a question?\n```";
        assert_eq!(split_follow_up(content), ("Done.", None));
    }

    #[test]
    fn answers_are_capped() {
        let content = "```follow-up\nPick\n- A\n- B\n- C\n- D\n```";
        assert_eq!(split_follow_up(content), ("", follow_up("Pick", &["A", "B", "C"])));
    }
}
