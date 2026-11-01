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
                        if let Some(groups) = groups {
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
    }
    text.push_str(
        "\nEach task's `target_ref` must be one of the quoted ids above, not a model name.\n",
    );
    text.push_str(
        "\nThe `^[...]` markers in the request mean the user explicitly selected those models to execute this task in parallel. \
         You MUST call `delegate_batch` first with one task per listed target (`task_key` unique per subtask, `target_ref` set to the exact quoted id above, and `instructions` giving that model its self-contained task), \
         then call `wait_delegates(group_id)` until the group settles, and finally synthesize and compare the returned conclusions, naming which model produced each point. \
         Only skip `delegate_batch` if the user is asking a meta-question about the models themselves (such as their context window or pricing) rather than giving a task or question to answer.",
    );
    Some(text)
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
}
