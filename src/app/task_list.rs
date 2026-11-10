//! The agent's task list, pinned above the composer while its turn runs.
//!
//! Providers report the list as plan activities (Claude's `TaskCreate` /
//! `TaskUpdate`, Codex's `update_plan`). Each also lands in the transcript,
//! where it scrolls away; this keeps the latest whole list in view.

use super::*;
use crate::model::{PlanStep, PlanStepStatus};

/// Rows shown before the list scrolls, so a long plan cannot crowd out the
/// transcript.
const VISIBLE_STEPS: f32 = 8.0;
const STEP_HEIGHT: f32 = 30.0;

/// A session's task list as of its latest plan update.
pub(super) struct SessionTaskList {
    steps: Vec<PlanStep>,
    /// The turn that last updated the list. The card belongs to that turn:
    /// a later turn that never touches the list does not resurrect it.
    turn_id: Option<Uuid>,
}

impl Raku {
    /// Runs as a plan activity arrives, so rendering only reads the store.
    pub(super) fn observe_plan_steps(&mut self, session_id: Uuid, item: &ActivityItem) {
        if item.plan_steps.is_empty() {
            return;
        }
        let turn_id = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(AgentSession::active_turn_id);
        self.task_lists.insert(
            session_id,
            SessionTaskList {
                steps: item.plan_steps.clone(),
                turn_id,
            },
        );
    }

    fn visible_task_list(&self) -> Option<&SessionTaskList> {
        let session = self.selected_session()?;
        let list = self.task_lists.get(&session.id)?;
        list_is_live(list.turn_id, session.active_turn_id()).then_some(list)
    }

    pub(super) fn render_task_list(&self, cx: &mut Context<Self>) -> Option<Div> {
        let list = self.visible_task_list()?;
        let theme = Theme::current(cx);

        Some(
            div().flex_none().px(px(20.0)).pb(px(8.0)).child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .mx_auto()
                    .rounded(px(14.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface)
                    .px(px(16.0))
                    .py(px(14.0))
                    .child(
                        div()
                            .id("task-list-steps")
                            .max_h(px(VISIBLE_STEPS * STEP_HEIGHT))
                            .overflow_y_scroll()
                            .children(list.steps.iter().map(|step| task_step_row(step, theme))),
                    ),
            ),
        )
    }
}

/// The card exists while its turn runs, so the finished list never stands
/// over an idle composer.
fn list_is_live(list_turn: Option<Uuid>, active_turn: Option<Uuid>) -> bool {
    list_turn.is_some() && list_turn == active_turn
}

/// One step. Its state is carried by the glyph's shape and the text tone,
/// never by color alone.
fn task_step_row(step: &PlanStep, theme: Theme) -> Div {
    let (glyph, glyph_color, text_color) = match step.status {
        PlanStepStatus::Completed => (
            "icons/ant/check-circle-filled.svg",
            theme.text_secondary,
            theme.text_secondary,
        ),
        PlanStepStatus::InProgress => (
            "icons/ant/circle-outline.svg",
            theme.text_tertiary,
            theme.text,
        ),
        PlanStepStatus::Pending => (
            "icons/ant/circle-dashed.svg",
            theme.text_secondary,
            theme.text,
        ),
    };
    div()
        .h(px(STEP_HEIGHT))
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(icon(glyph, 14.0, glyph_color))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(sp(14.0))
                .text_color(text_color)
                .child(SharedString::from(step.text.clone())),
        )
}

#[cfg(test)]
mod tests {
    use super::list_is_live;
    use uuid::Uuid;

    #[test]
    fn task_list_disappears_when_its_turn_settles() {
        let turn = Uuid::new_v4();
        assert!(list_is_live(Some(turn), Some(turn)));
        assert!(!list_is_live(Some(turn), None));
        assert!(!list_is_live(None, Some(turn)));
        assert!(!list_is_live(Some(turn), Some(Uuid::new_v4())));
    }
}
