//! Whether saves are landing, and the warning shown while they are not.
//!
//! Task state reaches the daemon as acknowledged requests and the desktop
//! documents land on the background executor; both report back here through
//! the event pump. A failed save re-marks what it carried, retries with
//! backoff, and keeps a banner up until a later save covering it succeeds.

use super::*;
use raku_client::persistence::PendingTaskStateSave;

const SAVE_RETRY_INITIAL: Duration = Duration::from_secs(1);
const SAVE_RETRY_MAX: Duration = Duration::from_secs(30);

pub(super) enum SaveOutcome {
    TaskState {
        sequence: u64,
        session_ids: Vec<Uuid>,
        result: Result<(), String>,
    },
    Documents {
        sequence: u64,
        result: Result<(), String>,
    },
}

#[derive(Default)]
pub(super) struct SaveStatus {
    /// Number of the most recent save; outcomes carry the save they belong to.
    sequence: u64,
    task_state: Option<SaveFailure>,
    documents: Option<SaveFailure>,
    retry_attempts: u32,
    retry_pending: bool,
    /// Bumped to orphan a scheduled retry, e.g. after a manual retry.
    retry_generation: u64,
}

struct SaveFailure {
    error: String,
    /// The newest save issued when this failure was handled. A failure only
    /// clears once a save issued after it succeeds: earlier in-flight saves
    /// did not carry the edits it put back.
    issued_before: u64,
}

impl SaveFailure {
    fn resolved_by(&self, sequence: u64) -> bool {
        sequence > self.issued_before
    }
}

impl SaveStatus {
    pub(super) fn next_sequence(&mut self) -> u64 {
        self.sequence = self.sequence.wrapping_add(1);
        self.sequence
    }

    fn failure(&self) -> Option<&str> {
        self.task_state
            .as_ref()
            .or(self.documents.as_ref())
            .map(|failure| failure.error.as_str())
    }

    fn record(
        slot: &mut Option<SaveFailure>,
        sequence: u64,
        latest: u64,
        result: Result<(), String>,
    ) {
        match result {
            Ok(()) => {
                if slot
                    .as_ref()
                    .is_some_and(|failure| failure.resolved_by(sequence))
                {
                    *slot = None;
                }
            }
            Err(error) => {
                *slot = Some(SaveFailure {
                    error,
                    issued_before: latest,
                });
            }
        }
    }
}

/// Awaits task-state saves in the order they were sent, off the UI thread.
/// The daemon commits saves in order, so one waiter keeps the outcomes
/// ordered too and never parks more than one executor thread on the daemon.
pub(super) fn spawn_task_state_save_waiter(
    outcomes: Sender<SaveOutcome>,
    wake: smol::channel::Sender<()>,
) -> Sender<(u64, PendingTaskStateSave)> {
    let (saves, pending) = unbounded::<(u64, PendingTaskStateSave)>();
    std::thread::Builder::new()
        .name("raku-task-state-save".into())
        .spawn(move || {
            for (sequence, save) in pending {
                let session_ids = save.session_ids().to_vec();
                let result = save.wait().map_err(|error| error.to_string());
                if outcomes
                    .send(SaveOutcome::TaskState {
                        sequence,
                        session_ids,
                        result,
                    })
                    .is_err()
                {
                    return;
                }
                signal_event_pump(&wake);
            }
        })
        .ok();
    saves
}

impl Raku {
    /// Applies finished saves. Returns whether the warning changed.
    pub(super) fn drain_save_outcomes(&mut self, cx: &mut Context<Self>) -> bool {
        let was_failing = self.save_status.failure().is_some();
        let mut failed = false;
        while let Ok(outcome) = self.save_outcomes.try_recv() {
            let latest = self.save_status.sequence;
            match outcome {
                SaveOutcome::TaskState {
                    sequence,
                    session_ids,
                    result,
                } => {
                    if result.is_err() {
                        failed = true;
                        // Put back what this save carried so the next one
                        // resends it; saves are whole-session and idempotent.
                        for session_id in session_ids {
                            if self
                                .state
                                .sessions
                                .iter()
                                .any(|session| session.id == session_id)
                            {
                                self.state.mark_session_dirty(session_id);
                            }
                        }
                        self.stream_state_dirty = true;
                    }
                    SaveStatus::record(&mut self.save_status.task_state, sequence, latest, result);
                }
                SaveOutcome::Documents { sequence, result } => {
                    failed |= result.is_err();
                    SaveStatus::record(&mut self.save_status.documents, sequence, latest, result);
                }
            }
        }
        let failing = self.save_status.failure().is_some();
        if failed {
            self.schedule_save_retry(cx);
        } else if !failing {
            self.save_status.retry_attempts = 0;
        }
        failing != was_failing || failed
    }

    fn schedule_save_retry(&mut self, cx: &mut Context<Self>) {
        if self.save_status.retry_pending {
            return;
        }
        let exponent = self.save_status.retry_attempts.min(5);
        let delay = SAVE_RETRY_INITIAL
            .saturating_mul(1 << exponent)
            .min(SAVE_RETRY_MAX);
        self.save_status.retry_attempts = self.save_status.retry_attempts.saturating_add(1);
        self.save_status.retry_pending = true;
        let generation = self.save_status.retry_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                if this.save_status.retry_generation != generation {
                    return;
                }
                this.save_status.retry_pending = false;
                this.save();
                cx.notify();
            });
        })
        .detach();
    }

    fn retry_saves_now(&mut self, cx: &mut Context<Self>) {
        self.save_status.retry_generation = self.save_status.retry_generation.wrapping_add(1);
        self.save_status.retry_pending = false;
        self.save();
        cx.notify();
    }

    /// A persistent warning while any save is failing. Unlike a toast it stays
    /// until a save succeeds, because the edits it covers are only in memory.
    pub(super) fn render_save_failure_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let error = self.save_status.failure()?.to_owned();
        let theme = Theme::current(cx);
        let retry = div()
            .id("save-failure-retry")
            .tab_index(0)
            .flex_none()
            .h(px(24.0))
            .px(px(9.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .items_center()
            .text_size(sp(12.0))
            .text_color(theme.text)
            .cursor_default()
            .focus_visible(|style| style.border_color(theme.accent))
            .hover(|element| element.bg(theme.overlay))
            .active(|element| element.bg(theme.overlay_strong))
            .tooltip(Tooltip::text(tr!("save_failure.retry_tooltip")))
            .child(tr!("common.retry"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.retry_saves_now(cx);
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.retry_saves_now(cx);
                    cx.stop_propagation();
                }
            }));
        Some(
            div()
                .id("save-failure-banner")
                .flex_none()
                .w_full()
                .px(px(16.0))
                .py(px(8.0))
                .border_b_1()
                .border_color(theme.border_strong)
                .bg(theme.raised)
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(icon("icons/alert.svg", 14.0, theme.danger))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .child(
                            div()
                                .text_size(sp(12.5))
                                .line_height(sp(16.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(tr!("save_failure.title")),
                        )
                        .child(
                            div()
                                .text_size(sp(12.0))
                                .line_height(sp(16.0))
                                .text_color(theme.text_secondary)
                                .whitespace_normal()
                                .child(tr!("save_failure.detail", error = error)),
                        ),
                )
                .child(retry)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_clears_only_after_a_save_issued_later_succeeds() {
        let mut status = SaveStatus::default();
        let first = status.next_sequence();
        let in_flight = status.next_sequence();
        let latest = status.sequence;

        SaveStatus::record(&mut status.task_state, first, latest, Err("offline".into()));
        assert_eq!(status.failure(), Some("offline"));

        // Sent before the failure was handled, so it never carried the
        // sessions the failure put back.
        SaveStatus::record(&mut status.task_state, in_flight, latest, Ok(()));
        assert_eq!(status.failure(), Some("offline"));

        let retry = status.next_sequence();
        SaveStatus::record(&mut status.task_state, retry, status.sequence, Ok(()));
        assert_eq!(status.failure(), None);
    }

    #[test]
    fn either_failing_store_keeps_the_warning_up() {
        let mut status = SaveStatus::default();
        let sequence = status.next_sequence();
        SaveStatus::record(
            &mut status.documents,
            sequence,
            sequence,
            Err("disk full".into()),
        );
        let next = status.next_sequence();
        SaveStatus::record(&mut status.task_state, next, next, Ok(()));
        assert_eq!(status.failure(), Some("disk full"));
        SaveStatus::record(&mut status.documents, next, next, Ok(()));
        assert_eq!(status.failure(), None);
    }
}
