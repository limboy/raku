//! Delegation group polling and shared task status presentation.
//!
//! The daemon owns every delegation row and every child event stream. This
//! module only mirrors a group's projection for display: it never writes a
//! subtask's status, and a missing group means "not known yet" rather than an
//! empty result.

use std::time::Duration;

use uuid::Uuid;
use waku_client::delegation::{DelegatedTaskStatus, DelegationGroupInfo};

use super::*;
use crate::ui::ActivationExt;

/// How often a session with a live group re-reads it. A frame never waits on
/// this: the list is read from memory and this only refreshes it.
const DELEGATION_POLL_INTERVAL: Duration = Duration::from_millis(1200);

impl Waku {
    /// The groups belonging to the selected session, in creation order.
    pub(super) fn selected_delegations(&self) -> &[DelegationGroupInfo] {
        self.selected_session()
            .and_then(|session| self.session_delegations.get(&session.id))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The selected session's turns that started delegation groups.
    ///
    /// A group records its parent turn as that turn's UUID string. Groups are
    /// few per session, so this stays cheap enough for the row fingerprint.
    pub(super) fn selected_delegated_turns(&self) -> HashSet<Uuid> {
        delegated_turn_ids(self.selected_delegations())
    }

    /// The groups the turn `turn_id` started, in creation order.
    pub(super) fn turn_delegations(
        &self,
        turn_id: Uuid,
    ) -> impl Iterator<Item = &DelegationGroupInfo> {
        self.selected_delegations()
            .iter()
            .filter(move |group| group_turn_id(group) == Some(turn_id))
    }

    /// Whether any group of the selected session is still working, which is
    /// what keeps the poll running.
    fn has_live_delegation(&self) -> bool {
        self.selected_delegations()
            .iter()
            .any(|group| !group.status.is_terminal())
    }

    /// Read the selected session's groups once, then keep reading while any of
    /// them is still running.
    pub(super) fn refresh_session_delegations(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session().map(|session| session.id) else {
            return;
        };
        let client = self.daemon.client();
        let already_pending = !self.delegation_refresh_pending.insert(session_id);
        if already_pending {
            return;
        }
        let entity = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            loop {
                let client = client.clone();
                let request = cx.background_executor().spawn(async move {
                    match client.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::ListSessionDelegations { session_id },
                    ) {
                        Ok(waku_client::ResponsePayload::SessionDelegations { groups }) => {
                            Some(groups)
                        }
                        _ => None,
                    }
                });
                let groups = request.await;
                let mut keep_polling = false;
                if entity
                    .update(cx, |waku, cx| {
                        let selected = waku.state.selected_session == Some(session_id);
                        let changed = groups.as_ref().is_some_and(|groups| {
                            waku.session_delegations.get(&session_id) != Some(groups)
                        });
                        let mut moved_turns = HashSet::new();
                        if let Some(groups) = groups {
                            moved_turns = changed_turn_ids(
                                waku.session_delegations
                                    .get(&session_id)
                                    .map(Vec::as_slice)
                                    .unwrap_or_default(),
                                &groups,
                            );
                            waku.session_delegations.insert(session_id, groups);
                        }
                        let turn_active = waku
                            .state
                            .sessions
                            .iter()
                            .find(|session| session.id == session_id)
                            .and_then(AgentSession::active_turn_id)
                            .is_some();
                        keep_polling = selected && (waku.has_live_delegation() || turn_active);
                        if !keep_polling {
                            waku.delegation_refresh_pending.remove(&session_id);
                        }
                        if selected {
                            waku.resume_active_delegation_detail(cx);
                        }
                        if changed && selected {
                            // A new group adds a row (the fingerprint sees
                            // it); a status change only reflows its card.
                            waku.remeasure_sub_agent_cards(&moved_turns);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
                if !keep_polling {
                    return;
                }
                cx.background_executor()
                    .timer(DELEGATION_POLL_INTERVAL)
                    .await;
            }
        })
        .detach();
    }

    /// The agents a settled turn delegated to, one row per subtask. Each row
    /// opens that subtask's transcript in the right panel, like the summary
    /// popover's rows. Reads only the mirrored groups; never the daemon.
    pub(super) fn render_sub_agents_card(
        &self,
        turn_id: Uuid,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self
            .selected_session()?
            .turns
            .iter()
            .find(|turn| turn.id == turn_id)
            .is_none_or(|turn| turn.status == TurnStatus::Running)
        {
            return None;
        }
        let tasks = self
            .turn_delegations(turn_id)
            .flat_map(|group| group.tasks.iter().map(move |task| (group.id, task)))
            .collect::<Vec<_>>();
        if tasks.is_empty() {
            return None;
        }
        let open_task = self.active_delegation_task();
        let title = if tasks.len() == 1 {
            tr!("transcript.sub_agent", count = tasks.len())
        } else {
            tr!("transcript.sub_agents", count = tasks.len())
        };
        // "2 Completed · 1 Failed", in first-seen order.
        let mut counts: Vec<(DelegatedTaskStatus, usize)> = Vec::new();
        for (_, task) in &tasks {
            match counts.iter_mut().find(|(status, _)| *status == task.status) {
                Some((_, count)) => *count += 1,
                None => counts.push((task.status, 1)),
            }
        }
        let summary = counts
            .iter()
            .map(|(status, count)| format!("{count} {}", task_status_label(*status)))
            .collect::<Vec<_>>()
            .join(" · ");

        let mut card = div()
            .id(SharedString::from(format!("sub-agents-card-{turn_id}")))
            .w_full()
            .min_w_0()
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.overlay)
            .tab_index(0)
            .tab_group()
            .tab_stop(false)
            .overflow_hidden()
            .child(
                div()
                    .min_h(px(58.0))
                    .px(px(12.0))
                    .py(px(9.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .size(px(36.0))
                            .flex_none()
                            .rounded(px(9.0))
                            .bg(theme.overlay_strong)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon("icons/users.svg", 16.0, theme.text_tertiary)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(sp(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(sp(12.5))
                                    .line_height(sp(14.0))
                                    .text_color(theme.text_tertiary)
                                    .child(summary),
                            ),
                    ),
            );

        let mut rows = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme.border);
        for (group_id, task) in tasks {
            let task_id = task.task_id;
            let control = format!("sub-agent-row-{task_id}");
            let focus = self.transcript_control_focus(control.clone(), cx);
            let selected = open_task == Some((group_id, task_id));
            let status_color = task_status_color(theme, task.status);
            let model = self.delegated_model_label(&task.target);
            let elapsed = task.duration_ms.map(format_task_duration);
            let status_text = (task.status != DelegatedTaskStatus::Completed)
                .then(|| task_status_label(task.status));
            let name = task.target.display_label.clone();
            rows = rows.child(
                div()
                    .id(SharedString::from(control))
                    .track_focus(&focus)
                    .tab_index(0)
                    .h(px(34.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_default()
                    .when(selected, |row| row.bg(theme.overlay_strong))
                    .focus_visible(|row| row.bg(theme.overlay_strong))
                    .hover(|row| row.bg(theme.overlay_strong))
                    .active(|row| row.bg(theme.overlay))
                    .tooltip(Tooltip::text(tr!(
                        "transcript.sub_agents_open",
                        name = name.clone()
                    )))
                    .child(icon(task_status_icon(task.status), 12.0, status_color))
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(220.0))
                            .truncate()
                            .text_size(sp(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(sp(12.5))
                            .text_color(theme.text_tertiary)
                            .child(model),
                    )
                    .when_some(status_text, |row, label| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(sp(12.5))
                                .text_color(status_color)
                                .child(label),
                        )
                    })
                    .when_some(elapsed, |row, elapsed| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(sp(12.5))
                                .text_color(theme.text_ghost)
                                .child(elapsed),
                        )
                    })
                    .child(icon("icons/chevron-right.svg", 11.0, theme.text_tertiary))
                    .on_activation(cx, move |this, window, cx| {
                        this.open_delegated_task(group_id, task_id, window, cx);
                    }),
            );
        }
        card = card.child(rows);
        Some(card.into_any_element())
    }

    /// "GPT-5 · High" for a delegated target, from the in-memory catalogue
    /// when it knows the model and from raw IDs when it does not.
    fn delegated_model_label(&self, target: &waku_client::delegation::ModelReference) -> String {
        let model = self.model_display_name(target.provider, Some(&target.model_id));
        let Some(effort) = target.reasoning_effort.as_deref() else {
            return model;
        };
        let effort = self
            .provider_probe(target.provider)
            .and_then(|probe| probe.model(&target.model_id))
            .and_then(|model| {
                model
                    .reasoning_efforts
                    .iter()
                    .find(|option| option.id == effort)
            })
            .map_or_else(|| effort.to_owned(), |option| option.label.clone());
        format!("{model} · {effort}")
    }

    /// Stop one subtask or the whole group. The daemon owns the drivers, so the
    /// UI only asks; the next poll reflects the result.
    pub(super) fn cancel_delegation(
        &mut self,
        group_id: Uuid,
        task_id: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        let client = self.daemon.client();
        let session_id = self.selected_session().map(|session| session.id);
        cx.spawn(async move |waku, cx| {
            let _ = cx
                .background_executor()
                .spawn(async move {
                    client.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::CancelDelegation { group_id, task_id },
                    )
                })
                .await;
            if let Some(session_id) = session_id {
                waku.update(cx, |waku, cx| {
                    waku.delegation_refresh_pending.remove(&session_id);
                    waku.refresh_session_delegations(cx);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }
}

fn format_task_duration(duration_ms: u64) -> String {
    let seconds = duration_ms / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3_600, (seconds % 3_600) / 60)
    }
}

/// The turn a group belongs to, when its recorded parent turn is one.
/// Older groups fell back to the session ID, which matches no turn.
pub(super) fn group_turn_id(group: &DelegationGroupInfo) -> Option<Uuid> {
    group.parent_turn_id.as_deref()?.parse().ok()
}

pub(super) fn delegated_turn_ids(groups: &[DelegationGroupInfo]) -> HashSet<Uuid> {
    groups.iter().filter_map(group_turn_id).collect()
}

/// Turns whose groups differ between two reads of one session.
fn changed_turn_ids(
    previous: &[DelegationGroupInfo],
    next: &[DelegationGroupInfo],
) -> HashSet<Uuid> {
    let mut turns = HashSet::new();
    for group in next {
        if !previous.contains(group)
            && let Some(turn_id) = group_turn_id(group)
        {
            turns.insert(turn_id);
        }
    }
    for group in previous {
        if !next.iter().any(|candidate| candidate.id == group.id)
            && let Some(turn_id) = group_turn_id(group)
        {
            turns.insert(turn_id);
        }
    }
    turns
}

pub(super) fn active_task_activity(
    status: DelegatedTaskStatus,
    latest_activity: Option<&str>,
) -> Option<&str> {
    if !matches!(
        status,
        DelegatedTaskStatus::Starting | DelegatedTaskStatus::Running
    ) {
        return None;
    }
    latest_activity?
        .split(['\n', '\r'])
        .map(str::trim)
        .find(|activity| !activity.is_empty())
}

pub(super) fn task_status_label(status: DelegatedTaskStatus) -> String {
    match status {
        DelegatedTaskStatus::Queued => tr!("delegation.status_queued"),
        DelegatedTaskStatus::Starting => tr!("delegation.status_starting"),
        DelegatedTaskStatus::Running => tr!("delegation.status_running"),
        DelegatedTaskStatus::Completed => tr!("delegation.status_completed"),
        DelegatedTaskStatus::Failed => tr!("delegation.status_failed"),
        DelegatedTaskStatus::Cancelled => tr!("delegation.status_cancelled"),
        DelegatedTaskStatus::TimedOut => tr!("delegation.status_timed_out"),
        DelegatedTaskStatus::Interrupted => tr!("delegation.status_interrupted"),
    }
}

pub(super) fn task_status_icon(status: DelegatedTaskStatus) -> &'static str {
    match status {
        DelegatedTaskStatus::Completed => "icons/check.svg",
        DelegatedTaskStatus::Failed => "icons/alert.svg",
        DelegatedTaskStatus::TimedOut => "icons/hourglass.svg",
        DelegatedTaskStatus::Cancelled | DelegatedTaskStatus::Interrupted => "icons/block.svg",
        DelegatedTaskStatus::Queued
        | DelegatedTaskStatus::Starting
        | DelegatedTaskStatus::Running => "icons/loader-circle.svg",
    }
}

pub(super) fn task_status_color(theme: &Theme, status: DelegatedTaskStatus) -> gpui::Hsla {
    match status {
        DelegatedTaskStatus::Completed => theme.text_tertiary,
        DelegatedTaskStatus::Failed | DelegatedTaskStatus::TimedOut => theme.warning,
        _ => theme.accent,
    }
}

/// The instruction block appended to a prompt that names delegation targets.
///
/// The daemon hands the transport a plain string, and the MCP server only
/// exposes two tool names. Neither tells the agent which `target_ref` a
/// `^[label]` marker stands for, or that this turn may delegate at all — so the
/// prompt carries that mapping, and nothing is inferred by the agent.
///
/// A configured sub-agent's standing instructions reach its subtask directly
/// from the daemon; the parent only sees a one-line role summary, enough to
/// split the work sensibly without restating the role in each task.
pub(super) fn delegation_prompt_suffix(
    references: &[waku_client::delegation::ModelReference],
) -> Option<String> {
    if references.is_empty() {
        return None;
    }
    let mut text = String::from("Delegation is available for this turn.\nTargets:\n");
    for reference in references {
        text.push_str(&format!(
            "- target_ref \"{}\": {} — provider {}, model {}, reasoning {}\n",
            reference.reference_id,
            reference.display_label,
            reference.provider.display_name(),
            reference.model_id,
            reference.reasoning_effort.as_deref().unwrap_or("default"),
        ));
        if let Some(role) = reference
            .agent_instructions
            .as_deref()
            .and_then(|instructions| {
                instructions
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
            })
        {
            text.push_str(&format!(
                "  role (already given to this agent): {}\n",
                role_summary(role)
            ));
        }
    }
    text.push_str(
        "\nEach task's `target_ref` must be one of the quoted ids above, not a model or agent name.\n",
    );
    text.push_str(
        "\nThe `^[...]` markers in the request mean the user explicitly selected those agents to execute this task in parallel. \
         You MUST call `delegate_batch` first with one task per listed target (`task_key` unique per subtask, `target_ref` set to the exact quoted id above, and `instructions` giving that agent its self-contained task), \
         then call `wait_delegates(group_id)` until the group settles, and finally synthesize and compare the returned conclusions, naming which agent produced each point. \
         Only skip `delegate_batch` if the user is asking a meta-question about the agents or models themselves (such as their context window or pricing) rather than giving a task or question to answer.",
    );
    Some(text)
}

/// One line of an agent's instructions, bounded so a long role prompt cannot
/// crowd the parent's own request.
fn role_summary(line: &str) -> String {
    const MAX_CHARS: usize = 160;
    if line.chars().count() <= MAX_CHARS {
        return line.to_owned();
    }
    let mut clipped = line.chars().take(MAX_CHARS - 1).collect::<String>();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_activity_only_replaces_live_execution_status() {
        for status in [DelegatedTaskStatus::Starting, DelegatedTaskStatus::Running] {
            assert_eq!(
                active_task_activity(status, Some("  Reading source  ")),
                Some("Reading source")
            );
            assert_eq!(
                active_task_activity(status, Some(" \n  web_search\r\nReading results")),
                Some("web_search")
            );
            assert_eq!(active_task_activity(status, Some(" \n\t ")), None);
            assert_eq!(active_task_activity(status, None), None);
        }
        for status in [
            DelegatedTaskStatus::Queued,
            DelegatedTaskStatus::Completed,
            DelegatedTaskStatus::Failed,
            DelegatedTaskStatus::Cancelled,
            DelegatedTaskStatus::TimedOut,
            DelegatedTaskStatus::Interrupted,
        ] {
            assert_eq!(active_task_activity(status, Some("Reading source")), None);
        }
    }

    #[test]
    fn prompt_suffix_names_agents_and_summarizes_their_role() {
        let mut agent = waku_client::delegation::ModelReference::new(
            "target-1",
            ProviderKind::Codex,
            "gpt-5",
            "Reviewer",
        );
        agent.agent_id = Some(Uuid::new_v4());
        agent.agent_instructions = Some("\n  Focus on security.\nCite files.".into());
        let suffix = delegation_prompt_suffix(&[agent]).unwrap();
        assert!(suffix.contains("target_ref \"target-1\": Reviewer"));
        assert!(suffix.contains("role (already given to this agent): Focus on security."));
        assert!(!suffix.contains("Cite files."));
        assert_eq!(delegation_prompt_suffix(&[]), None);
    }
}
