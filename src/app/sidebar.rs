use chrono::{Datelike, NaiveDate, TimeZone};
use gpui::{KeyBinding, actions};

use super::*;

actions!(waku_sidebar, [CancelSessionRename]);

/// Select a session by its zero-based position in the sidebar history.
#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = waku_sidebar, no_json)]
pub(super) struct SelectSidebarSession(pub usize);

const SESSION_SHORTCUT_COUNT: usize = 9;
const SESSION_RENAME_PARENT_CONTEXT: &str = "SessionRename";
const SESSION_RENAME_FIELD_CONTEXT: &str = "SessionRename > TextInput";

/// Keep Escape inside the focused inline rename editor so it cancels the
/// rename rather than falling through to the window-wide Stop.
pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "escape",
        CancelSessionRename,
        Some(SESSION_RENAME_FIELD_CONTEXT),
    )]);
    cx.bind_keys((0..SESSION_SHORTCUT_COUNT).map(|index| {
        KeyBinding::new(
            &format!("secondary-{}", index + 1),
            SelectSidebarSession(index),
            Some("Waku"),
        )
    }));
}

/// Height of a session card plus the separation reserved beneath it in the
/// virtualized sidebar list. Keep the gap inside the list row so measured and
/// estimated heights stay identical for off-screen sessions.
const SIDEBAR_SESSION_CARD_HEIGHT: f32 = 32.0;
const SIDEBAR_SESSION_ROW_GAP: f32 = 1.0;
const SIDEBAR_SESSION_ROW_HEIGHT: f32 = SIDEBAR_SESSION_CARD_HEIGHT + SIDEBAR_SESSION_ROW_GAP;
const SIDEBAR_PROJECT_SELECTOR_HEIGHT: f32 = 36.0;
const TITLEBAR_BUTTON_SIZE: f32 = 26.0;
const TITLEBAR_NAVIGATION_GAP: f32 = 6.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SidebarDateGroup {
    Today,
    Yesterday,
    ThisWeek,
    ThisMonth,
    ThisYear,
    Earlier,
}

impl SidebarDateGroup {
    fn label(self) -> String {
        match self {
            Self::Today => tr!("sidebar.today"),
            Self::Yesterday => tr!("sidebar.yesterday"),
            Self::ThisWeek => tr!("sidebar.this_week"),
            Self::ThisMonth => tr!("sidebar.this_month"),
            Self::ThisYear => tr!("sidebar.this_year"),
            Self::Earlier => tr!("sidebar.more"),
        }
    }
}

/// Resolve calendar boundaries off-thread: local timezone lookup can read the
/// filesystem. Frames and row builders only compare these cached Unix seconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SidebarDateBoundaries {
    starts: [i64; 5],
    next_day: i64,
}

impl SidebarDateBoundaries {
    fn at<Tz: TimeZone>(now: DateTime<Tz>) -> Self {
        let today = now.date_naive();
        let timezone = now.timezone();
        let start = |date: NaiveDate| {
            let midnight = date.and_hms_opt(0, 0, 0).unwrap();
            // Some timezones skip or repeat midnight. Use the first valid
            // instant of the date, including a skipped day at a dateline move.
            (0..=24 * 60)
                .find_map(|minute| {
                    timezone
                        .from_local_datetime(&(midnight + chrono::Duration::minutes(minute)))
                        .earliest()
                })
                .expect("local date has a valid boundary")
                .timestamp()
        };
        Self {
            starts: [
                start(today),
                start(today.pred_opt().unwrap()),
                start(today - chrono::Days::new(today.weekday().num_days_from_monday().into())),
                start(today.with_day(1).unwrap()),
                start(today.with_ordinal(1).unwrap()),
            ],
            next_day: start(today.succ_opt().unwrap()),
        }
    }

    fn group(self, timestamp: u64) -> SidebarDateGroup {
        let timestamp = i64::try_from(timestamp).unwrap_or(i64::MAX);
        match self.starts.iter().position(|start| timestamp >= *start) {
            Some(0) => SidebarDateGroup::Today,
            Some(1) => SidebarDateGroup::Yesterday,
            Some(2) => SidebarDateGroup::ThisWeek,
            Some(3) => SidebarDateGroup::ThisMonth,
            Some(4) => SidebarDateGroup::ThisYear,
            _ => SidebarDateGroup::Earlier,
        }
    }
}

/// Recency for sidebar ordering. Creation promotes a new fork even when its
/// copied turns are older. Submitting a turn promotes the task immediately;
/// metadata edits such as a rename do not.
fn sidebar_session_timestamp(session: &AgentSession) -> u64 {
    session.last_reply_at.unwrap_or(0).max(session.created_at)
}

fn sort_sidebar_sessions(sessions: &mut Vec<&AgentSession>, ordering: SidebarOrdering) {
    sessions.sort_by_key(|session| (sidebar_session_timestamp(session), session.created_at));
    if ordering == SidebarOrdering::Newest {
        // Sessions are appended to the store, so reverse timestamp ties too.
        sessions.reverse();
    }
}

/// Resolve the selected project's session set once per snapshot. Projectless
/// tasks have separate workspaces but share one "No project" history.
fn selected_sidebar_sessions<'a>(
    sessions: &'a [AgentSession],
    projects: &[Project],
    selected_project: Option<Uuid>,
    projectless_root: Option<&Path>,
    ordering: SidebarOrdering,
) -> Vec<&'a AgentSession> {
    let Some(selected) = projects
        .iter()
        .find(|project| Some(project.id) == selected_project)
    else {
        return Vec::new();
    };
    let project_ids = if sidebar_project_is_projectless(selected, projectless_root) {
        projects
            .iter()
            .filter(|project| sidebar_project_is_projectless(project, projectless_root))
            .map(|project| project.id)
            .collect::<HashSet<_>>()
    } else {
        HashSet::from([selected.id])
    };
    let mut sessions = sessions
        .iter()
        .filter(|session| session.is_listed_task() && project_ids.contains(&session.project_id))
        .collect::<Vec<_>>();
    sort_sidebar_sessions(&mut sessions, ordering);
    sessions
}

fn sidebar_project_is_projectless(project: &Project, projectless_root: Option<&Path>) -> bool {
    projectless_root.is_some_and(|root| project.path.starts_with(root))
}

/// Compact "how long ago" for the sidebar: "just now", then one coarse unit —
/// "5m", "3h", "420d". Days are the largest unit so a glance still reads as a
/// count rather than a date.
pub(super) fn format_time_ago(seconds: u64) -> String {
    match seconds {
        0..=59 => tr!("sidebar.just_now"),
        60..=3_599 => tr!("sidebar.minutes_ago", count = seconds / 60),
        3_600..=86_399 => tr!("sidebar.hours_ago", count = seconds / 3_600),
        _ => tr!("sidebar.days_ago", count = seconds / 86_400),
    }
}

/// One row of the virtualized sidebar session history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SidebarRow {
    /// Non-interactive heading for sessions kept above the date groups.
    PinnedHeader,
    /// Non-interactive calendar heading for the following sessions.
    DateHeader(SidebarDateGroup),
    /// A started session.
    Session(Uuid),
}

/// Headers do not consume shortcuts; pinned sessions keep their displayed
/// positions. Resolve this bounded table once for all visible row builders.
fn sidebar_session_shortcuts(rows: &[SidebarRow]) -> [Option<Uuid>; SESSION_SHORTCUT_COUNT] {
    let mut sessions = rows.iter().filter_map(|row| match row {
        SidebarRow::Session(id) => Some(*id),
        _ => None,
    });
    std::array::from_fn(|_| sessions.next())
}

fn grouped_sidebar_rows(
    sessions: &[&AgentSession],
    dates: Option<SidebarDateBoundaries>,
    pinned_session_ids: &HashSet<Uuid>,
) -> Vec<SidebarRow> {
    let mut rows = Vec::with_capacity(sessions.len() + 7);
    for session in sessions {
        if pinned_session_ids.contains(&session.id) {
            if rows.is_empty() {
                rows.push(SidebarRow::PinnedHeader);
            }
            rows.push(SidebarRow::Session(session.id));
        }
    }
    let mut previous_group = None;
    for session in sessions {
        if pinned_session_ids.contains(&session.id) {
            continue;
        }
        if let Some(dates) = dates {
            let group = dates.group(sidebar_session_timestamp(session));
            if previous_group != Some(group) {
                rows.push(SidebarRow::DateHeader(group));
                previous_group = Some(group);
            }
        }
        rows.push(SidebarRow::Session(session.id));
    }
    rows
}

fn sidebar_session_row_index(rows: &[SidebarRow], session_id: Uuid) -> Option<usize> {
    rows.iter()
        .position(|row| *row == SidebarRow::Session(session_id))
}

fn sidebar_remapped_scroll_offset(
    previous_rows: &[SidebarRow],
    rows: &[SidebarRow],
    previous: ListOffset,
) -> Option<ListOffset> {
    // Staying at the top must also reveal any newly inserted first heading.
    if previous.item_ix == 0 && previous.offset_in_item <= Pixels::ZERO {
        return None;
    }
    // Date headings can appear after startup or disappear at midnight. Keep
    // the same row at the viewport edge, or its next session if a heading went.
    [previous.item_ix, previous.item_ix + 1]
        .into_iter()
        .find_map(|previous_index| {
            let anchor = previous_rows.get(previous_index)?;
            let item_ix = rows.iter().position(|row| row == anchor)?;
            Some(ListOffset {
                item_ix,
                offset_in_item: if previous_index == previous.item_ix {
                    previous.offset_in_item
                } else {
                    Pixels::ZERO
                },
            })
        })
}

fn sidebar_row_height(row: SidebarRow) -> Pixels {
    px(match row {
        // Keep headings the same height as session rows so off-screen height
        // hints and nearest-edge selection reveal remain exact.
        SidebarRow::PinnedHeader | SidebarRow::DateHeader(_) | SidebarRow::Session(_) => {
            SIDEBAR_SESSION_ROW_HEIGHT
        }
    })
}

fn sidebar_bottom_aligned_offset(
    rows: &[SidebarRow],
    target: usize,
    viewport_height: Pixels,
) -> ListOffset {
    let mut item_ix = target;
    let mut height = sidebar_row_height(rows[target]);
    while item_ix > 0 && height < viewport_height {
        item_ix -= 1;
        height += sidebar_row_height(rows[item_ix]);
    }
    ListOffset {
        item_ix,
        offset_in_item: (height - viewport_height).max(Pixels::ZERO),
    }
}

fn reveal_sidebar_list_row(list: &ListState, rows: &[SidebarRow], index: usize) {
    let viewport = list.viewport_bounds();
    if viewport.size.height <= Pixels::ZERO {
        return;
    }
    if let Some(item) = list.bounds_for_item(index) {
        if item.top() >= viewport.top() && item.bottom() <= viewport.bottom() {
            return;
        }
        list.scroll_to_reveal_item(index);
    } else if index <= list.logical_scroll_top().item_ix {
        list.scroll_to(ListOffset {
            item_ix: index,
            offset_in_item: Pixels::ZERO,
        });
    } else {
        // Off-screen rows have not necessarily been measured yet. Their
        // sidebar heights are fixed, so align a lower target to the viewport
        // bottom just like scrollIntoView({ block: "nearest" }).
        list.scroll_to(sidebar_bottom_aligned_offset(
            rows,
            index,
            viewport.size.height,
        ));
    }
}

impl Waku {
    pub(super) fn start_sidebar_date_clock(&mut self, cx: &mut Context<Self>) {
        // One task owns all refreshes, so a superseded pass cannot land late.
        self.sidebar_date_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let dates = cx
                    .background_executor()
                    .spawn(async { SidebarDateBoundaries::at(Local::now()) })
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.sidebar_date_boundaries != Some(dates) {
                            this.sidebar_date_boundaries = Some(dates);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
                // Wake at local midnight; check at least once a minute for
                // timezone changes, wall-clock adjustments, and system wake.
                // An unchanged calendar never requests a frame.
                let now = i64::try_from(unix_time()).unwrap_or(i64::MAX);
                let delay = dates.next_day.saturating_sub(now).clamp(1, 60) as u64;
                cx.background_executor()
                    .timer(Duration::from_secs(delay))
                    .await;
            }
        }));
    }

    pub(super) fn window_drag_region(
        &self,
        region: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        // Windows drags from the hit test, not from a mouse-move handler:
        // `DefWindowProc` moves the window once the region reports itself as
        // caption, and performs the user's configured double-click action.
        #[cfg(target_os = "windows")]
        let region = region.window_control_area(gpui::WindowControlArea::Drag);

        region
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    crate::platform::titlebar_double_click(window);
                }
            })
            .on_mouse_down_out(cx.listener(|this, _, _, _| {
                this.header_drag_armed = false;
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.header_drag_armed = true;
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.header_drag_armed = false;
                }),
            )
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.header_drag_armed {
                    this.header_drag_armed = false;
                    crate::platform::start_window_move(window);
                }
            }))
    }
    // ── Sidebar ────────────────────────────────────────────────────────────

    fn render_fps_counter(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let fps = self.fps_value;
        let dot = if fps == 0 {
            theme.text_ghost
        } else if fps >= 55 {
            theme.success
        } else if fps >= 30 {
            theme.warning
        } else {
            theme.danger
        };
        div()
            .flex_none()
            .h(px(26.0))
            .px(px(6.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .text_size(sp(12.5))
            .line_height(sp(0.0))
            .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(dot))
            .child(
                div()
                    .text_color(theme.text_tertiary)
                    .font_family(crate::md::render::MONO_FAMILY)
                    .child(SharedString::from(format!("{fps} FPS"))),
            )
    }

    fn render_sidebar_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let focus = self.transcript_control_focus("toggle-sidebar", cx);
        div()
            .id("toggle-sidebar")
            .track_focus(&focus)
            .tab_index(0)
            .size(px(TITLEBAR_BUTTON_SIZE))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|element| element.bg(theme.overlay))
            .active(|element| element.bg(theme.overlay_strong))
            .tooltip(Tooltip::text(tr!(
                if self.sidebar_visible {
                    "sidebar.hide"
                } else {
                    "sidebar.open"
                },
                shortcut = crate::platform::primary_shortcut("⌘B", "Ctrl+B")
            )))
            .child(icon("icons/panel-left.svg", 20.0, theme.text))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.set_sidebar_visible(!this.sidebar_visible, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    this.set_sidebar_visible(!this.sidebar_visible, cx);
                }
            }))
    }

    fn render_titlebar_search(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let open = self.command_palette.is_open();
        div()
            .id("titlebar-search")
            .tab_index(0)
            .size(px(TITLEBAR_BUTTON_SIZE))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .when(open, |element| element.bg(theme.overlay))
            .hover(|element| element.bg(theme.overlay))
            .active(|element| element.bg(theme.overlay_strong))
            .tooltip(Tooltip::text(tr!(
                "sidebar.search_tasks",
                shortcut = crate::platform::primary_shortcut("⌘K", "Ctrl+K")
            )))
            .child(icon("icons/search.svg", 20.0, theme.text))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.toggle_command_palette_action(&ToggleCommandPalette, window, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.toggle_command_palette_action(&ToggleCommandPalette, window, cx);
                    cx.stop_propagation();
                }
            }))
    }

    fn window_navigation_width(&self, window: &Window, cx: &App) -> f32 {
        self.client_window_controls_width(super::window_chrome::WindowControlSide::Left, window, cx)
            + TRAFFIC_LIGHT_CLEARANCE
            + TITLEBAR_BUTTON_SIZE * 2.0
            + TITLEBAR_NAVIGATION_GAP
    }

    /// Keep navigation outside both panel trees so the sidebar's clip and
    /// the main header's moving origin never move or hide these controls.
    pub(super) fn render_window_navigation(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id("window-navigation")
            .absolute()
            .top_0()
            .left_0()
            .w(px(self.window_navigation_width(window, cx)))
            .h(px(48.0))
            .flex()
            .items_center()
            .children(self.render_client_window_controls(
                super::window_chrome::WindowControlSide::Left,
                window,
                cx,
            ))
            .child(
                self.window_drag_region(
                    div()
                        .id("navigation-traffic-light-drag-region")
                        .w(px(TRAFFIC_LIGHT_CLEARANCE))
                        .h_full()
                        .flex_none(),
                    cx,
                ),
            )
            .child(self.render_sidebar_toggle(cx))
            .child(
                self.render_titlebar_search(cx)
                    .ml(px(TITLEBAR_NAVIGATION_GAP)),
            )
    }

    fn install_sidebar_update(&mut self, cx: &mut Context<Self>) {
        if self.update_status != crate::updater::UpdateStatus::Available {
            return;
        }
        let installed = cx
            .try_global::<crate::updater::UpdaterState>()
            .and_then(|state| state.0.as_ref())
            .is_some_and(|updater| updater.install_available_update());
        if installed {
            self.update_status = crate::updater::UpdateStatus::Updating;
            cx.notify();
        }
    }

    fn render_sidebar_update(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        use crate::updater::UpdateStatus;

        if self.update_status == UpdateStatus::Idle {
            return None;
        }
        let theme = Theme::current(cx);
        let updating = self.update_status == UpdateStatus::Updating;
        let label = if updating {
            tr!("updater.updating")
        } else {
            tr!("updater.update")
        };
        Some(
            div()
                .id("sidebar-install-update")
                .role(gpui::Role::Button)
                .aria_label(label.clone())
                .track_focus(&self.sidebar_update_focus)
                .tab_index(0)
                .size(px(TITLEBAR_BUTTON_SIZE))
                .flex_none()
                .rounded(px(6.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor_default()
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .when(!updating, |button| {
                    button
                        .hover(|style| style.bg(theme.overlay))
                        .active(|style| style.bg(theme.overlay_strong))
                })
                .tooltip(Tooltip::text(label))
                .child(if updating {
                    motion::spin_slow(icon("icons/loader-circle.svg", 20.0, theme.text))
                } else {
                    icon("icons/download.svg", 20.0, theme.accent).into_any_element()
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.install_sidebar_update(cx);
                }))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                        this.install_sidebar_update(cx);
                    }
                })),
        )
    }

    fn render_sidebar_project_selector(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let projectless = self.selected_project().is_some_and(Project::is_projectless);
        let handle = self.project_picker_handle(ProjectPickerSite::Sidebar, cx);
        let trigger = div()
            .id("sidebar-project")
            .flex_1()
            .min_w_0()
            .h(px(SIDEBAR_PROJECT_SELECTOR_HEIGHT))
            // The border takes 1px of the inset.
            .px(px(8.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.overlay)
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_default()
            .focus_visible(|style| style.border_color(theme.accent))
            .when(handle.is_open(), |element| element.bg(theme.overlay_strong))
            .when(!handle.is_open(), |element| {
                element.tooltip(Tooltip::text(tr!(
                    "sidebar.switch_project",
                    shortcut = crate::platform::primary_shortcut("⌘P", "Ctrl+P")
                )))
            })
            .hover(|element| element.bg(theme.overlay_strong))
            .active(|element| element.bg(theme.overlay_strong))
            .child(
                div()
                    .size(px(20.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon(
                        if projectless {
                            "icons/folder-dashed.svg"
                        } else {
                            "icons/folder.svg"
                        },
                        16.0,
                        theme.text,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(sp(13.0))
                    .text_color(theme.text)
                    .child(self.project_picker_label()),
            )
            .child(
                div()
                    .size(px(20.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon("icons/chevrons-up-down.svg", 12.0, theme.text_ghost)),
            );
        self.render_project_picker(trigger, &handle, ProjectPickerSite::Sidebar, cx)
    }

    fn render_sidebar_new_task(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        div()
            .id("sidebar-new-task")
            .tab_index(0)
            .size(px(TITLEBAR_BUTTON_SIZE))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|element| element.bg(theme.overlay))
            .active(|element| element.bg(theme.overlay_strong))
            .tooltip(Tooltip::text(tr!(
                "sidebar.new_task",
                shortcut = crate::platform::primary_shortcut("⌘N", "Ctrl+N")
            )))
            .child(icon("icons/compose.svg", 20.0, theme.text))
            // The titlebar drags the window from a mouse-down; a press here
            // must stay a click.
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.new_session_action(&NewSession, window, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.new_session_action(&NewSession, window, cx);
                    cx.stop_propagation();
                }
            }))
    }

    pub(super) fn render_sidebar(
        &self,
        width: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let is_resizing = self
            .panel_resize_drag
            .is_some_and(|drag| drag.target == PanelResizeTarget::Sidebar);
        // The New task button always fits beside the fixed window navigation
        // at the minimum width. A narrow sidebar leaves no room for the update
        // button too, so it moves beside the project selector instead.
        let update_in_titlebar = width
            >= self.window_navigation_width(window, cx)
                + TITLEBAR_BUTTON_SIZE * 2.0
                + TITLEBAR_NAVIGATION_GAP
                + 20.0;

        let rows = self.sidebar_rows_cached();
        let shortcuts = sidebar_session_shortcuts(&rows);
        self.sync_sidebar_rows(&rows);
        // Restored selection exists before ListState knows the viewport size.
        // Retry after the first layout so nearest-edge alignment has a height.
        if self.sidebar_list_state.viewport_bounds().size.height <= Pixels::ZERO
            && let Some(session_id) = self
                .pending_session_activation
                .map(|pending| pending.session_id)
                .or(self.state.selected_session)
        {
            let entity = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = entity.update(cx, |this, cx| {
                    let selected_session = this
                        .pending_session_activation
                        .map(|pending| pending.session_id)
                        .or(this.state.selected_session);
                    if selected_session == Some(session_id) {
                        this.reveal_sidebar_session(session_id);
                        cx.notify();
                    }
                });
            });
        }
        let history_scrolled =
            self.sidebar_list_state.scroll_px_offset_for_scrollbar().y < px(-0.5);
        let entity = cx.entity().downgrade();

        div()
            .w(px(width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(if is_resizing {
                theme.sidebar_drag_background
            } else {
                theme.sidebar
            })
            .child(
                self.window_drag_region(
                    div()
                        .id("sidebar-titlebar-drag-region")
                        .h(px(48.0))
                        .flex_none()
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .justify_end()
                        .gap(px(TITLEBAR_NAVIGATION_GAP))
                        .tab_index(0)
                        .tab_group()
                        .tab_stop(false)
                        .when(update_in_titlebar, |titlebar| {
                            titlebar.children(self.render_sidebar_update(cx))
                        })
                        .child(self.render_sidebar_new_task(cx)),
                    cx,
                ),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(10.0))
                    .pb(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .tab_index(0)
                    .tab_group()
                    .tab_stop(false)
                    .child(self.render_sidebar_project_selector(cx))
                    .when(!update_in_titlebar, |row| {
                        row.children(self.render_sidebar_update(cx))
                    }),
            )
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        div().px(px(10.0)).size_full().child(
                            list(
                                self.sidebar_list_state.clone(),
                                move |index, _window, cx| {
                                    entity
                                        .upgrade()
                                        .map(|entity| {
                                            entity.update(cx, |this, cx| {
                                                this.sidebar_row(index, &rows, &shortcuts, cx)
                                            })
                                        })
                                        .unwrap_or_else(|| div().into_any_element())
                                },
                            )
                            .size_full(),
                        ),
                    )
                    .child(scrollbar::vertical(
                        &self.sidebar_list_state,
                        &self.sidebar_scrollbar,
                    ))
                    .when(history_scrolled, |scroll| {
                        scroll.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .w_full()
                                .h(px(1.0))
                                .bg(theme.border),
                        )
                    }),
            )
    }

    /// Keep a newly selected task visible without disturbing the sidebar when
    /// its row is already fully inside the viewport.
    pub(super) fn reveal_sidebar_session(&self, session_id: Uuid) {
        let rows = self.sidebar_rows_cached();
        self.sync_sidebar_rows(&rows);
        if let Some(index) = sidebar_session_row_index(&rows, session_id) {
            reveal_sidebar_list_row(&self.sidebar_list_state, &rows, index);
        }
    }

    /// Cache the selected project's grouped history between session, selection,
    /// or local calendar changes. A streaming frame only scans the
    /// allocation-free fingerprint.
    fn sidebar_rows_cached(&self) -> Rc<Vec<SidebarRow>> {
        let mut fingerprint = mix(
            0x51de_ba5e_5eed_c0de,
            u64::from(self.state.selected_project.is_some()),
        );
        if let Some(project_id) = self.state.selected_project {
            fingerprint = mix_uuid(fingerprint, project_id);
        }
        fingerprint = mix(
            fingerprint,
            match self.state.sidebar_ordering {
                SidebarOrdering::Newest => 1,
                SidebarOrdering::Oldest => 2,
            },
        );
        let dates = self.sidebar_date_boundaries;
        fingerprint = mix(fingerprint, u64::from(dates.is_some()));
        if let Some(dates) = dates {
            for start in dates.starts {
                fingerprint = mix(fingerprint, start as u64);
            }
        }
        for session in &self.state.sessions {
            if !session.is_listed_task() {
                continue;
            }
            fingerprint = mix_uuid(fingerprint, session.id);
            fingerprint = mix_uuid(fingerprint, session.project_id);
            fingerprint = mix(fingerprint, sidebar_session_timestamp(session));
        }
        for session_id in &self.state.pinned_session_ids {
            fingerprint = mix_uuid(fingerprint, *session_id);
        }
        for project in &self.state.projects {
            fingerprint = mix_uuid(fingerprint, project.id);
        }
        if self.sidebar_rows_fingerprint.get() != Some(fingerprint) {
            *self.sidebar_rows_snapshot.borrow_mut() = Rc::new(self.sidebar_rows());
            self.sidebar_rows_fingerprint.set(Some(fingerprint));
        }
        self.sidebar_rows_snapshot.borrow().clone()
    }

    fn sidebar_rows(&self) -> Vec<SidebarRow> {
        let projectless_root = crate::projectless::workspace_root();
        let sessions = selected_sidebar_sessions(
            &self.state.sessions,
            &self.state.projects,
            self.state.selected_project,
            projectless_root.as_deref(),
            self.state.sidebar_ordering,
        );
        grouped_sidebar_rows(
            &sessions,
            self.sidebar_date_boundaries,
            &self.state.pinned_session_ids,
        )
    }

    /// Keep the virtualized list in sync with the current row snapshot.
    /// Rows are cheap values, so only the minimal changed suffix is spliced,
    /// preserving scroll position and measured heights across unrelated churn
    /// (e.g. the active session's `updated_at` bumping on every stream tick).
    fn sync_sidebar_rows(&self, rows: &[SidebarRow]) {
        let project_changed = self
            .sidebar_row_project
            .replace(self.state.selected_project)
            != self.state.selected_project;
        let mut cached = self.sidebar_row_cache.borrow_mut();
        // Projectless tasks have distinct workspace project IDs but share
        // one history. Switching between them must not reset its scroll.
        if cached.as_slice() == rows {
            return;
        }
        if project_changed {
            self.sidebar_list_state.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: Pixels::ZERO,
            });
        }
        let anchored_offset = if project_changed {
            None
        } else {
            sidebar_remapped_scroll_offset(
                &cached,
                rows,
                self.sidebar_list_state.logical_scroll_top(),
            )
        };
        let prefix = cached
            .iter()
            .zip(rows.iter())
            .take_while(|(a, b)| a == b)
            .count();
        let old_count = cached.len();
        *cached = rows.to_vec();
        if old_count == 0 {
            self.sidebar_list_state
                .reset_with_uniform_height(rows.len(), px(SIDEBAR_SESSION_ROW_HEIGHT));
        } else {
            self.sidebar_list_state
                .splice(prefix..old_count, rows.len() - prefix);
            // Newly inserted rows have no measured height yet; give them the
            // uniform hint so the scrollbar keeps a correct total height.
            self.sidebar_list_state
                .clone()
                .with_uniform_item_height(px(SIDEBAR_SESSION_ROW_HEIGHT));
        }
        if let Some(offset) = anchored_offset {
            self.sidebar_list_state.scroll_to(offset);
        }
    }

    fn sidebar_row(
        &self,
        index: usize,
        rows: &[SidebarRow],
        shortcuts: &[Option<Uuid>; SESSION_SHORTCUT_COUNT],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match rows.get(index) {
            Some(row @ (SidebarRow::PinnedHeader | SidebarRow::DateHeader(_))) => {
                let label = match row {
                    SidebarRow::PinnedHeader => tr!("sidebar.pinned"),
                    SidebarRow::DateHeader(group) => group.label(),
                    SidebarRow::Session(_) => unreachable!(),
                };
                let theme = Theme::current(cx);
                div()
                    .w_full()
                    .h(sidebar_row_height(*row))
                    .px(px(8.0))
                    .pt(px(6.0))
                    .flex()
                    .items_center()
                    .text_size(sp(11.5))
                    .line_height(sp(16.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text_tertiary)
                    .child(label)
                    .into_any_element()
            }
            Some(SidebarRow::Session(session_id)) => {
                let shortcut = if self.sidebar_session_shortcuts_visible {
                    shortcuts
                        .iter()
                        .position(|id| *id == Some(*session_id))
                        .map(|index| index + 1)
                } else {
                    None
                };
                self.render_sidebar_session_item(*session_id, shortcut, cx)
            }
            None => div().into_any_element(),
        }
    }

    fn toggle_session_pin(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        if !self
            .state
            .sessions
            .iter()
            .any(|session| session.id == session_id && session.is_listed_task())
        {
            return;
        }
        if !self.state.pinned_session_ids.remove(&session_id) {
            self.state.pinned_session_ids.insert(session_id);
        }
        self.save();
        cx.notify();
    }

    fn begin_session_rename(
        &mut self,
        session_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(title) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(localized_session_title)
        else {
            return;
        };

        self.session_rename = Some(session_id);
        self.session_rename_input.update(cx, |input, cx| {
            input.set_content(title, cx);
            input.select_all_text(cx);
        });
        let focus = self.session_rename_input.read(cx).focus();
        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    pub(super) fn commit_session_rename(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_rename.take() else {
            return;
        };
        let title = self
            .session_rename_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let should_update = !title.is_empty()
            && self
                .state
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .is_some_and(|session| session.title != title);
        if should_update
            && self
                .state
                .session_mut(session_id)
                .is_some_and(|session| session.set_title(&title))
        {
            self.save();
        }
        cx.notify();
    }

    fn cancel_session_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_rename.take().is_none() {
            return;
        }
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    pub(super) fn select_sidebar_session_action(
        &mut self,
        action: &SelectSidebarSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shortcuts = sidebar_session_shortcuts(&self.sidebar_rows_cached());
        let Some(session_id) = shortcuts.get(action.0).copied().flatten() else {
            return;
        };
        if self.settings_page.is_some() {
            self.close_settings_popup(window, cx);
        }
        self.select_session(session_id, cx);
    }

    pub(super) fn sidebar_session_shortcuts_modifiers_changed(
        &mut self,
        event: &gpui::ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_sidebar_session_shortcuts_visible(
            window.is_window_active() && event.secondary(),
            cx,
        );
    }

    pub(super) fn set_sidebar_session_shortcuts_visible(
        &mut self,
        visible: bool,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar_session_shortcuts_visible != visible {
            self.sidebar_session_shortcuts_visible = visible;
            cx.notify();
        }
    }

    fn render_sidebar_session_item(
        &self,
        session_id: Uuid,
        shortcut: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return div().into_any_element();
        };
        let selected = sidebar_session_selected(
            self.state.selected_session,
            self.pending_session_activation
                .map(|pending| pending.session_id),
            session_id,
        );
        let working = matches!(
            session.status,
            SessionStatus::Connecting | SessionStatus::Working
        );
        let pinned = self.state.pinned_session_ids.contains(&session_id);
        let rename_input =
            (self.session_rename == Some(session_id)).then(|| self.session_rename_input.clone());
        let renaming = rename_input.is_some();
        let title = if let Some(rename_input) = rename_input {
            div()
                .id(SharedString::from(format!(
                    "session-rename-field-{session_id}"
                )))
                .key_context(SESSION_RENAME_PARENT_CONTEXT)
                .on_action(cx.listener(|this, _: &CancelSessionRename, window, cx| {
                    this.cancel_session_rename(window, cx);
                }))
                .h(px(18.0))
                .flex_1()
                .min_w_0()
                .px(px(4.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(theme.accent)
                .bg(theme.inset)
                .flex()
                .items_center()
                .text_size(sp(13.5))
                .text_color(theme.text)
                .child(rename_input)
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(sp(13.5))
                .text_color(theme.text)
                .child(SharedString::from(single_line_label(
                    &localized_session_title(session),
                )))
                .into_any_element()
        };
        let waku = cx.entity().downgrade();
        let menu = self.menu_handle(format!("session-{session_id}"), cx);
        let row_focus = menu.trigger_focus_handle().clone();
        let keyboard_menu = menu.clone();
        let row = div()
            .id(SharedString::from(format!("session-{}", session.id)))
            .w_full()
            .min_w_0()
            .h(px(SIDEBAR_SESSION_CARD_HEIGHT))
            .flex()
            .items_center()
            .px(px(8.0))
            .rounded(px(7.0))
            .cursor_default()
            .when(selected, |element| {
                element.bg(theme.sidebar_item_background)
            })
            .hover(|element| element.bg(theme.sidebar_item_background))
            .active(|element| element.bg(theme.sidebar_item_background))
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .overflow_hidden()
                    .line_height(sp(18.0))
                    .child(title)
                    .when(pinned, |element| {
                        element.child(icon("icons/pin.svg", 12.0, theme.text_tertiary))
                    })
                    .when(working, |element| {
                        element.child(motion::spin_slow(icon(
                            "icons/loader-circle.svg",
                            12.0,
                            status_color(&theme, session.status),
                        )))
                    })
                    .when(session.status == SessionStatus::Background, |element| {
                        element.child(icon(
                            "icons/hourglass.svg",
                            12.0,
                            status_color(&theme, session.status),
                        ))
                    })
                    .when(session.status == SessionStatus::Waiting, |element| {
                        element.child(icon(
                            "icons/alert.svg",
                            12.0,
                            status_color(&theme, session.status),
                        ))
                    })
                    .when(session.status == SessionStatus::Failed, |element| {
                        element.child(icon(
                            "icons/x.svg",
                            12.0,
                            status_color(&theme, session.status),
                        ))
                    })
                    .when(
                        sidebar_session_indicates_finished(
                            &self.finished_session_ids,
                            selected,
                            session.status,
                            session_id,
                        ),
                        |element| {
                            element.child(
                                div()
                                    .size(px(12.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(div().size(px(6.0)).rounded_full().bg(theme.accent)),
                            )
                        },
                    )
                    .children(shortcut.map(|number| {
                        div()
                            .size(px(18.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(theme.border_strong)
                            .bg(theme.inset)
                            .text_size(sp(11.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text_secondary)
                            .child(number.to_string())
                    })),
            )
            .when(!renaming, |element| {
                element
                    .track_focus(&row_focus)
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        let key = event.keystroke.key.as_str();
                        if matches!(key, "enter" | "space") {
                            this.select_session(session_id, cx);
                            cx.stop_propagation();
                        } else if key == "f10" && event.keystroke.modifiers.shift {
                            keyboard_menu.open_context_menu(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_session(session_id, cx);
                    }))
            });
        let row = if renaming {
            div()
                .w_full()
                .child(row)
                .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                    if this.session_rename == Some(session_id) {
                        this.commit_session_rename(cx);
                    }
                }))
                .into_any_element()
        } else {
            context_menu(
                div().w_full().child(row),
                SharedString::from(format!("session-menu-{session_id}")),
                &menu,
                move |_| sidebar_session_menu_items(session_id, pinned, waku.clone()),
            )
        };

        div()
            .relative()
            .w_full()
            .pb(px(SIDEBAR_SESSION_ROW_GAP))
            .child(row)
            .into_any_element()
    }

    // ── Header ─────────────────────────────────────────────────────────────

    pub(super) fn render_header(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = Theme::current(cx);
        let session = self.selected_session();
        let title = session
            .map(localized_session_title)
            .unwrap_or_else(|| tr!("session.new_task"));
        div()
            .id("window-header")
            .h(px(48.0))
            .flex_none()
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .items_center()
            .gap(px(8.0))
            // Navigation stays pinned to the window while this header follows
            // the sidebar edge. Reserve only the part still over the header.
            .pl(px((self.window_navigation_width(window, cx) + 8.0
                - self.sidebar_rendered_width)
                .max(14.0)))
            .pr(px((self.right_titlebar_controls_width(window, cx) + 8.0
                - self.right_panel_rendered_width)
                .max(14.0)))
            .child(
                self.window_drag_region(
                    div()
                        .id("header-title-drag-region")
                        .h_full()
                        .min_w_0()
                        .flex_shrink(1.0)
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(sp(13.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from(title)),
                        ),
                    cx,
                ),
            )
            .child(
                self.window_drag_region(
                    div().id("header-center-drag-region").h_full().flex_1(),
                    cx,
                ),
            )
            .child(self.render_background_work_summary(cx))
            .when(
                !self.right_panel_visible && self.fps_counter_visible,
                |element| element.child(self.render_fps_counter(cx)),
            )
    }

    // ── Empty states ───────────────────────────────────────────────────────

    pub(super) fn render_empty_state(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        if self.selected_project().is_none() {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .px_8()
                .pb(px(46.0))
                .child(icon("icons/sparkle.svg", 24.0, theme.accent))
                .child(
                    div()
                        .mt(px(16.0))
                        .text_size(sp(20.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(tr_cow!("onboarding.open_project_to_begin")),
                )
                .child(
                    div()
                        .mt(px(8.0))
                        .max_w(px(380.0))
                        .text_center()
                        .text_size(sp(12.5))
                        .line_height(sp(19.0))
                        .text_color(theme.text_tertiary)
                        .child(tr_cow!("onboarding.description")),
                )
                .child(
                    div()
                        .mt(px(20.0))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(8.0))
                        .tab_index(0)
                        .tab_group()
                        .tab_stop(false)
                        .child(
                            div()
                                .id("onboarding-add-project")
                                .track_focus(&self.onboarding_add_project_focus)
                                .tab_index(0)
                                .focus_visible(|style| style.border_1().border_color(theme.accent))
                                .h(px(32.0))
                                .px(px(14.0))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .cursor_default()
                                .bg(theme.inverse)
                                .text_color(theme.on_inverse)
                                .text_size(sp(12.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .hover(|element| element.opacity(0.9))
                                .active(|element| element.opacity(0.8))
                                .child(tr_cow!("onboarding.open_project_folder"))
                                .on_click(cx.listener(|this, _, _, cx| this.add_project(cx)))
                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        this.add_project(cx);
                                        cx.stop_propagation();
                                    }
                                })),
                        )
                        .child(
                            div()
                                .id("onboarding-projectless")
                                .track_focus(&self.onboarding_projectless_focus)
                                .tab_index(1)
                                .focus_visible(|style| style.border_1().border_color(theme.accent))
                                .h(px(30.0))
                                .px(px(12.0))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .cursor_default()
                                .text_color(theme.text_secondary)
                                .text_size(sp(12.5))
                                .hover(|element| element.bg(theme.overlay))
                                .active(|element| element.bg(theme.overlay_strong))
                                .child(icon("icons/folder-dashed.svg", 11.0, theme.text_tertiary))
                                .child(tr_cow!("project.no_project"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.create_projectless_session(cx);
                                }))
                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        this.create_projectless_session(cx);
                                        cx.stop_propagation();
                                    }
                                })),
                        ),
                );
        }
        let projectless_selected = self.selected_project().is_some_and(Project::is_projectless);
        let project_name = self
            .selected_project()
            .map(|project| {
                if project.is_projectless() {
                    tr!("project.without_a_project")
                } else {
                    project.display_name()
                }
            })
            .unwrap_or_else(|| tr!("project.your_project"));
        let handle = self.project_picker_handle(ProjectPickerSite::EmptyState, cx);
        let project_selector = self.render_project_picker(
            ProjectNameSelector::new("empty-state-project", project_name)
                .selected(handle.is_open()),
            &handle,
            ProjectPickerSite::EmptyState,
            cx,
        );
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .px_8()
            .pb(px(52.0))
            .child(icon("icons/sparkle.svg", 20.0, theme.accent))
            .child(
                div()
                    .mt(px(14.0))
                    .flex()
                    .items_baseline()
                    .text_size(sp(20.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .when(projectless_selected, |element| {
                        element.child(tr_cow!("onboarding.what_should_we_build"))
                    })
                    .when(!projectless_selected, |element| {
                        element
                            .child(tr_cow!("onboarding.what_should_we_build_in"))
                            .child(project_selector)
                            .child(tr_cow!("onboarding.question_mark"))
                    }),
            )
    }
}

fn localized_session_title(session: &AgentSession) -> String {
    let title = session.display_title();
    if title == AgentSession::DEFAULT_TITLE {
        tr!("session.new_task")
    } else {
        title.to_owned()
    }
}

fn sidebar_session_selected(
    selected_session: Option<Uuid>,
    pending_session: Option<Uuid>,
    session_id: Uuid,
) -> bool {
    pending_session.map_or(selected_session == Some(session_id), |pending| {
        pending == session_id
    })
}

pub(super) fn sidebar_session_indicates_finished(
    finished_session_ids: &HashSet<Uuid>,
    selected: bool,
    status: SessionStatus,
    session_id: Uuid,
) -> bool {
    !selected && status == SessionStatus::Idle && finished_session_ids.contains(&session_id)
}

fn sidebar_session_menu_items(
    session_id: Uuid,
    pinned: bool,
    waku: WeakEntity<Waku>,
) -> Vec<MenuItem> {
    let pin_waku = waku.clone();
    let rename_waku = waku.clone();
    let copy_waku = waku.clone();
    let export_waku = waku.clone();
    let remove_waku = waku;
    vec![
        MenuItem::new(tr!("common.rename"), move |window, cx| {
            let _ = rename_waku.update(cx, |waku, cx| {
                waku.begin_session_rename(session_id, window, cx);
            });
        }),
        MenuItem::Separator,
        MenuItem::new(
            if pinned {
                tr!("session.unpin")
            } else {
                tr!("session.pin")
            },
            move |_, cx| {
                let _ = pin_waku.update(cx, |waku, cx| waku.toggle_session_pin(session_id, cx));
            },
        ),
        MenuItem::Separator,
        MenuItem::submenu(tr!("common.copy"), {
            let copy_waku = copy_waku.clone();
            move |_| {
                let copy_conversation_waku = copy_waku.clone();
                let copy_all_waku = copy_waku.clone();
                let copy_deeplink_waku = copy_waku.clone();
                vec![
                    MenuItem::new(tr!("session.copy_conversation"), move |_, cx| {
                        let _ = copy_conversation_waku.update(cx, |waku, cx| {
                            waku.copy_session(
                                session_id,
                                crate::session_export::ExportScope::Conversation,
                                cx,
                            );
                        });
                    }),
                    MenuItem::new(tr!("session.copy_all"), move |_, cx| {
                        let _ = copy_all_waku.update(cx, |waku, cx| {
                            waku.copy_session(
                                session_id,
                                crate::session_export::ExportScope::All,
                                cx,
                            );
                        });
                    }),
                    MenuItem::new(tr!("session.copy_deeplink"), move |_, cx| {
                        let _ = copy_deeplink_waku.update(cx, |waku, cx| {
                            waku.copy_session_deeplink(session_id, cx);
                        });
                    }),
                ]
            }
        }),
        MenuItem::submenu(tr!("common.export"), {
            let export_waku = export_waku.clone();
            move |_| {
                let export_conversation_waku = export_waku.clone();
                let export_all_waku = export_waku.clone();
                vec![
                    MenuItem::new(tr!("session.export_conversation"), move |_, cx| {
                        let _ = export_conversation_waku.update(cx, |waku, cx| {
                            waku.export_session(
                                session_id,
                                crate::session_export::ExportScope::Conversation,
                                cx,
                            );
                        });
                    }),
                    MenuItem::new(tr!("session.export_all"), move |_, cx| {
                        let _ = export_all_waku.update(cx, |waku, cx| {
                            waku.export_session(
                                session_id,
                                crate::session_export::ExportScope::All,
                                cx,
                            );
                        });
                    }),
                ]
            }
        }),
        MenuItem::Separator,
        MenuItem::new(tr!("common.remove"), move |_, cx| {
            let _ = remove_waku.update(cx, |waku, cx| waku.remove_session(session_id, cx));
        })
        .destructive(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calendar_time(year: i32, month: u32, day: u32, hour: u32) -> DateTime<chrono::FixedOffset> {
        chrono::FixedOffset::east_opt(8 * 3_600)
            .unwrap()
            .with_ymd_and_hms(year, month, day, hour, 0, 0)
            .unwrap()
    }

    #[test]
    fn sidebar_date_groups_use_local_calendar_boundaries() {
        use SidebarDateGroup::*;

        let dates = SidebarDateBoundaries::at(calendar_time(2026, 10, 30, 12));
        for (timestamp, expected) in [
            (calendar_time(2026, 10, 31, 0), Today),
            (calendar_time(2026, 10, 30, 0), Today),
            (calendar_time(2026, 10, 29, 23), Yesterday),
            (calendar_time(2026, 10, 29, 0), Yesterday),
            (calendar_time(2026, 10, 28, 12), ThisWeek),
            (calendar_time(2026, 10, 26, 0), ThisWeek),
            (calendar_time(2026, 10, 25, 23), ThisMonth),
            (calendar_time(2026, 10, 1, 0), ThisMonth),
            (calendar_time(2026, 9, 30, 23), ThisYear),
            (calendar_time(2026, 1, 1, 0), ThisYear),
            (calendar_time(2025, 12, 31, 23), Earlier),
        ] {
            assert_eq!(dates.group(timestamp.timestamp() as u64), expected);
        }

        // Local midnight is still the preceding day in UTC.
        let midnight = Utc.with_ymd_and_hms(2026, 10, 29, 16, 0, 0).unwrap();
        assert_eq!(dates.group(midnight.timestamp() as u64), Today);
        assert_eq!(dates.group(midnight.timestamp() as u64 - 1), Yesterday);
    }

    #[test]
    fn sidebar_calendar_prioritizes_yesterday_and_week_across_months_and_years() {
        use SidebarDateGroup::*;

        let new_year = SidebarDateBoundaries::at(calendar_time(2026, 1, 1, 12));
        assert_eq!(
            new_year.group(calendar_time(2025, 12, 31, 12).timestamp() as u64),
            Yesterday
        );
        assert_eq!(
            new_year.group(calendar_time(2025, 12, 29, 0).timestamp() as u64),
            ThisWeek
        );
        assert_eq!(
            new_year.group(calendar_time(2025, 12, 28, 23).timestamp() as u64),
            Earlier
        );

        let new_month = SidebarDateBoundaries::at(calendar_time(2026, 8, 1, 12));
        assert_eq!(
            new_month.group(calendar_time(2026, 7, 31, 12).timestamp() as u64),
            Yesterday
        );
        assert_eq!(
            new_month.group(calendar_time(2026, 7, 27, 0).timestamp() as u64),
            ThisWeek
        );
        assert_eq!(
            new_month.group(calendar_time(2026, 7, 26, 23).timestamp() as u64),
            ThisYear
        );

        let monday = SidebarDateBoundaries::at(calendar_time(2026, 3, 2, 12));
        assert_eq!(
            monday.group(calendar_time(2026, 3, 1, 12).timestamp() as u64),
            Yesterday
        );
        assert_eq!(
            monday.group(calendar_time(2026, 2, 28, 12).timestamp() as u64),
            ThisYear
        );
    }

    #[test]
    fn sidebar_headers_preserve_both_sort_orders_and_omit_empty_groups() {
        use SidebarDateGroup::*;

        let project_id = Uuid::new_v4();
        let dates = SidebarDateBoundaries::at(calendar_time(2026, 10, 30, 12));
        let sessions = [
            (2026, 10, 30, 10),
            (2026, 10, 30, 9),
            (2026, 10, 29, 12),
            (2025, 1, 1, 12),
        ]
        .map(|(year, month, day, hour)| {
            let mut session = AgentSession::new(project_id, ProviderKind::Codex);
            session.created_at = calendar_time(year, month, day, hour).timestamp() as u64;
            session
        });
        let mut sorted = sessions.iter().collect::<Vec<_>>();
        let unpinned = HashSet::new();
        sort_sidebar_sessions(&mut sorted, SidebarOrdering::Newest);
        let newest = vec![
            SidebarRow::DateHeader(Today),
            SidebarRow::Session(sessions[0].id),
            SidebarRow::Session(sessions[1].id),
            SidebarRow::DateHeader(Yesterday),
            SidebarRow::Session(sessions[2].id),
            SidebarRow::DateHeader(Earlier),
            SidebarRow::Session(sessions[3].id),
        ];
        assert_eq!(
            grouped_sidebar_rows(&sorted, Some(dates), &unpinned),
            newest
        );

        sort_sidebar_sessions(&mut sorted, SidebarOrdering::Oldest);
        assert_eq!(
            grouped_sidebar_rows(&sorted, Some(dates), &unpinned),
            vec![
                SidebarRow::DateHeader(Earlier),
                SidebarRow::Session(sessions[3].id),
                SidebarRow::DateHeader(Yesterday),
                SidebarRow::Session(sessions[2].id),
                SidebarRow::DateHeader(Today),
                SidebarRow::Session(sessions[1].id),
                SidebarRow::Session(sessions[0].id),
            ]
        );
        assert!(grouped_sidebar_rows(&[], Some(dates), &unpinned).is_empty());
        assert_eq!(
            grouped_sidebar_rows(&sorted, None, &unpinned),
            sorted
                .iter()
                .map(|session| SidebarRow::Session(session.id))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn sidebar_shortcuts_skip_headers_and_stop_at_nine_sessions() {
        let mut rows = vec![SidebarRow::PinnedHeader];
        rows.extend((1..=2).map(|id| SidebarRow::Session(Uuid::from_u128(id))));
        rows.push(SidebarRow::DateHeader(SidebarDateGroup::Today));
        rows.extend((3..=11).map(|id| SidebarRow::Session(Uuid::from_u128(id))));

        assert_eq!(
            sidebar_session_shortcuts(&rows),
            std::array::from_fn(|index| Some(Uuid::from_u128(index as u128 + 1)))
        );
        assert_eq!(
            sidebar_session_shortcuts(&rows[..3]),
            [
                Some(Uuid::from_u128(1)),
                Some(Uuid::from_u128(2)),
                None,
                None,
                None,
                None,
                None,
                None,
                None
            ]
        );
        assert_eq!(sidebar_session_shortcuts(&[]), [None; 9]);
    }

    #[test]
    fn sidebar_pins_stay_above_dates_within_the_selected_project() {
        let projects =
            ["first", "second"].map(|name| Project::from_path(Path::new("/work").join(name)));
        let dates = SidebarDateBoundaries::at(calendar_time(2026, 10, 30, 12));
        let sessions = [(0, 30), (0, 29), (0, 28), (1, 30)].map(|(project, day)| {
            let mut session = AgentSession::new(projects[project].id, ProviderKind::Codex);
            session.push_message(MessageRole::User, "Started task");
            session.created_at = calendar_time(2026, 10, day, 10).timestamp() as u64;
            session
        });
        let mut pins = HashSet::from([sessions[0].id, sessions[2].id, sessions[3].id]);
        for (ordering, pinned_order) in [
            (SidebarOrdering::Newest, [0, 2]),
            (SidebarOrdering::Oldest, [2, 0]),
        ] {
            let history = selected_sidebar_sessions(
                &sessions,
                &projects,
                Some(projects[0].id),
                None,
                ordering,
            );
            let shortcuts =
                sidebar_session_shortcuts(&grouped_sidebar_rows(&history, Some(dates), &pins));
            assert_eq!(
                &shortcuts[..3],
                &[
                    Some(sessions[pinned_order[0]].id),
                    Some(sessions[pinned_order[1]].id),
                    Some(sessions[1].id)
                ]
            );
            assert!(shortcuts[3..].iter().all(Option::is_none));
            assert_eq!(
                grouped_sidebar_rows(&history, Some(dates), &pins),
                vec![
                    SidebarRow::PinnedHeader,
                    SidebarRow::Session(sessions[pinned_order[0]].id),
                    SidebarRow::Session(sessions[pinned_order[1]].id),
                    SidebarRow::DateHeader(SidebarDateGroup::Yesterday),
                    SidebarRow::Session(sessions[1].id),
                ]
            );
        }

        let history = selected_sidebar_sessions(
            &sessions,
            &projects,
            Some(projects[0].id),
            None,
            SidebarOrdering::Newest,
        );
        // Pins are visible even before the background calendar lookup lands.
        assert_eq!(
            grouped_sidebar_rows(&history, None, &pins),
            vec![
                SidebarRow::PinnedHeader,
                SidebarRow::Session(sessions[0].id),
                SidebarRow::Session(sessions[2].id),
                SidebarRow::Session(sessions[1].id),
            ]
        );
        pins.remove(&sessions[0].id);
        pins.remove(&sessions[2].id);
        // A pin in another project must not leave an empty heading behind.
        assert_eq!(
            grouped_sidebar_rows(&history, Some(dates), &pins),
            vec![
                SidebarRow::DateHeader(SidebarDateGroup::Today),
                SidebarRow::Session(sessions[0].id),
                SidebarRow::DateHeader(SidebarDateGroup::Yesterday),
                SidebarRow::Session(sessions[1].id),
                SidebarRow::DateHeader(SidebarDateGroup::ThisWeek),
                SidebarRow::Session(sessions[2].id),
            ]
        );
    }

    #[test]
    fn sidebar_groups_change_at_midnight_without_session_edits() {
        let before = SidebarDateBoundaries::at(calendar_time(2026, 10, 30, 23));
        let after = SidebarDateBoundaries::at(calendar_time(2026, 10, 31, 0));
        let timestamp = calendar_time(2026, 10, 30, 12).timestamp() as u64;

        assert_ne!(before, after);
        assert_eq!(before.next_day, after.starts[0]);
        assert_eq!(before.group(timestamp), SidebarDateGroup::Today);
        assert_eq!(after.group(timestamp), SidebarDateGroup::Yesterday);
    }

    #[test]
    fn sidebar_date_changes_keep_the_viewport_on_the_same_session() {
        let session = SidebarRow::Session(Uuid::from_u128(1));
        let grouped = [SidebarRow::DateHeader(SidebarDateGroup::Today), session];
        assert!(
            sidebar_remapped_scroll_offset(&[session], &grouped, ListOffset::default()).is_none()
        );
        let offset = sidebar_remapped_scroll_offset(
            &[session],
            &grouped,
            ListOffset {
                item_ix: 0,
                offset_in_item: px(7.0),
            },
        )
        .unwrap();
        assert_eq!(offset.item_ix, 1);
        assert_eq!(offset.offset_in_item, px(7.0));

        let tomorrow = [SidebarRow::DateHeader(SidebarDateGroup::Yesterday), session];
        let offset = sidebar_remapped_scroll_offset(
            &grouped,
            &tomorrow,
            ListOffset {
                item_ix: 0,
                offset_in_item: px(7.0),
            },
        )
        .unwrap();
        assert_eq!(offset.item_ix, 1);
        assert_eq!(offset.offset_in_item, Pixels::ZERO);
    }

    #[test]
    fn sidebar_recency_uses_last_reply_with_creation_fallback() {
        let project_id = Uuid::new_v4();
        let mut renamed_old_session = AgentSession::new(project_id, ProviderKind::Codex);
        renamed_old_session.created_at = 10;
        renamed_old_session.last_reply_at = Some(20);
        renamed_old_session.updated_at = 1_000;

        let mut newer_unanswered_session = AgentSession::new(project_id, ProviderKind::Codex);
        newer_unanswered_session.created_at = 30;
        newer_unanswered_session.last_reply_at = None;
        newer_unanswered_session.updated_at = 30;

        assert_eq!(sidebar_session_timestamp(&renamed_old_session), 20);
        assert_eq!(sidebar_session_timestamp(&newer_unanswered_session), 30);

        let mut sessions = vec![&renamed_old_session, &newer_unanswered_session];
        sort_sidebar_sessions(&mut sessions, SidebarOrdering::Newest);
        assert_eq!(sessions[0].id, newer_unanswered_session.id);

        sort_sidebar_sessions(&mut sessions, SidebarOrdering::Oldest);
        assert_eq!(sessions[0].id, renamed_old_session.id);
    }

    #[test]
    fn sidebar_places_new_response_forks_above_their_source() {
        let project_id = Uuid::new_v4();
        let mut source = AgentSession::new(project_id, ProviderKind::Codex);
        source.begin_turn("Check network connectivity");
        source.mark_active_turn_provider_started();
        source.finish_active_turn(TurnStatus::Completed);
        source.created_at = 10;
        source.last_reply_at = Some(20);

        let fork = |title: &str| {
            source
                .fork_through_turn(
                    1,
                    ProviderResumeCursor::Codex {
                        thread_id: Uuid::new_v4().to_string(),
                    },
                    title,
                )
                .unwrap()
        };
        let first_fork = fork("Network Connectivity Test (2)");
        let mut second_fork = fork("Network Connectivity Test (3)");
        // Rapid forks can share the same second-resolution creation time.
        second_fork.created_at = first_fork.created_at;

        let stored = [&source, &first_fork, &second_fork];
        for (ordering, expected) in [
            (
                SidebarOrdering::Newest,
                [second_fork.id, first_fork.id, source.id],
            ),
            (
                SidebarOrdering::Oldest,
                [source.id, first_fork.id, second_fork.id],
            ),
        ] {
            let mut sorted = stored.to_vec();
            sort_sidebar_sessions(&mut sorted, ordering);
            assert_eq!(
                sorted.iter().map(|session| session.id).collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn sidebar_history_follows_the_selected_project_and_preserves_ordering() {
        let projects = ["first", "second", "empty"]
            .map(|name| Project::from_path(Path::new("/work").join(name)));
        let started = |project_id, timestamp| {
            let mut session = AgentSession::new(project_id, ProviderKind::Codex);
            session.push_message(MessageRole::User, "Started task");
            session.created_at = timestamp;
            session
        };
        let sessions = vec![
            started(projects[0].id, 10),
            started(projects[1].id, 30),
            started(projects[0].id, 20),
            AgentSession::new(projects[0].id, ProviderKind::Codex),
        ];
        let history = |selected, ordering| {
            selected_sidebar_sessions(&sessions, &projects, selected, None, ordering)
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            history(Some(projects[0].id), SidebarOrdering::Newest),
            vec![sessions[2].id, sessions[0].id]
        );
        assert_eq!(
            history(Some(projects[1].id), SidebarOrdering::Newest),
            vec![sessions[1].id]
        );
        assert_eq!(
            history(Some(projects[0].id), SidebarOrdering::Oldest),
            vec![sessions[0].id, sessions[2].id]
        );
        assert!(history(Some(projects[2].id), SidebarOrdering::Newest).is_empty());
        assert!(history(None, SidebarOrdering::Newest).is_empty());
        assert!(history(Some(Uuid::new_v4()), SidebarOrdering::Newest).is_empty());
    }

    #[test]
    fn no_project_history_includes_every_projectless_workspace() {
        let root = Path::new("/tmp/.waku/projects");
        let projects = vec![
            Project::from_path(PathBuf::from("/work/ordinary")),
            Project::from_path(root.join("2026-10-29/first")),
            Project::from_path(root.join("2026-10-30/second")),
        ];
        let sessions = projects
            .iter()
            .enumerate()
            .map(|(index, project)| {
                let mut session = AgentSession::new(project.id, ProviderKind::Codex);
                session.push_message(MessageRole::User, "Started task");
                session.created_at = index as u64;
                session
            })
            .collect::<Vec<_>>();
        for selected in &projects[1..] {
            let history = selected_sidebar_sessions(
                &sessions,
                &projects,
                Some(selected.id),
                Some(root),
                SidebarOrdering::Newest,
            );
            assert_eq!(
                history.iter().map(|session| session.id).collect::<Vec<_>>(),
                vec![sessions[2].id, sessions[1].id]
            );
        }
    }

    #[test]
    fn projectless_sidebar_projects_are_paths_under_the_workspace_root() {
        let root = Path::new("/tmp/.waku/projects");
        let projectless = Project {
            id: Uuid::from_u128(1),
            name: "Task".to_owned(),
            path: root.join("2026-08-23/task"),
            created_at: 0,
        };
        let ordinary = Project {
            id: Uuid::from_u128(2),
            name: "Ordinary".to_owned(),
            path: PathBuf::from("/tmp/dev/ordinary"),
            created_at: 0,
        };

        assert!(sidebar_project_is_projectless(&projectless, Some(root)));
        assert!(!sidebar_project_is_projectless(&ordinary, Some(root)));
        assert!(!sidebar_project_is_projectless(&projectless, None));
    }

    #[test]
    fn pending_session_replaces_sidebar_selection_immediately() {
        let current = Uuid::from_u128(1);
        let pending = Uuid::from_u128(2);

        assert!(!sidebar_session_selected(
            Some(current),
            Some(pending),
            current
        ));
        assert!(sidebar_session_selected(
            Some(current),
            Some(pending),
            pending
        ));
        assert!(sidebar_session_selected(Some(current), None, current));
    }

    #[test]
    fn selected_session_uses_nearest_bottom_edge_for_an_unmeasured_lower_row() {
        let target = Uuid::from_u128(31);
        let rows: Vec<_> = (1..=40)
            .map(|id| SidebarRow::Session(Uuid::from_u128(id)))
            .collect();

        let index = sidebar_session_row_index(&rows, target).unwrap();
        let offset = sidebar_bottom_aligned_offset(&rows, index, px(400.0));

        assert_eq!(index, 30);
        assert_eq!(offset.item_ix, 18);
        assert_eq!(offset.offset_in_item, px(29.0));
        let visible_height = rows[offset.item_ix..=index]
            .iter()
            .copied()
            .map(sidebar_row_height)
            .fold(Pixels::ZERO, |height, row| height + row)
            - offset.offset_in_item;
        assert_eq!(visible_height, px(400.0));
        assert_eq!(sidebar_session_row_index(&rows, Uuid::from_u128(41)), None);
    }

    #[test]
    fn selected_session_reveal_accounts_for_date_headers() {
        let target = Uuid::from_u128(15);
        let mut rows = vec![SidebarRow::DateHeader(SidebarDateGroup::Today)];
        rows.extend((1..=10).map(|id| SidebarRow::Session(Uuid::from_u128(id))));
        rows.push(SidebarRow::DateHeader(SidebarDateGroup::Yesterday));
        rows.extend((11..=20).map(|id| SidebarRow::Session(Uuid::from_u128(id))));

        let index = sidebar_session_row_index(&rows, target).unwrap();
        let offset = sidebar_bottom_aligned_offset(&rows, index, px(200.0));

        assert_eq!(index, 16);
        assert_eq!(offset.item_ix, 10);
        assert_eq!(offset.offset_in_item, px(31.0));
        let visible_height = rows[offset.item_ix..=index]
            .iter()
            .copied()
            .map(sidebar_row_height)
            .fold(Pixels::ZERO, |height, row| height + row)
            - offset.offset_in_item;
        assert_eq!(visible_height, px(200.0));
    }

    #[test]
    fn sidebar_session_indicates_finished_only_when_idle_unselected_and_finished() {
        let session_id = Uuid::from_u128(42);
        let mut finished = HashSet::new();
        finished.insert(session_id);

        // When in finished set, unselected, and idle: returns true
        assert!(sidebar_session_indicates_finished(
            &finished,
            false,
            SessionStatus::Idle,
            session_id
        ));

        // When currently selected: returns false
        assert!(!sidebar_session_indicates_finished(
            &finished,
            true,
            SessionStatus::Idle,
            session_id
        ));

        // When working or connecting: returns false
        assert!(!sidebar_session_indicates_finished(
            &finished,
            false,
            SessionStatus::Working,
            session_id
        ));
        assert!(!sidebar_session_indicates_finished(
            &finished,
            false,
            SessionStatus::Connecting,
            session_id
        ));

        // When waiting or background or failed: returns false
        assert!(!sidebar_session_indicates_finished(
            &finished,
            false,
            SessionStatus::Waiting,
            session_id
        ));
        assert!(!sidebar_session_indicates_finished(
            &finished,
            false,
            SessionStatus::Background,
            session_id
        ));
        assert!(!sidebar_session_indicates_finished(
            &finished,
            false,
            SessionStatus::Failed,
            session_id
        ));

        // When not in finished set: returns false
        let other_session = Uuid::from_u128(99);
        assert!(!sidebar_session_indicates_finished(
            &finished,
            false,
            SessionStatus::Idle,
            other_session
        ));
    }

    #[gpui::test]
    fn sidebar_session_menu_items_has_copy_and_export_submenus(cx: &mut gpui::TestAppContext) {
        let session_id = Uuid::new_v4();
        let items = sidebar_session_menu_items(session_id, false, WeakEntity::new_invalid());
        assert_eq!(items.len(), 8);

        match &items[0] {
            MenuItem::Entry { label, .. } => assert_eq!(label, tr!("common.rename").as_str()),
            _ => panic!("expected rename entry"),
        }
        assert!(matches!(&items[1], MenuItem::Separator));
        match &items[2] {
            MenuItem::Entry { label, .. } => assert_eq!(label, tr!("session.pin").as_str()),
            _ => panic!("expected pin entry"),
        }
        assert!(matches!(&items[3], MenuItem::Separator));

        // Copy submenu
        match &items[4] {
            MenuItem::Submenu { label, items, .. } => {
                assert_eq!(label, tr!("common.copy").as_str());
                let sub_items = cx.update(|cx| items(cx));
                assert_eq!(sub_items.len(), 3);
                match &sub_items[0] {
                    MenuItem::Entry { label, .. } => {
                        assert_eq!(label, tr!("session.copy_conversation").as_str())
                    }
                    _ => panic!("expected copy conversation entry"),
                }
                match &sub_items[1] {
                    MenuItem::Entry { label, .. } => {
                        assert_eq!(label, tr!("session.copy_all").as_str())
                    }
                    _ => panic!("expected copy all entry"),
                }
                match &sub_items[2] {
                    MenuItem::Entry { label, .. } => {
                        assert_eq!(label, tr!("session.copy_deeplink").as_str())
                    }
                    _ => panic!("expected copy deeplink entry"),
                }
            }
            _ => panic!("expected copy submenu"),
        }

        // Export submenu
        match &items[5] {
            MenuItem::Submenu { label, items, .. } => {
                assert_eq!(label, tr!("common.export").as_str());
                let sub_items = cx.update(|cx| items(cx));
                assert_eq!(sub_items.len(), 2);
                match &sub_items[0] {
                    MenuItem::Entry { label, .. } => {
                        assert_eq!(label, tr!("session.export_conversation").as_str())
                    }
                    _ => panic!("expected export conversation entry"),
                }
                match &sub_items[1] {
                    MenuItem::Entry { label, .. } => {
                        assert_eq!(label, tr!("session.export_all").as_str())
                    }
                    _ => panic!("expected export all entry"),
                }
            }
            _ => panic!("expected export submenu"),
        }

        assert!(matches!(&items[6], MenuItem::Separator));
        match &items[7] {
            MenuItem::Entry { label, .. } => assert_eq!(label, tr!("common.remove").as_str()),
            _ => panic!("expected remove entry"),
        }

        let pinned_items = sidebar_session_menu_items(session_id, true, WeakEntity::new_invalid());
        match &pinned_items[2] {
            MenuItem::Entry { label, .. } => assert_eq!(label, tr!("session.unpin").as_str()),
            _ => panic!("expected unpin entry"),
        }
    }
}
