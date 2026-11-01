//! The project picker behind the sidebar, empty-state headline and composer's
//! project chip: a filter field over a virtualized project list, with the
//! add-project and no-project actions pinned beneath it so a long list can
//! never push them out of reach.
//!
//! The triggers never have their panels open together, so they share one
//! filter field, one keyboard cursor and one list. Rows are only ranked while
//! a panel is open; a closed picker costs what any other closed menu does.

use gpui::ElementId;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};

use super::composer::next_picker_highlight;
use super::*;

const PROJECT_PICKER_ROW_HEIGHT: f32 = 26.0;
/// Rows drawn before the list scrolls. Whole rows only: the edge fade is the
/// cue that more follow.
const PROJECT_PICKER_VISIBLE_ROWS: f32 = 11.0;
const PROJECT_PICKER_WIDTH: f32 = 300.0;

/// Where a picker is drawn, which decides its anchoring and how a choice
/// lands. Choices configuring an unsent task carry its draft to the new project.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProjectPickerSite {
    Sidebar,
    EmptyState,
    Composer,
}

impl ProjectPickerSite {
    fn menu_id(self) -> &'static str {
        match self {
            Self::Sidebar => "sidebar-project",
            Self::EmptyState => "empty-state-project",
            Self::Composer => "workspace-project",
        }
    }

    fn align(self) -> MenuAlign {
        match self {
            Self::EmptyState => MenuAlign::BelowLeft,
            // Both footers sit at the bottom of the window.
            Self::Sidebar | Self::Composer => MenuAlign::AboveLeft,
        }
    }
}

/// A project row in the picker, with its activity-derived ordering and ongoing status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ProjectPickerEntry {
    pub id: Uuid,
    pub name: SharedString,
    pub status: Option<SessionStatus>,
    pub finished: bool,
}

/// A keyboard-reachable target, in drawn order: each project and its remove
/// button, then the two pinned actions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectPickerAction {
    Select(Uuid),
    Remove(Uuid),
    NewProject,
    NoProject,
}

impl Waku {
    /// Both footer selectors reflect the same active project, including an
    /// explicit label for a task without a project.
    pub(super) fn project_picker_label(&self) -> String {
        self.selected_project()
            .map(|project| {
                if project.is_projectless() {
                    tr!("project.no_project_name")
                } else {
                    project.display_name()
                }
            })
            .unwrap_or_else(|| tr!("project.choose_project"))
    }

    /// The menu handle for `site`'s picker. Opening resets the filter and the
    /// keyboard cursor and hands focus to the field; closing returns focus to
    /// the sidebar trigger or the composer.
    pub(super) fn project_picker_handle(
        &self,
        site: ProjectPickerSite,
        cx: &mut Context<Self>,
    ) -> ContextMenuHandle {
        let weak = cx.entity().downgrade();
        let search = self.project_search.clone();
        let search_focus = search.read(cx).focus_handle(cx);
        self.menu_handle_with(site.menu_id(), cx, move |open, window, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.project_picker_highlight = None;
                if open {
                    // Forces the next sync to reset the list, showing the
                    // most recently changed projects at the top.
                    this.project_picker_row_cache.borrow_mut().clear();
                    search.update(cx, |input, cx| input.clear(cx));
                } else {
                    let focus = match site {
                        ProjectPickerSite::Sidebar => this
                            .menus
                            .borrow()
                            .get(site.menu_id())
                            .map(|handle| handle.trigger_focus_handle().clone()),
                        _ => Some(this.composer_focus(cx)),
                    };
                    if let Some(focus) = focus {
                        window.focus(&focus, cx);
                    }
                }
                cx.notify();
            });
            if open {
                let search_focus = search_focus.clone();
                window.on_next_frame(move |window, _| {
                    window.on_next_frame(move |window, cx| window.focus(&search_focus, cx));
                });
            }
        })
    }

    pub(super) fn render_project_picker<E>(
        &self,
        trigger: E,
        handle: &ContextMenuHandle,
        site: ProjectPickerSite,
        cx: &mut Context<Self>,
    ) -> AnyElement
    where
        E: ParentElement + Styled + InteractiveElement + IntoElement + 'static,
    {
        let theme = Theme::current(cx);
        let selected_project = self.state.selected_project;
        let projectless_selected = self.selected_project().is_some_and(Project::is_projectless);
        let projects = Rc::new(if handle.is_open() {
            visible_project_entries(
                &self.state.projects,
                &self.state.sessions,
                &self.state.removed_project_ids,
                &self.finished_project_ids,
                self.project_search.read(cx).content(),
                &mut self.project_picker_matcher.borrow_mut(),
            )
        } else {
            Vec::new()
        });
        if handle.is_open() {
            self.sync_project_picker_rows(&projects);
        }
        let actions = Rc::new(
            projects
                .iter()
                .flat_map(|entry| {
                    [
                        ProjectPickerAction::Select(entry.id),
                        ProjectPickerAction::Remove(entry.id),
                    ]
                })
                .chain([
                    ProjectPickerAction::NewProject,
                    ProjectPickerAction::NoProject,
                ])
                .collect::<Vec<_>>(),
        );
        let highlight = self
            .project_picker_highlight
            .filter(|index| *index < actions.len());
        let weak = cx.entity().downgrade();
        let search = self.project_search.clone();
        let list_state = self.project_picker_list_state.clone();
        let scrollbar_state = self.project_picker_scrollbar.clone();

        popover(
            trigger,
            handle,
            site.align(),
            move |popover, _window, _cx| {
                let rows = if projects.is_empty() {
                    div()
                        .h(px(64.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(sp(12.5))
                        .text_color(theme.text_ghost)
                        .child(tr!("project.none_found"))
                        .into_any_element()
                } else {
                    let list_projects = projects.clone();
                    let list_weak = weak.clone();
                    let list_popover = popover.clone();
                    let height = (projects.len() as f32).min(PROJECT_PICKER_VISIBLE_ROWS)
                        * PROJECT_PICKER_ROW_HEIGHT;
                    div()
                        .id("project-picker-list")
                        .relative()
                        .w_full()
                        .h(px(height))
                        .flex_none()
                        .px(px(4.0))
                        .child(
                            list(list_state.clone(), move |index, _window, _cx| {
                                let Some(entry) = list_projects.get(index) else {
                                    return div().into_any_element();
                                };
                                let project_id = entry.id;
                                let name = entry.name.clone();
                                let status = entry.status;
                                let finished = entry.finished;
                                let select_weak = list_weak.clone();
                                let remove_weak = list_weak.clone();
                                let remove_key_weak = list_weak.clone();
                                let select_popover = list_popover.clone();
                                project_picker_row(
                                    ("project-row", index),
                                    highlight == Some(index * 2),
                                    &theme,
                                )
                                .group("project-picker-row")
                                .w_full()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.0))
                                        .min_w_0()
                                        .flex_1()
                                        .child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .text_color(theme.text)
                                                .child(name),
                                        )
                                        .when_some(
                                            status.and_then(|s| {
                                                project_picker_status_icon(s, &theme)
                                            }),
                                            |element, icon| {
                                                element.child(div().flex_none().child(icon))
                                            },
                                        ),
                                )
                                .child(
                                    div()
                                        .relative()
                                        .size(px(22.0))
                                        .flex_none()
                                        .when(
                                            selected_project == Some(project_id)
                                                && highlight != Some(index * 2 + 1),
                                            |element| {
                                                element.child(
                                                    div()
                                                        .absolute()
                                                        .inset_0()
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .group_hover(
                                                            "project-picker-row",
                                                            |style| style.invisible(),
                                                        )
                                                        .child(icon(
                                                            "icons/check.svg",
                                                            11.0,
                                                            theme.text_secondary,
                                                        )),
                                                )
                                            },
                                        )
                                        .when(
                                            selected_project != Some(project_id)
                                                && finished
                                                && highlight != Some(index * 2 + 1),
                                            |element| {
                                                element.child(
                                                    div()
                                                        .absolute()
                                                        .inset_0()
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .group_hover(
                                                            "project-picker-row",
                                                            |style| style.invisible(),
                                                        )
                                                        .child(
                                                            div()
                                                                .size(px(6.0))
                                                                .rounded_full()
                                                                .bg(theme.accent),
                                                        ),
                                                )
                                            },
                                        )
                                        .child(
                                            icon_button(
                                                ("project-remove", index),
                                                "icons/x.svg",
                                                theme,
                                            )
                                            .tab_index(index as isize)
                                            .opacity(0.0)
                                            .group_hover("project-picker-row", |style| {
                                                style.opacity(1.0)
                                            })
                                            .focus_visible(|style| {
                                                style
                                                    .opacity(1.0)
                                                    .bg(theme.raised)
                                                    .border_1()
                                                    .border_color(theme.accent)
                                            })
                                            .when(highlight == Some(index * 2 + 1), |element| {
                                                element.opacity(1.0).bg(theme.overlay_strong)
                                            })
                                            .tooltip(Tooltip::text(tr!("project.remove")))
                                            .on_click(move |_, _, cx| {
                                                cx.stop_propagation();
                                                let _ = remove_weak.update(cx, |this, cx| {
                                                    this.remove_project_from_picker(project_id, cx);
                                                });
                                            })
                                            .on_key_down(move |event: &KeyDownEvent, _, cx| {
                                                if !event.keystroke.modifiers.modified()
                                                    && matches!(
                                                        event.keystroke.key.as_str(),
                                                        "enter" | "space"
                                                    )
                                                {
                                                    cx.stop_propagation();
                                                    let _ =
                                                        remove_key_weak.update(cx, |this, cx| {
                                                            this.remove_project_from_picker(
                                                                project_id, cx,
                                                            );
                                                        });
                                                }
                                            }),
                                        ),
                                )
                                .on_click(move |_, window, cx| {
                                    select_popover.close(window, cx);
                                    window.refresh();
                                    let _ = select_weak.update(cx, |this, cx| {
                                        this.apply_project_picker_action(
                                            ProjectPickerAction::Select(project_id),
                                            site,
                                            cx,
                                        );
                                    });
                                })
                                .into_any_element()
                            })
                            .size_full(),
                        )
                        .child(scrollbar::edge_fade(
                            list_state.clone(),
                            scrollbar::FadeEdge::Top,
                            theme.raised,
                        ))
                        .child(scrollbar::edge_fade(
                            list_state.clone(),
                            scrollbar::FadeEdge::Bottom,
                            theme.raised,
                        ))
                        .child(scrollbar::vertical(&list_state, &scrollbar_state))
                        .into_any_element()
                };

                let pinned_row = |action: ProjectPickerAction,
                                  id: &'static str,
                                  icon_path: &'static str,
                                  label: String,
                                  selected: bool| {
                    let weak = weak.clone();
                    let popover = popover.clone();
                    let index = actions.iter().position(|candidate| *candidate == action);
                    project_picker_row(id, highlight.is_some() && highlight == index, &theme)
                        .mx(px(4.0))
                        .child(icon(icon_path, 12.0, theme.text_secondary))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_color(theme.text)
                                .child(label),
                        )
                        .when(selected, |element| {
                            element.child(icon("icons/check.svg", 11.0, theme.text_secondary))
                        })
                        .on_click(move |_, window, cx| {
                            popover.close(window, cx);
                            window.refresh();
                            let _ = weak.update(cx, |this, cx| {
                                this.apply_project_picker_action(action, site, cx);
                            });
                        })
                };

                let navigate = |key: &'static str| {
                    let weak = weak.clone();
                    let row_count = projects.len();
                    move |cx: &mut App| {
                        let _ = weak.update(cx, |this, cx| {
                            this.move_project_picker_highlight(key, row_count, cx);
                        });
                    }
                };
                let next = navigate("down");
                let previous = navigate("up");
                let next_tab = navigate("tab");
                let previous_tab = navigate("shift-tab");
                let confirm_weak = weak.clone();
                let confirm_actions = actions.clone();
                let confirm_popover = popover.clone();

                div()
                    .tab_group()
                    .tab_stop(false)
                    .w(px(PROJECT_PICKER_WIDTH))
                    // A deferred panel inherits its trigger's text style, and
                    // the empty state's trigger sits in a 20pt medium
                    // headline. Pin the rows' metrics for the filter field.
                    .text_size(sp(12.5))
                    .line_height(sp(15.0))
                    .font_weight(FontWeight::NORMAL)
                    .rounded(px(13.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(theme.raised)
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .on_action(move |_: &SelectNextEntry, _, cx| next(cx))
                    .on_action(move |_: &SelectPreviousEntry, _, cx| previous(cx))
                    // Arrows select rows; Tab also visits each remove button.
                    .on_action(move |_: &SelectNextTab, _, cx| next_tab(cx))
                    .on_action(move |_: &SelectPreviousTab, _, cx| previous_tab(cx))
                    .on_action(move |_: &ConfirmEntry, window, cx| {
                        // Read the live cursor: a keystroke can arrive before
                        // the frame that redraws the last move.
                        let action = confirm_weak
                            .update(cx, |this, _| {
                                confirm_actions
                                    .get(this.project_picker_highlight.unwrap_or(0))
                                    .copied()
                            })
                            .ok()
                            .flatten();
                        let Some(action) = action else {
                            return;
                        };
                        // Close before applying: the toggle observer updates
                        // `Waku`, so it cannot run inside the update below.
                        if !matches!(action, ProjectPickerAction::Remove(_)) {
                            confirm_popover.close(window, cx);
                            window.refresh();
                        }
                        let _ = confirm_weak.update(cx, |this, cx| {
                            this.apply_project_picker_action(action, site, cx);
                        });
                    })
                    .child(
                        div()
                            .px(px(12.0))
                            .pt(px(10.0))
                            .pb(px(8.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .w_full()
                                    .h(px(28.0))
                                    .px(px(10.0))
                                    .rounded(px(9.0))
                                    .bg(theme.surface)
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(icon("icons/search.svg", 12.0, theme.text_secondary))
                                    .child(div().flex_1().min_w_0().child(search.clone())),
                            ),
                    )
                    .child(rows)
                    .child(div().mx(px(6.0)).my(px(4.0)).h(px(1.0)).bg(theme.border))
                    .child(pinned_row(
                        ProjectPickerAction::NewProject,
                        "project-picker-new",
                        "icons/folder-new.svg",
                        tr!("project.new_project"),
                        false,
                    ))
                    .child(pinned_row(
                        ProjectPickerAction::NoProject,
                        "project-picker-none",
                        "icons/x.svg",
                        tr!("project.no_project"),
                        projectless_selected,
                    ))
                    .child(div().h(px(4.0)))
                    .into_any_element()
            },
        )
    }

    /// Keep the virtualized list's item count in step with the filtered rows.
    /// Rows have one fixed height, so a changed set only needs its count.
    fn sync_project_picker_rows(&self, rows: &[ProjectPickerEntry]) {
        let mut cached = self.project_picker_row_cache.borrow_mut();
        if cached.iter().eq(rows.iter().map(|entry| &entry.id)) {
            return;
        }
        *cached = rows.iter().map(|entry| entry.id).collect();
        self.project_picker_list_state
            .reset_with_uniform_height(rows.len(), px(PROJECT_PICKER_ROW_HEIGHT));
    }

    /// Move the drawn selection. The filter field keeps focus throughout, so
    /// typing continues to narrow the list.
    fn move_project_picker_highlight(
        &mut self,
        key: &str,
        row_count: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(next) =
            next_project_picker_highlight(self.project_picker_highlight, row_count, key)
        else {
            return;
        };
        self.project_picker_highlight = Some(next);
        // The pinned actions sit outside the list and are always visible.
        if next < row_count * 2 {
            self.project_picker_list_state
                .scroll_to_reveal_item(next / 2);
        }
        cx.notify();
    }

    fn apply_project_picker_action(
        &mut self,
        action: ProjectPickerAction,
        site: ProjectPickerSite,
        cx: &mut Context<Self>,
    ) {
        // A sidebar choice configures an unsent task just like the editor's
        // picker. From an existing conversation it starts a task in the new
        // project, leaving that conversation's draft in place.
        let carry_draft = site == ProjectPickerSite::Composer
            || (site == ProjectPickerSite::Sidebar
                && self
                    .selected_session()
                    .is_some_and(|session| !session.has_started() && !session.is_busy()));
        match action {
            ProjectPickerAction::Select(project_id) => {
                if self.state.selected_project == Some(project_id) {
                    return;
                }
                if carry_draft {
                    self.select_project_from_composer(project_id, cx);
                } else {
                    self.select_project(project_id, cx);
                }
            }
            ProjectPickerAction::NewProject => self.add_project(cx),
            ProjectPickerAction::Remove(project_id) => {
                self.remove_project_from_picker(project_id, cx);
            }
            ProjectPickerAction::NoProject => {
                if self.selected_project().is_some_and(Project::is_projectless) {
                    return;
                }
                if carry_draft {
                    self.create_projectless_session_from_composer(cx);
                } else {
                    self.create_projectless_session(cx);
                }
            }
        }
    }

    fn remove_project_from_picker(&mut self, project_id: Uuid, cx: &mut Context<Self>) {
        if !self
            .state
            .projects
            .iter()
            .any(|project| project.id == project_id && !project.is_projectless())
            || !self.state.removed_project_ids.insert(project_id)
        {
            return;
        }
        self.finished_project_ids.remove(&project_id);
        self.finished_session_ids.retain(|id| {
            self.state
                .sessions
                .iter()
                .any(|session| session.id == *id && session.project_id != project_id)
        });
        // Keep the row cache in step with this deletion so the next render
        // does not reset the whole list and discard its scroll position.
        remove_project_picker_row(
            project_id,
            &mut self.project_picker_row_cache.borrow_mut(),
            &self.project_picker_list_state,
        );
        self.project_picker_highlight = None;
        self.save();
        cx.notify();
    }
}

fn remove_project_picker_row(project_id: Uuid, rows: &mut Vec<Uuid>, list: &ListState) {
    if let Some(index) = rows.iter().position(|id| *id == project_id) {
        rows.remove(index);
        list.splice(index..index + 1, 0);
    }
}

/// Arrows keep the existing row navigation. Tab visits the inline remove
/// controls too, with the search field retaining focus for filtering.
fn next_project_picker_highlight(
    current: Option<usize>,
    row_count: usize,
    key: &str,
) -> Option<usize> {
    let current = current.filter(|index| *index < row_count * 2 + 2);
    match key {
        "tab" => next_picker_highlight(current, row_count * 2 + 2, "down"),
        "shift-tab" => next_picker_highlight(current, row_count * 2 + 2, "up"),
        "up" | "down" => {
            let row = current.map(|index| {
                if index < row_count * 2 {
                    index / 2
                } else {
                    index - row_count
                }
            });
            next_picker_highlight(row, row_count + 2, key).map(|index| {
                if index < row_count {
                    index * 2
                } else {
                    index + row_count
                }
            })
        }
        _ => None,
    }
}

/// The shared row: fixed height for the virtualized list, hover, and the
/// keyboard cursor drawn beneath it.
fn project_picker_row(id: impl Into<ElementId>, highlighted: bool, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(PROJECT_PICKER_ROW_HEIGHT))
        .px(px(8.0))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .cursor_default()
        .text_size(sp(12.5))
        .line_height(sp(15.0))
        .when(highlighted, |element| element.bg(theme.overlay_strong))
        .hover(|element| element.bg(theme.overlay))
        .active(|element| element.opacity(0.85))
}

/// The picker's project rows. Without a query that is every real project,
/// projects with ongoing sessions first, then newest session activity; with one,
/// only fuzzy matches, best first. The matcher has no length penalty, so equal
/// scores fall back to fzf's default tiebreak — the shorter name, which puts
/// an exact name above its longer namesakes — and then to the unfiltered order.
fn visible_project_entries(
    projects: &[Project],
    sessions: &[AgentSession],
    removed_project_ids: &HashSet<Uuid>,
    finished_project_ids: &HashSet<Uuid>,
    query: &str,
    matcher: &mut Matcher,
) -> Vec<ProjectPickerEntry> {
    // Aggregate lightweight session metadata once for the whole open picker,
    // never from its virtualized row builders or by hydrating transcripts.
    let mut last_changed = HashMap::<Uuid, u64>::with_capacity(projects.len());
    let mut project_status = HashMap::<Uuid, SessionStatus>::with_capacity(projects.len());
    for session in sessions {
        if !session.has_started() {
            continue;
        }
        let timestamp = session.last_reply_at.unwrap_or(session.created_at);
        let latest = last_changed.entry(session.project_id).or_default();
        *latest = (*latest).max(timestamp);

        if session.is_busy() {
            let current = project_status.get(&session.project_id).copied();
            if let Some(status) = resolve_ongoing_status(current, session.status) {
                project_status.insert(session.project_id, status);
            }
        }
    }
    let mut ordered = projects
        .iter()
        .filter(|project| !project.is_projectless() && !removed_project_ids.contains(&project.id))
        .collect::<Vec<_>>();
    // Stable ties preserve saved order; projects with ongoing activity lead,
    // followed by recently active projects. Projects without sessions follow
    // projects with activity. Selection never overrides session recency.
    ordered.sort_by_key(|project| {
        let is_ongoing = project_status.contains_key(&project.id);
        let recency = last_changed.get(&project.id).copied().unwrap_or_default();
        (std::cmp::Reverse(is_ongoing), std::cmp::Reverse(recency))
    });
    let query = query.trim();
    if query.is_empty() {
        return ordered
            .into_iter()
            .map(|project| ProjectPickerEntry {
                id: project.id,
                name: project.display_name().into(),
                status: project_status.get(&project.id).copied(),
                finished: finished_project_ids.contains(&project.id)
                    && !project_status.contains_key(&project.id),
            })
            .collect();
    }

    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut utf32 = Vec::new();
    let mut scored = ordered
        .into_iter()
        .filter_map(|project| {
            let name = project.display_name();
            let score = pattern.score(Utf32Str::new(&name, &mut utf32), matcher)?;
            Some((
                score,
                name.chars().count(),
                ProjectPickerEntry {
                    id: project.id,
                    name: SharedString::from(name),
                    status: project_status.get(&project.id).copied(),
                    finished: finished_project_ids.contains(&project.id)
                        && !project_status.contains_key(&project.id),
                },
            ))
        })
        .collect::<Vec<_>>();
    // Stable, so full ties hold their unfiltered order.
    scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    scored.into_iter().map(|(_, _, entry)| entry).collect()
}

fn resolve_ongoing_status(
    current: Option<SessionStatus>,
    incoming: SessionStatus,
) -> Option<SessionStatus> {
    if !incoming.is_busy() {
        return current;
    }
    match (current, incoming) {
        (None, status) => Some(status),
        (Some(SessionStatus::Connecting | SessionStatus::Working), _) => current,
        (_, SessionStatus::Connecting | SessionStatus::Working) => Some(incoming),
        (Some(SessionStatus::Waiting), _) => current,
        (_, SessionStatus::Waiting) => Some(incoming),
        _ => Some(incoming),
    }
}

fn project_picker_status_icon(status: SessionStatus, theme: &Theme) -> Option<AnyElement> {
    match status {
        SessionStatus::Connecting | SessionStatus::Working => Some(motion::spin_slow(icon(
            "icons/loader-circle.svg",
            12.0,
            status_color(theme, status),
        ))),
        SessionStatus::Background => {
            Some(icon("icons/hourglass.svg", 12.0, status_color(theme, status)).into_any_element())
        }
        SessionStatus::Waiting => {
            Some(icon("icons/alert.svg", 12.0, status_color(theme, status)).into_any_element())
        }
        SessionStatus::Idle | SessionStatus::Failed => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn project(name: &str) -> Project {
        Project::from_path(PathBuf::from("/work").join(name))
    }

    fn names(entries: &[ProjectPickerEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.name.as_ref()).collect()
    }

    #[test]
    fn project_picker_orders_by_each_projects_latest_session_change() {
        let projects = vec![
            project("before.town"),
            project("wikis"),
            project("crow"),
            project("no-sessions"),
        ];
        let mut sessions = vec![
            session(&projects[0], 40),
            session(&projects[1], 60),
            session(&projects[0], 80),
            session(&projects[2], 20),
        ];
        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let entries = visible_project_entries(
            &projects,
            &sessions,
            &HashSet::new(),
            &HashSet::new(),
            "",
            &mut matcher,
        );
        assert_eq!(
            names(&entries),
            vec!["before.town", "wikis", "crow", "no-sessions"]
        );

        // A change to an older session promotes its project on the next render.
        sessions[3].last_reply_at = Some(100);
        sessions[3].updated_at = 100;
        let entries = visible_project_entries(
            &projects,
            &sessions,
            &HashSet::new(),
            &HashSet::new(),
            "",
            &mut matcher,
        );
        assert_eq!(
            names(&entries),
            vec!["crow", "before.town", "wikis", "no-sessions"]
        );
    }

    #[test]
    fn unstarted_draft_sessions_do_not_promote_projects() {
        let projects = vec![project("active-project"), project("draft-project")];
        let mut draft = AgentSession::new(projects[1].id, ProviderKind::Codex);
        draft.created_at = 100;
        draft.updated_at = 100;
        draft.last_reply_at = Some(100);
        assert!(!draft.has_started());

        let sessions = vec![session(&projects[0], 50), draft];
        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let entries = visible_project_entries(
            &projects,
            &sessions,
            &HashSet::new(),
            &HashSet::new(),
            "",
            &mut matcher,
        );
        assert_eq!(names(&entries), vec!["active-project", "draft-project"]);
    }

    #[test]
    fn ongoing_sessions_lead_the_project_picker_and_show_status() {
        let projects = vec![
            project("recent-idle"),
            project("older-ongoing"),
            project("oldest-idle"),
        ];
        let mut ongoing_session = session(&projects[1], 20);
        ongoing_session.status = SessionStatus::Working;

        let sessions = vec![
            session(&projects[0], 100),
            ongoing_session,
            session(&projects[2], 10),
        ];
        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let entries = visible_project_entries(
            &projects,
            &sessions,
            &HashSet::new(),
            &HashSet::new(),
            "",
            &mut matcher,
        );
        assert_eq!(
            names(&entries),
            vec!["older-ongoing", "recent-idle", "oldest-idle"]
        );
        assert_eq!(entries[0].status, Some(SessionStatus::Working));
        assert_eq!(entries[1].status, None);
        assert_eq!(entries[2].status, None);
    }

    #[test]
    fn finished_projects_show_finished_dot_flag() {
        let projects = vec![project("finished-project"), project("idle-project")];
        let sessions = vec![session(&projects[0], 20), session(&projects[1], 10)];
        let finished = HashSet::from([projects[0].id]);
        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let entries = visible_project_entries(
            &projects,
            &sessions,
            &HashSet::new(),
            &finished,
            "",
            &mut matcher,
        );
        assert!(entries[0].finished);
        assert!(!entries[1].finished);
    }

    fn session(project: &Project, timestamp: u64) -> AgentSession {
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.push_message(MessageRole::User, "Started task");
        session.created_at = timestamp;
        session.last_reply_at = Some(timestamp);
        session.updated_at = timestamp;
        session
    }

    #[test]
    fn project_picker_query_keeps_fuzzy_matches_best_first() {
        let projects = vec![
            project("crow-companion"),
            project("speech-to-subtitle"),
            project("crow"),
            project("waku-crow-sync"),
        ];
        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let sessions = vec![session(&projects[0], 80), session(&projects[2], 20)];
        let entries = visible_project_entries(
            &projects,
            &sessions,
            &HashSet::new(),
            &HashSet::new(),
            "  CROW ",
            &mut matcher,
        );
        // Every match survives, the exact name ranks above its longer
        // namesakes, and the non-matching project is gone.
        assert_eq!(
            names(&entries),
            vec!["crow", "crow-companion", "waku-crow-sync"]
        );

        let entries = visible_project_entries(
            &projects,
            &[],
            &HashSet::new(),
            &HashSet::new(),
            "sts",
            &mut matcher,
        );
        assert_eq!(names(&entries), vec!["speech-to-subtitle"]);

        assert!(
            visible_project_entries(
                &projects,
                &[],
                &HashSet::new(),
                &HashSet::new(),
                "zzz",
                &mut matcher
            )
            .is_empty()
        );
    }

    #[test]
    fn project_picker_ties_keep_the_unfiltered_order() {
        let projects = vec![project("alpha-one"), project("alpha-two")];
        let sessions = vec![session(&projects[0], 20), session(&projects[1], 80)];
        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        // Session recency breaks equal fuzzy scores and equal name lengths.
        let entries = visible_project_entries(
            &projects,
            &sessions,
            &HashSet::new(),
            &HashSet::new(),
            "alpha",
            &mut matcher,
        );
        assert_eq!(names(&entries), vec!["alpha-two", "alpha-one"]);
    }

    #[test]
    fn removed_projects_stay_out_of_the_picker_even_with_recent_activity_or_search() {
        let projects = vec![project("crow"), project("waku")];
        let sessions = vec![session(&projects[0], 80), session(&projects[1], 20)];
        let mut removed = HashSet::from([projects[0].id]);
        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let entries = visible_project_entries(
            &projects,
            &sessions,
            &removed,
            &HashSet::new(),
            "",
            &mut matcher,
        );
        assert_eq!(names(&entries), vec!["waku"]);
        assert!(
            visible_project_entries(
                &projects,
                &sessions,
                &removed,
                &HashSet::new(),
                "crow",
                &mut matcher
            )
            .is_empty()
        );
        removed.remove(&projects[0].id);
        let entries = visible_project_entries(
            &projects,
            &sessions,
            &removed,
            &HashSet::new(),
            "crow",
            &mut matcher,
        );
        assert_eq!(names(&entries), vec!["crow"]);
    }

    #[test]
    fn removing_a_project_preserves_the_picker_viewport() {
        let projects = (0..40)
            .map(|index| project(&format!("project-{index}")))
            .collect::<Vec<_>>();

        for (removed_index, expected_top) in [(25, 20), (5, 19), (20, 20), (39, 20)] {
            let mut rows = projects
                .iter()
                .map(|project| project.id)
                .collect::<Vec<_>>();
            let list = ListState::new(rows.len(), ListAlignment::Top, px(152.0))
                .with_uniform_item_height(px(PROJECT_PICKER_ROW_HEIGHT));
            list.scroll_to(ListOffset {
                item_ix: 20,
                offset_in_item: px(7.0),
            });

            let removed_id = rows[removed_index];
            remove_project_picker_row(removed_id, &mut rows, &list);

            assert_eq!(list.item_count(), projects.len() - 1);
            let offset = list.logical_scroll_top();
            assert_eq!(offset.item_ix, expected_top);
            assert_eq!(
                offset.offset_in_item,
                if removed_index == 20 {
                    px(0.0)
                } else {
                    px(7.0)
                }
            );
            // The next render sees the same row IDs, so it skips the full
            // reset that used to jump this viewport back to the first row.
            let entries = visible_project_entries(
                &projects,
                &[],
                &HashSet::from([removed_id]),
                &HashSet::new(),
                "",
                &mut Matcher::new(nucleo_matcher::Config::DEFAULT),
            );
            assert!(rows.iter().eq(entries.iter().map(|entry| &entry.id)));
        }
    }

    #[test]
    fn arrows_skip_remove_buttons_while_tab_visits_them() {
        // Two projects, then New project and No project.
        assert_eq!(next_project_picker_highlight(None, 2, "down"), Some(0));
        assert_eq!(next_project_picker_highlight(Some(0), 2, "tab"), Some(1));
        assert_eq!(next_project_picker_highlight(Some(1), 2, "down"), Some(2));
        assert_eq!(next_project_picker_highlight(Some(2), 2, "up"), Some(0));
        assert_eq!(next_project_picker_highlight(Some(2), 2, "tab"), Some(3));
        assert_eq!(next_project_picker_highlight(Some(2), 2, "down"), Some(4));
        assert_eq!(next_project_picker_highlight(Some(4), 2, "down"), Some(5));
        assert_eq!(next_project_picker_highlight(Some(5), 2, "down"), Some(0));
        assert_eq!(next_project_picker_highlight(Some(4), 2, "up"), Some(2));
        assert_eq!(
            next_project_picker_highlight(Some(0), 2, "shift-tab"),
            Some(5)
        );
        assert_eq!(next_project_picker_highlight(None, 0, "down"), Some(0));
        assert_eq!(next_project_picker_highlight(Some(0), 0, "down"), Some(1));
    }
}
