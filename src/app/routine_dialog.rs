//! Session routine dialog, header trigger, and Settings -> Routines overview.

use gpui::{
    AnyElement, App, Entity, FocusHandle, KeyBinding, MouseButton, SharedString, Window, actions,
};

use super::routines::{format_routine_timestamp, routine_schedule_label};
use super::sidebar::localized_session_title;
use super::*;
use crate::model::{
    ROUTINE_MAX_CONSECUTIVE_FAILURES, RoutineRunOutcome, RoutineSchedule, SessionRoutine,
};
use crate::ui::menu::dropdown_menu_with_priority;
use crate::ui::{ActivationExt as _, motion};

actions!(
    raku_routine_dialog,
    [ConfirmRoutineDialog, DismissRoutineDialog]
);

const DIALOG_CONTEXT: &str = "RoutineDialog";
/// Same layer as the other modal dialogs.
const ROUTINE_DIALOG_PRIORITY: usize = 4;
/// The dialog's dropdowns paint above the dialog itself.
const ROUTINE_DIALOG_MENU_PRIORITY: usize = ROUTINE_DIALOG_PRIORITY + 1;
const DIALOG_INPUT_CONTEXT: &str = "RoutineDialog > TextInput";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(
            "secondary-enter",
            ConfirmRoutineDialog,
            Some(DIALOG_INPUT_CONTEXT),
        ),
        KeyBinding::new(
            "secondary-enter",
            ConfirmRoutineDialog,
            Some(DIALOG_CONTEXT),
        ),
        KeyBinding::new("escape", DismissRoutineDialog, Some(DIALOG_CONTEXT)),
    ]);
}

struct RoutineRunnerOption {
    name: Option<String>,
    label: String,
    /// False for a saved runner that was since deleted, disabled, or lost
    /// its provider. Shown so the problem is visible, not selectable.
    available: bool,
}

pub(super) struct RoutineDialogState {
    pub(super) session_id: Uuid,
    prompt: Entity<TextInput>,
    schedule: RoutineSchedule,
    /// Chosen subagent definition by name; `None` is the task's own model.
    runner: Option<String>,
    /// The runner choices as of opening.
    runner_options: Vec<RoutineRunnerOption>,
    save_focus: FocusHandle,
    check_now_focus: FocusHandle,
    toggle_focus: FocusHandle,
    remove_focus: FocusHandle,
}

impl Raku {
    pub(super) fn open_routine_dialog(
        &mut self,
        session_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        if session.is_child() {
            return;
        }
        let existing = session.routine.clone();
        let initial_prompt = existing
            .as_ref()
            .map(|routine| routine.prompt.clone())
            .unwrap_or_default();
        let schedule = existing
            .as_ref()
            .map(|routine| routine.schedule)
            .unwrap_or_default();

        // Three lines when empty, growing to nine before the prompt scrolls
        // inside the box; a fixed box let long prompts paint over the
        // schedule fields and the action bar.
        let prompt = cx.new(|cx| {
            let mut input = TextInput::new(window, cx)
                .multi_line()
                .auto_height_within(px(66.0), px(198.0))
                .placeholder(tr!("routine.prompt_placeholder"));
            input.set_padding_x(px(10.0));
            input
        });
        prompt.update(cx, |input, cx| {
            input.set_content(initial_prompt, cx);
        });
        let prompt_focus = prompt.read(cx).focus_handle(cx);
        let runner = existing.as_ref().and_then(|routine| routine.runner.clone());
        let mut runner_options = vec![RoutineRunnerOption {
            name: None,
            label: tr!("routine.runner_task"),
            available: true,
        }];
        runner_options.extend(
            self.state
                .subagents
                .iter()
                .filter(|agent| {
                    agent.enabled && self.subagent_is_available(agent, session.provider)
                })
                .map(|agent| RoutineRunnerOption {
                    name: Some(agent.name.clone()),
                    label: match agent.model.as_deref() {
                        Some(model) => format!("{} · {model}", agent.name),
                        None => agent.name.clone(),
                    },
                    available: true,
                }),
        );
        if let Some(current) = runner.as_ref()
            && !runner_options
                .iter()
                .any(|option| option.name.as_ref() == Some(current))
        {
            runner_options.push(RoutineRunnerOption {
                name: Some(current.clone()),
                label: tr!("routine.runner_unavailable", name = current),
                available: false,
            });
        }

        self.routine_dialog = Some(RoutineDialogState {
            session_id,
            prompt,
            schedule,
            runner,
            runner_options,
            save_focus: cx.focus_handle(),
            check_now_focus: cx.focus_handle(),
            toggle_focus: cx.focus_handle(),
            remove_focus: cx.focus_handle(),
        });

        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| window.focus(&prompt_focus, cx));
        });
        cx.notify();
    }

    pub(super) fn close_routine_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.routine_dialog.take().is_none() {
            return;
        }
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn set_routine_dialog_schedule(&mut self, schedule: RoutineSchedule, cx: &mut Context<Self>) {
        if let Some(dialog) = self.routine_dialog.as_mut() {
            dialog.schedule = schedule;
            cx.notify();
        }
    }

    fn set_routine_dialog_runner(&mut self, runner: Option<String>, cx: &mut Context<Self>) {
        if let Some(dialog) = self.routine_dialog.as_mut() {
            dialog.runner = runner;
            cx.notify();
        }
    }

    fn confirm_routine_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.routine_dialog.as_ref() else {
            return;
        };
        let session_id = dialog.session_id;
        let schedule = dialog.schedule;
        let runner = dialog.runner.clone();
        let prompt = dialog.prompt.read(cx).content().trim().to_owned();
        if prompt.is_empty() {
            return;
        }
        self.close_routine_dialog(window, cx);
        self.save_session_routine(session_id, prompt, schedule, runner, cx);
    }

    fn check_now_from_routine_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.routine_dialog.as_ref() else {
            return;
        };
        let session_id = dialog.session_id;
        let schedule = dialog.schedule;
        let runner = dialog.runner.clone();
        let prompt = dialog.prompt.read(cx).content().trim().to_owned();
        if prompt.is_empty() {
            return;
        }
        let needs_save = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| session.routine.as_ref())
            .is_none_or(|routine| {
                routine.prompt.trim() != prompt
                    || routine.schedule != schedule
                    || routine.runner != runner
            });
        if needs_save {
            self.save_session_routine(session_id, prompt, schedule, runner, cx);
        }
        self.close_routine_dialog(window, cx);
        self.trigger_session_routine_now(session_id, cx);
    }

    fn toggle_from_routine_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session_id) = self.routine_dialog.as_ref().map(|dialog| dialog.session_id) else {
            return;
        };
        let enabled = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| session.routine.as_ref())
            .is_some_and(|routine| routine.enabled);
        self.close_routine_dialog(window, cx);
        self.set_session_routine_enabled(session_id, !enabled, cx);
    }

    fn remove_from_routine_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session_id) = self.routine_dialog.as_ref().map(|dialog| dialog.session_id) else {
            return;
        };
        self.close_routine_dialog(window, cx);
        self.remove_session_routine(session_id, cx);
    }

    /// Header trigger for the active session's routine.
    pub(super) fn render_routine_header_button(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.selected_session()?;
        if session.is_child() {
            return None;
        }
        let session_id = session.id;
        let routine = session.routine.as_ref();
        let running = self.is_routine_running(session_id);
        let theme = Theme::current(cx);
        let focus = self.transcript_control_focus(format!("routine-header-{session_id}"), cx);
        let weak = cx.entity().downgrade();

        let tooltip = match routine {
            Some(routine) => routine_header_tooltip(routine, running),
            None => tr!("routine.header_tooltip"),
        };

        let auto_paused = |routine: &SessionRoutine| {
            !routine.enabled && routine.consecutive_failures >= ROUTINE_MAX_CONSECUTIVE_FAILURES
        };
        // Active and paused routines are just the header icon, tinted by
        // state; the schedule and next check are in the tooltip. Only a check
        // in progress and an auto-pause after failures spell themselves out.
        let trigger = if routine.is_some_and(|routine| running || auto_paused(routine)) {
            let (icon_el, label, text_color, bg_color) = if running {
                (
                    motion::spin_slow(icon("icons/loader-circle-bold.svg", 13.0, theme.accent))
                        .into_any_element(),
                    tr!("routine.header_checking"),
                    theme.accent,
                    theme.overlay,
                )
            } else {
                (
                    icon("icons/ant/clock-alert.svg", 13.0, theme.danger).into_any_element(),
                    tr!("routine.header_failed"),
                    theme.danger,
                    theme.danger_soft,
                )
            };

            div()
                .id("header-routine-button")
                .track_focus(&focus)
                .tab_index(0)
                .h(px(28.0))
                .px(px(8.0))
                .rounded(px(7.0))
                .bg(bg_color)
                .flex()
                .items_center()
                .gap(px(5.0))
                .cursor_default()
                .text_size(sp(12.0))
                .text_color(text_color)
                .hover(|style| style.bg(theme.overlay_strong))
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .tooltip(Tooltip::text(tooltip))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    let _ = weak.update(cx, |raku, cx| {
                        raku.open_routine_dialog(session_id, window, cx);
                    });
                })
                .on_key_down({
                    let weak = cx.entity().downgrade();
                    move |event, window, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            let _ = weak.update(cx, |raku, cx| {
                                raku.open_routine_dialog(session_id, window, cx);
                            });
                            cx.stop_propagation();
                        }
                    }
                })
                .child(icon_el)
                .child(SharedString::from(label))
                .into_any_element()
        } else {
            div()
                .id("header-routine-button")
                .track_focus(&focus)
                .tab_index(0)
                .size(px(28.0))
                .rounded(px(7.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor_default()
                .hover(|style| style.bg(theme.overlay))
                .active(|style| style.bg(theme.overlay_strong))
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .tooltip(Tooltip::text(tooltip))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    let _ = weak.update(cx, |raku, cx| {
                        raku.open_routine_dialog(session_id, window, cx);
                    });
                })
                .on_key_down({
                    let weak = cx.entity().downgrade();
                    move |event, window, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            let _ = weak.update(cx, |raku, cx| {
                                raku.open_routine_dialog(session_id, window, cx);
                            });
                            cx.stop_propagation();
                        }
                    }
                })
                // Same size as the neighboring header toggles (background
                // work, files), which draw ant icons at 20px. Their tint with
                // no routine; accent when active, the disabled tint when
                // paused.
                .child(icon(
                    "icons/ant/alarm-clock.svg",
                    20.0,
                    match routine {
                        Some(routine) if routine.enabled => theme.accent,
                        // Paused: the disabled tint.
                        Some(_) => theme.text_ghost,
                        None => theme.text,
                    },
                ))
                .into_any_element()
        };

        Some(
            div()
                .tab_group()
                .tab_stop(false)
                .flex_none()
                .child(trigger)
                .into_any_element(),
        )
    }

    /// Modal dialog for creating, editing, testing, pausing, or removing a
    /// session's scheduled routine.
    pub(super) fn render_routine_dialog(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.routine_dialog.as_ref()?;
        let session_id = dialog.session_id;
        let current = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| session.routine.as_ref())
            .cloned();
        let running = self.is_routine_running(session_id);
        let prompt_non_empty = !dialog.prompt.read(cx).content().trim().is_empty();
        let selected_schedule = dialog.schedule;
        let selected_runner = dialog.runner.clone();
        let theme = Theme::current(cx);
        let weak = cx.entity().downgrade();

        let status_pill = current.as_ref().map(|routine| {
            if running {
                (tr!("routine.status_checking"), theme.accent)
            } else if !routine.enabled
                && routine.consecutive_failures >= ROUTINE_MAX_CONSECUTIVE_FAILURES
            {
                (tr!("routine.status_error"), theme.danger)
            } else if routine.enabled {
                (tr!("routine.status_active"), theme.success)
            } else {
                (tr!("routine.status_paused"), theme.text_secondary)
            }
        });

        // Schedule and runner are menus rather than chip grids: nine presets
        // and every subagent as chips made the dialog mostly buttons.
        let schedule_handle = self.menu_handle("routine-schedule-menu", cx);
        let schedule_weak = weak.clone();
        let schedule_field = dropdown_menu_with_priority(
            MenuChip::new("routine-schedule")
                .label(routine_schedule_label(selected_schedule))
                .outlined()
                .background(theme.inset)
                .selected(schedule_handle.is_open())
                .w_full()
                .justify_between(),
            "routine-schedule-menu",
            &schedule_handle,
            MenuAlign::BelowLeft,
            ROUTINE_DIALOG_MENU_PRIORITY,
            move |_| {
                RoutineSchedule::PRESETS
                    .iter()
                    .map(|&preset| {
                        let weak = schedule_weak.clone();
                        MenuItem::new(routine_schedule_label(preset), move |_, cx| {
                            let _ = weak.update(cx, |raku, cx| {
                                raku.set_routine_dialog_schedule(preset, cx);
                            });
                        })
                        .selected(preset == selected_schedule)
                    })
                    .collect()
            },
        );

        let runner_handle = self.menu_handle("routine-runner-menu", cx);
        let runner_options = dialog
            .runner_options
            .iter()
            .map(|option| (option.name.clone(), option.label.clone(), option.available))
            .collect::<Vec<_>>();
        let (runner_label, runner_available) = runner_options
            .iter()
            .find(|(name, ..)| *name == selected_runner)
            .map(|(_, label, available)| (label.clone(), *available))
            .unwrap_or_else(|| (tr!("routine.runner_task"), true));
        let runner_weak = weak.clone();
        let runner_field = dropdown_menu_with_priority(
            MenuChip::new("routine-runner")
                .label(runner_label)
                .outlined()
                .background(theme.inset)
                .when(!runner_available, |chip| {
                    chip.icon("icons/alert.svg", theme.danger)
                })
                .selected(runner_handle.is_open())
                .w_full()
                .justify_between(),
            "routine-runner-menu",
            &runner_handle,
            MenuAlign::BelowLeft,
            ROUTINE_DIALOG_MENU_PRIORITY,
            move |_| {
                let mut items = Vec::new();
                for (index, (name, label, available)) in runner_options.iter().enumerate() {
                    // The task's own model first, then the subagents.
                    if index == 1 {
                        items.push(MenuItem::Separator);
                    }
                    let weak = runner_weak.clone();
                    let name = name.clone();
                    let selected = name == selected_runner;
                    items.push(
                        MenuItem::new(label.clone(), move |_, cx| {
                            let name = name.clone();
                            let _ = weak.update(cx, |raku, cx| {
                                raku.set_routine_dialog_runner(name, cx);
                            });
                        })
                        .selected(selected)
                        .disabled(!available),
                    );
                }
                items
            },
        );

        let field = |label: String, control: AnyElement| {
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(sp(12.0))
                        .text_color(theme.text_tertiary)
                        .child(label),
                )
                .child(control)
        };

        // One line of state instead of a history box: the latest outcome and
        // when the next check runs. The full log stays out of the way.
        let status = current.as_ref().map(|routine| {
            let mut parts = Vec::new();
            let latest = routine.recent_runs.last();
            match latest {
                Some(run) => {
                    let (_, _, outcome) = routine_outcome_presentation(run.outcome, &theme);
                    parts.push(format!(
                        "{outcome} · {}",
                        format_routine_timestamp(run.checked_at)
                    ));
                }
                None => parts.push(tr!("routine.never_checked")),
            }
            if let Some(next) = routine.next_run_at
                && routine.enabled
            {
                parts.push(tr!(
                    "routine.next_check",
                    time = format_routine_timestamp(next)
                ));
            }
            let (icon_path, icon_color) = latest
                .map(|run| {
                    let (path, color, _) = routine_outcome_presentation(run.outcome, &theme);
                    (path, color)
                })
                .unwrap_or(("icons/ant/clock.svg", theme.text_tertiary));
            let summary = latest.and_then(|run| run.summary.clone());
            let error = routine.last_error.clone();
            div()
                .px(px(16.0))
                .pt(px(12.0))
                .flex()
                .flex_col()
                .gap(px(3.0))
                .text_size(sp(12.0))
                .line_height(sp(16.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_color(theme.text_secondary)
                        .child(icon(icon_path, 12.0, icon_color))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(parts.join(" · "))),
                        ),
                )
                .when_some(error.or(summary), |column, detail| {
                    column.child(
                        div()
                            .pl(px(18.0))
                            .min_w_0()
                            .truncate()
                            .text_color(if routine.last_error.is_some() {
                                theme.danger
                            } else {
                                theme.text_tertiary
                            })
                            .child(SharedString::from(detail)),
                    )
                })
        });

        let mut secondary_actions = div().flex().items_center().gap(px(2.0));
        if let Some(routine) = current.as_ref() {
            let (toggle_icon, toggle_label) = if routine.enabled {
                ("icons/ant/pause.svg", tr!("routine.pause"))
            } else {
                ("icons/ant/play.svg", tr!("routine.resume"))
            };
            secondary_actions = secondary_actions
                .child(
                    icon_button("routine-dialog-toggle", toggle_icon, theme)
                        .track_focus(&dialog.toggle_focus)
                        .tab_index(0)
                        .focus_visible(|style| style.border_1().border_color(theme.accent))
                        .tooltip(Tooltip::text(toggle_label))
                        .on_activation(cx, |raku, window, cx| {
                            raku.toggle_from_routine_dialog(window, cx);
                        }),
                )
                .child(
                    div()
                        .id("routine-dialog-remove")
                        .track_focus(&dialog.remove_focus)
                        .tab_index(0)
                        .size(px(28.0))
                        .rounded(px(7.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_default()
                        .hover(|style| style.bg(theme.overlay))
                        .active(|style| style.bg(theme.overlay_strong))
                        .focus_visible(|style| style.border_1().border_color(theme.accent))
                        .tooltip(Tooltip::text(tr!("routine.remove")))
                        .child(icon("icons/trash.svg", 15.0, theme.danger))
                        .on_activation(cx, |raku, window, cx| {
                            raku.remove_from_routine_dialog(window, cx);
                        }),
                );
        }

        let check_enabled = prompt_non_empty && !running;
        let check_now = div()
            .id("routine-dialog-check-now")
            .track_focus(&dialog.check_now_focus)
            .when(check_enabled, |button| button.tab_index(0))
            .h(px(28.0))
            .px(px(12.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_default()
            .text_size(sp(SMALL_UI_FONT_SIZE))
            .text_color(if check_enabled {
                theme.text
            } else {
                theme.text_ghost
            })
            .focus_visible(|style| style.border_color(theme.accent))
            .when(running, |button| {
                button.child(motion::spin_slow(icon(
                    "icons/loader-circle-bold.svg",
                    12.0,
                    theme.text_secondary,
                )))
            })
            .child(if running {
                tr!("routine.header_checking")
            } else {
                tr!("routine.check_now")
            })
            .when(check_enabled, |button| {
                button
                    .hover(|style| style.bg(theme.overlay))
                    .active(|style| style.bg(theme.overlay_strong))
                    .on_activation(cx, |raku, window, cx| {
                        raku.check_now_from_routine_dialog(window, cx);
                    })
            });

        let save = div()
            .id("routine-dialog-save")
            .track_focus(&dialog.save_focus)
            .when(prompt_non_empty, |button| button.tab_index(0))
            .h(px(28.0))
            .px(px(12.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(theme.inverse)
            .bg(theme.inverse)
            .flex()
            .items_center()
            .gap(px(8.0))
            .cursor_default()
            .text_size(sp(SMALL_UI_FONT_SIZE))
            .text_color(theme.on_inverse)
            .when(!prompt_non_empty, |button| button.opacity(0.45))
            .focus_visible(|style| style.border_color(theme.accent))
            .child(tr!("routine.save"))
            .child(
                div()
                    .text_size(sp(11.0))
                    .opacity(0.6)
                    .child(crate::platform::primary_shortcut("⌘↩", "Ctrl+Enter")),
            )
            .when(prompt_non_empty, |button| {
                button
                    .hover(|style| style.opacity(0.9))
                    .active(|style| style.opacity(0.8))
                    .on_activation(cx, |raku, window, cx| {
                        raku.confirm_routine_dialog(window, cx);
                    })
            });

        let card = div()
            .id("routine-dialog-card")
            .key_context(DIALOG_CONTEXT)
            .on_action(cx.listener(|raku, _: &ConfirmRoutineDialog, window, cx| {
                raku.confirm_routine_dialog(window, cx);
            }))
            .on_action(cx.listener(|raku, _: &DismissRoutineDialog, window, cx| {
                raku.close_routine_dialog(window, cx);
            }))
            .tab_group()
            .tab_stop(false)
            .w_full()
            .max_w(px(468.0))
            .overflow_hidden()
            .rounded(px(18.0))
            .bg(theme.composer)
            .shadow_xl()
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(48.0))
                    .px(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .text_size(sp(14.0))
                    .text_color(theme.text)
                    .child(icon("icons/ant/alarm-clock.svg", 15.0, theme.text))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(tr!("routine.title")),
                    )
                    .when_some(status_pill, |header, (label, color)| {
                        header.child(
                            div()
                                .flex_none()
                                .px(px(7.0))
                                .py(px(2.0))
                                .rounded(px(9.0))
                                .bg(theme.overlay)
                                .text_size(sp(12.0))
                                .text_color(color)
                                .child(label),
                        )
                    }),
            )
            .child(
                div()
                    .px(px(16.0))
                    .pb(px(6.0))
                    .text_size(sp(12.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!("routine.prompt_label")),
            )
            .child(
                div()
                    .relative()
                    .mx(px(16.0))
                    .py(px(6.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(theme.inset)
                    .child(dialog.prompt.clone()),
            )
            .child(
                div()
                    .px(px(16.0))
                    .pt(px(14.0))
                    .flex()
                    .gap(px(10.0))
                    .child(field(tr!("routine.schedule_label"), schedule_field))
                    .child(field(tr!("routine.runner_label"), runner_field)),
            )
            .children(status)
            .child(div().mt(px(14.0)).h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(secondary_actions)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(check_now)
                            .child(save),
                    ),
            );

        let scrim = if theme.is_dark {
            gpui::hsla(0.0, 0.0, 0.0, 0.34)
        } else {
            gpui::hsla(0.0, 0.0, 0.0, 0.16)
        };
        let layer = div()
            .id("routine-dialog-layer")
            .absolute()
            .inset_0()
            .occlude()
            .bg(scrim)
            .p(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|raku, _, window, cx| raku.close_routine_dialog(window, cx)),
            )
            .child(card);

        Some(
            gpui::deferred(layer)
                .with_priority(ROUTINE_DIALOG_PRIORITY)
                .into_any_element(),
        )
    }

    /// Global management page under Settings -> Routines.
    pub(super) fn render_routines_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let mut entries = self
            .state
            .sessions
            .iter()
            .filter_map(|session| {
                if session.is_child() {
                    return None;
                }
                let routine = session.routine.as_ref()?;
                let project_name = self
                    .state
                    .projects
                    .iter()
                    .find(|project| project.id == session.project_id && !project.is_projectless())
                    .map(|project| project.name.clone());
                Some((
                    session.id,
                    localized_session_title(session),
                    project_name,
                    routine.clone(),
                ))
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| {
            b.3.enabled
                .cmp(&a.3.enabled)
                .then_with(|| b.3.updated_at.cmp(&a.3.updated_at))
        });

        let mut root = div().w_full().flex().flex_col().gap(px(14.0)).child(
            div()
                .text_size(sp(13.0))
                .line_height(sp(19.0))
                .text_color(theme.text_secondary)
                .child(tr!("routine.settings_description")),
        );

        if entries.is_empty() {
            root = root.child(
                div()
                    .w_full()
                    .p(px(20.0))
                    .rounded(px(13.0))
                    .bg(theme.raised)
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(icon("icons/ant/alarm-clock.svg", 18.0, theme.text_tertiary))
                    .child(
                        div()
                            .text_size(sp(13.0))
                            .line_height(sp(19.0))
                            .text_color(theme.text_secondary)
                            .child(tr!("routine.settings_empty")),
                    ),
            );
            return root.into_any_element();
        }

        let mut list = div()
            .w_full()
            .flex()
            .flex_col()
            .rounded(px(13.0))
            .overflow_hidden()
            .bg(theme.raised);

        for (index, (session_id, title, project_name, routine)) in entries.into_iter().enumerate() {
            let running = self.is_routine_running(session_id);
            let schedule_label = routine_schedule_label(routine.schedule);
            let mut subtitle_parts = Vec::new();
            if let Some(project) = project_name {
                subtitle_parts.push(project);
            }
            subtitle_parts.push(schedule_label);
            if let Some(runner) = routine.runner.as_deref() {
                subtitle_parts.push(tr!("routine.runner_via", name = runner));
            }
            if let Some(last) = routine.last_checked_at {
                subtitle_parts.push(tr!(
                    "routine.last_checked",
                    time = format_routine_timestamp(last)
                ));
            }
            if let Some(next) = routine.next_run_at
                && routine.enabled
            {
                subtitle_parts.push(tr!(
                    "routine.next_check",
                    time = format_routine_timestamp(next)
                ));
            }

            let toggle = toggle_switch(
                SharedString::from(format!("settings-routine-toggle-{session_id}")),
                routine.enabled,
                false,
                theme,
                cx,
                move |raku, _, cx| {
                    raku.set_session_routine_enabled(session_id, !routine.enabled, cx)
                },
            );

            let check_focus =
                self.transcript_control_focus(format!("settings-routine-check-{session_id}"), cx);
            let check_btn = icon_button(
                SharedString::from(format!("settings-routine-check-{session_id}")),
                if running {
                    "icons/loader-circle-bold.svg"
                } else {
                    "icons/ant/refresh.svg"
                },
                theme,
            )
            .track_focus(&check_focus)
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .tooltip(Tooltip::text(tr!("routine.check_now")))
            .on_activation(cx, move |raku, _, cx| {
                raku.trigger_session_routine_now(session_id, cx);
            });

            let edit_focus =
                self.transcript_control_focus(format!("settings-routine-edit-{session_id}"), cx);
            let edit_btn = icon_button(
                SharedString::from(format!("settings-routine-edit-{session_id}")),
                "icons/compose.svg",
                theme,
            )
            .track_focus(&edit_focus)
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .tooltip(Tooltip::text(tr!("routine.settings_edit")))
            .on_activation(cx, move |raku, window, cx| {
                raku.close_settings_popup(window, cx);
                raku.select_session(session_id, cx);
                raku.open_routine_dialog(session_id, window, cx);
            });

            let remove_focus =
                self.transcript_control_focus(format!("settings-routine-remove-{session_id}"), cx);
            let remove_btn = icon_button(
                SharedString::from(format!("settings-routine-remove-{session_id}")),
                "icons/trash.svg",
                theme,
            )
            .track_focus(&remove_focus)
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .tooltip(Tooltip::text(tr!("routine.remove")))
            .on_activation(cx, move |raku, _, cx| {
                raku.remove_session_routine(session_id, cx);
            });

            let title_focus =
                self.transcript_control_focus(format!("settings-routine-open-{session_id}"), cx);
            let title_row = div()
                .id(("settings-routine-open", index))
                .track_focus(&title_focus)
                .tab_index(0)
                .rounded(px(5.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .cursor_default()
                .text_size(sp(13.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text)
                .hover(|style| style.text_color(theme.accent))
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .tooltip(Tooltip::text(tr!("routine.settings_open_session")))
                .on_activation(cx, move |raku, window, cx| {
                    raku.close_settings_popup(window, cx);
                    raku.select_session(session_id, cx);
                })
                .child(div().min_w_0().truncate().child(SharedString::from(title)))
                .when(running, |row| {
                    row.child(motion::spin_slow(icon(
                        "icons/loader-circle-bold.svg",
                        12.0,
                        theme.accent,
                    )))
                });

            let row = div()
                .w_full()
                .px(px(18.0))
                .py(px(14.0))
                .when(index > 0, |row| row.border_t_1().border_color(theme.border))
                .flex()
                .items_start()
                .gap(px(14.0))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(title_row)
                        .child(
                            div()
                                .text_size(sp(12.0))
                                .text_color(theme.text_tertiary)
                                .truncate()
                                .child(SharedString::from(subtitle_parts.join(" · "))),
                        )
                        .child(
                            div()
                                .text_size(sp(12.5))
                                .line_height(sp(18.0))
                                .text_color(theme.text_secondary)
                                .child(SharedString::from(routine.prompt.clone())),
                        )
                        .when_some(routine.last_error.clone(), |col, error| {
                            col.child(
                                div()
                                    .text_size(sp(12.0))
                                    .text_color(theme.danger)
                                    .child(tr!("routine.settings_error_banner", error = error)),
                            )
                        }),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(check_btn)
                        .child(edit_btn)
                        .child(remove_btn)
                        .child(toggle),
                );

            list = list.child(row);
        }

        root.child(list).into_any_element()
    }
}

fn routine_header_tooltip(routine: &SessionRoutine, running: bool) -> String {
    let mut parts = Vec::new();
    if running {
        parts.push(tr!("routine.header_checking"));
    } else if routine.enabled {
        parts.push(routine_schedule_label(routine.schedule));
    } else if routine.consecutive_failures >= ROUTINE_MAX_CONSECUTIVE_FAILURES {
        parts.push(tr!("routine.status_error"));
    } else {
        parts.push(tr!("routine.status_paused"));
    }
    if let Some(last) = routine.last_checked_at {
        parts.push(tr!(
            "routine.last_checked",
            time = format_routine_timestamp(last)
        ));
    }
    if routine.enabled
        && let Some(next) = routine.next_run_at
    {
        parts.push(tr!(
            "routine.next_check",
            time = format_routine_timestamp(next)
        ));
    }
    parts.join(" · ")
}

fn routine_outcome_presentation(
    outcome: RoutineRunOutcome,
    theme: &Theme,
) -> (&'static str, gpui::Hsla, String) {
    match outcome {
        RoutineRunOutcome::Updated => (
            "icons/check.svg",
            theme.success,
            tr!("routine.outcome_updated"),
        ),
        RoutineRunOutcome::NoUpdate => (
            "icons/ant/clock.svg",
            theme.text_tertiary,
            tr!("routine.outcome_no_update"),
        ),
        RoutineRunOutcome::Failed => (
            "icons/alert.svg",
            theme.danger,
            tr!("routine.outcome_failed"),
        ),
        RoutineRunOutcome::SkippedBusy => (
            "icons/hourglass.svg",
            theme.warning,
            tr!("routine.outcome_skipped_busy"),
        ),
    }
}
