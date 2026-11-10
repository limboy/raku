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
const STEP_HEIGHT: f32 = 24.0;

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

    fn visible_task_list(&self) -> Option<(Uuid, &SessionTaskList)> {
        let session = self.selected_session()?;
        let turn_id = session.active_turn_id()?;
        let list = self.task_lists.get(&session.id)?;
        (list.turn_id == Some(turn_id)).then_some((session.id, list))
    }

    fn toggle_task_list(&mut self, cx: &mut Context<Self>) {
        self.task_list_collapsed = !self.task_list_collapsed;
        cx.notify();
    }

    pub(super) fn render_task_list(&self, cx: &mut Context<Self>) -> Option<Div> {
        let (session_id, list) = self.visible_task_list()?;
        let theme = Theme::current(cx);
        let collapsed = self.task_list_collapsed;
        let completed = list
            .steps
            .iter()
            .filter(|step| step.status == PlanStepStatus::Completed)
            .count();
        // Collapsed, the header still says what the agent is on.
        let current = collapsed
            .then(|| {
                list.steps
                    .iter()
                    .find(|step| step.status == PlanStepStatus::InProgress)
            })
            .flatten();

        let focus = self.transcript_control_focus(format!("task-list-toggle-{session_id}"), cx);
        let header = div()
            .id("task-list-toggle")
            .track_focus(&focus)
            .tab_index(0)
            .tab_stop(true)
            .h(px(30.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .cursor_default()
            .rounded(px(11.0))
            .border_1()
            .border_color(gpui::transparent_black())
            .hover(|element| element.bg(theme.overlay))
            .focus_visible(|element| element.border_color(theme.accent))
            .child(icon(
                if collapsed {
                    "icons/chevron-right.svg"
                } else {
                    "icons/chevron-down.svg"
                },
                11.0,
                theme.text_tertiary,
            ))
            .child(
                div()
                    .flex_none()
                    .text_size(sp(SMALL_UI_FONT_SIZE))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(tr!("task_list.title")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(sp(SMALL_UI_FONT_SIZE))
                    .text_color(theme.text_secondary)
                    .children(current.map(|step| SharedString::from(step.text.clone()))),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(sp(12.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!(
                        "activity.plan_progress",
                        completed = completed,
                        total = list.steps.len()
                    )),
            )
            .tooltip(Tooltip::text(if collapsed {
                tr!("task_list.expand")
            } else {
                tr!("task_list.collapse")
            }))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_task_list(cx)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.toggle_task_list(cx);
                    cx.stop_propagation();
                }
            }));

        let steps = (!collapsed).then(|| {
            div()
                .id("task-list-steps")
                .max_h(px(VISIBLE_STEPS * STEP_HEIGHT))
                .overflow_y_scroll()
                .px(px(12.0))
                .pb(px(6.0))
                .children(list.steps.iter().map(|step| task_step_row(step, theme)))
        });

        Some(
            div().flex_none().px(px(20.0)).pb(px(8.0)).child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .mx_auto()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.raised)
                    .child(header)
                    .children(steps),
            ),
        )
    }
}

/// One step. Its state is carried by the glyph's shape and the text weight,
/// never by color alone.
fn task_step_row(step: &PlanStep, theme: Theme) -> Div {
    let (glyph, glyph_color, text_color) = match step.status {
        PlanStepStatus::Completed => (
            "icons/ant/check-circle-filled.svg",
            theme.text_tertiary,
            theme.text_tertiary,
        ),
        PlanStepStatus::InProgress => {
            ("icons/ant/arrow-right-circle.svg", theme.accent, theme.text)
        }
        PlanStepStatus::Pending => (
            "icons/ant/circle-dashed.svg",
            theme.text_tertiary,
            theme.text_secondary,
        ),
    };
    div()
        .h(px(STEP_HEIGHT))
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(icon(glyph, 13.0, glyph_color))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(sp(SMALL_UI_FONT_SIZE))
                .text_color(text_color)
                .when(step.status == PlanStepStatus::InProgress, |element| {
                    element.font_weight(FontWeight::MEDIUM)
                })
                .when(step.status == PlanStepStatus::Completed, |element| {
                    element.line_through()
                })
                .child(SharedString::from(step.text.clone())),
        )
}
