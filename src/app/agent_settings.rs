//! Settings → Agents: the named sub-agents the composer's `^` picker offers.
//!
//! An agent is a model route (provider, model, channel, reasoning) under a
//! user-chosen name, plus optional standing instructions the daemon prepends
//! to every subtask it runs. The list lives in desktop settings; the composer
//! reads it through the in-memory delegation indexes, never from disk.
//!
//! The editor's model picker filters the catalogue routes already expanded
//! for delegation. Its rows are memoized per query and source index, so a
//! caret blink re-renders the page without re-running the fuzzy matcher.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{KeyBinding, Subscription};
use nucleo_matcher::Matcher;

use crate::composer_complete::{self, ModelCandidate, Scored};
use crate::delegation::{SubAgent, SubAgentNameError, validate_sub_agent_name};
use crate::ui::ActivationExt;
use crate::ui::menu::{DismissMenu, SelectNextEntry, SelectPreviousEntry};

use super::composer::next_picker_highlight;
use super::*;

/// Key context of the model picker's search field while it is open.
const MODEL_PICKER_CONTEXT: &str = "AgentModelPicker";
const MODEL_PICKER_FIELD_CONTEXT: &str = "AgentModelPicker > TextInput";
const MODEL_PICKER_ROW_HEIGHT: f32 = 32.0;
const MODEL_PICKER_MAX_HEIGHT: f32 = 240.0;

/// Bind the picker's keys. Must run after [`crate::input::init`] so the
/// arrows and `escape` outrank the field's own bindings at the same depth.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", SelectNextEntry, Some(MODEL_PICKER_FIELD_CONTEXT)),
        KeyBinding::new("up", SelectPreviousEntry, Some(MODEL_PICKER_FIELD_CONTEXT)),
        KeyBinding::new("escape", DismissMenu, Some(MODEL_PICKER_FIELD_CONTEXT)),
    ]);
}

/// The route an agent runs on, as chosen in the editor.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AgentRoute {
    provider: ProviderKind,
    model_id: String,
    sub_provider: Option<String>,
    reasoning_effort: Option<String>,
}

impl AgentRoute {
    fn of_agent(agent: &SubAgent) -> Self {
        Self {
            provider: agent.provider,
            model_id: agent.model_id.clone(),
            sub_provider: agent.sub_provider.clone(),
            reasoning_effort: agent.reasoning_effort.clone(),
        }
    }

    fn of_candidate(candidate: &ModelCandidate) -> Self {
        Self {
            provider: candidate.provider,
            model_id: candidate.model_id.clone(),
            sub_provider: candidate.sub_provider.clone(),
            reasoning_effort: candidate.reasoning.as_ref().map(|option| option.id.clone()),
        }
    }
}

/// Filtered picker rows for one (query, source index).
struct PickerMemo {
    query: String,
    source: usize,
    rows: Rc<Vec<Scored<ModelCandidate>>>,
}

pub(super) struct AgentEditor {
    /// The agent being edited; `None` while adding a new one.
    editing: Option<Uuid>,
    name: Entity<TextInput>,
    instructions: Entity<TextInput>,
    model_search: Entity<TextInput>,
    route: Option<AgentRoute>,
    picker_open: bool,
    picker_highlight: usize,
    /// Virtualized: the full catalogue can expand to hundreds of routes.
    picker_list: ListState,
    /// The list's offset as of the last wheel event, for scroll containment.
    picker_wheel_offset: Rc<std::cell::Cell<Pixels>>,
    picker_memo: RefCell<Option<PickerMemo>>,
    error: Option<String>,
    model_focus: FocusHandle,
    save_focus: FocusHandle,
    cancel_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

/// Cross-frame state for the Agents page.
pub(super) struct AgentSettings {
    pub(super) editor: Option<AgentEditor>,
    add_focus: FocusHandle,
    /// Per-row control focus, keyed by control id; created on first draw.
    row_focus: RefCell<HashMap<String, FocusHandle>>,
    matcher: RefCell<Matcher>,
}

impl AgentSettings {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            editor: None,
            add_focus: cx.focus_handle(),
            row_focus: RefCell::new(HashMap::new()),
            matcher: RefCell::new(composer_complete::matcher()),
        }
    }

    fn row_focus(&self, key: String, cx: &mut App) -> FocusHandle {
        self.row_focus
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }
}

impl Waku {
    /// Open Settings → Agents with a blank editor, focused on the name. The
    /// composer's `^` list ends with this entry.
    pub(super) fn open_new_sub_agent_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings_action(&OpenSettings, window, cx);
        self.open_settings_page(SettingsPage::Agents, cx);
        self.open_sub_agent_editor(None, window, cx);
    }

    fn open_sub_agent_editor(
        &mut self,
        editing: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let agent = editing.and_then(|id| {
            self.state
                .sub_agents
                .iter()
                .find(|agent| agent.id == id)
                .cloned()
        });
        let name =
            cx.new(|cx| TextInput::new(window, cx).placeholder(tr!("agents.name_placeholder")));
        let instructions = cx.new(|cx| {
            TextInput::new(window, cx)
                .multi_line()
                .placeholder(tr!("agents.instructions_placeholder"))
        });
        let model_search =
            cx.new(|cx| TextInput::new(window, cx).placeholder(tr!("agents.search_models")));
        if let Some(agent) = agent.as_ref() {
            name.update(cx, |input, cx| input.set_content(agent.name.clone(), cx));
            instructions.update(cx, |input, cx| {
                input.set_content(agent.instructions.clone(), cx)
            });
        }
        let subscriptions = vec![
            cx.subscribe_in(
                &name,
                window,
                |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Edited => {
                        if let Some(editor) = this.agent_settings.editor.as_mut() {
                            editor.error = None;
                        }
                        cx.notify();
                    }
                    InputEvent::Submit(_) => this.save_sub_agent_editor(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &model_search,
                window,
                |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Edited => {
                        if let Some(editor) = this.agent_settings.editor.as_mut() {
                            editor.picker_highlight = 0;
                            editor.picker_list.scroll_to_reveal_item(0);
                        }
                        cx.notify();
                    }
                    InputEvent::Submit(_) => this.choose_picker_route(None, window, cx),
                    _ => {}
                },
            ),
        ];
        let name_focus = name.read(cx).focus();
        self.agent_settings.editor = Some(AgentEditor {
            editing: agent.as_ref().map(|agent| agent.id),
            name,
            instructions,
            model_search,
            route: agent.as_ref().map(AgentRoute::of_agent),
            picker_open: false,
            picker_highlight: 0,
            picker_list: ListState::new(0, ListAlignment::Top, px(MODEL_PICKER_MAX_HEIGHT)),
            picker_wheel_offset: Rc::new(std::cell::Cell::new(Pixels::ZERO)),
            picker_memo: RefCell::new(None),
            error: None,
            model_focus: cx.focus_handle(),
            save_focus: cx.focus_handle(),
            cancel_focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        });
        // Opening Settings hands focus to its tab in a deferred effect; this
        // one is queued after it, so typing lands in the new name field.
        window.defer(cx, move |window, cx| window.focus(&name_focus, cx));
        cx.notify();
    }

    fn close_sub_agent_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent_settings.editor.take().is_some() {
            window.focus(&self.agent_settings.add_focus, cx);
        }
        cx.notify();
    }

    fn save_sub_agent_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.agent_settings.editor.as_mut() else {
            return;
        };
        let name = editor.name.read(cx).content().trim().to_owned();
        let instructions = editor.instructions.read(cx).content().trim().to_owned();
        if let Err(error) = validate_sub_agent_name(&name, &self.state.sub_agents, editor.editing) {
            editor.error = Some(match error {
                SubAgentNameError::Empty => tr!("agents.error_name_empty"),
                SubAgentNameError::InvalidCharacter => tr!("agents.error_name_invalid"),
                SubAgentNameError::Duplicate => tr!("agents.error_name_duplicate"),
            });
            cx.notify();
            return;
        }
        let Some(route) = editor.route.clone() else {
            editor.error = Some(tr!("agents.error_model_missing"));
            cx.notify();
            return;
        };
        let agent = SubAgent {
            id: editor.editing.unwrap_or_else(Uuid::new_v4),
            name,
            provider: route.provider,
            model_id: route.model_id,
            sub_provider: route.sub_provider,
            reasoning_effort: route.reasoning_effort,
            instructions,
        };
        match self
            .state
            .sub_agents
            .iter_mut()
            .find(|existing| existing.id == agent.id)
        {
            Some(existing) => *existing = agent,
            None => self.state.sub_agents.push(agent),
        }
        self.agent_settings.editor = None;
        window.focus(&self.agent_settings.add_focus, cx);
        self.save();
        self.refresh_delegation_model_index();
        cx.notify();
    }

    fn delete_sub_agent(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let before = self.state.sub_agents.len();
        self.state.sub_agents.retain(|agent| agent.id != id);
        if self.state.sub_agents.len() == before {
            return;
        }
        if self
            .agent_settings
            .editor
            .as_ref()
            .is_some_and(|editor| editor.editing == Some(id))
        {
            self.agent_settings.editor = None;
        }
        // The deleted row's controls are gone; keep keyboard focus on the page.
        window.focus(&self.agent_settings.add_focus, cx);
        self.save();
        self.refresh_delegation_model_index();
        cx.notify();
    }

    // ── Model picker ───────────────────────────────────────────────────────

    fn toggle_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.agent_settings.editor.as_mut() else {
            return;
        };
        editor.picker_open = !editor.picker_open;
        if editor.picker_open {
            // Open on the current route so `enter` keeps it.
            editor.model_search.update(cx, |input, cx| input.clear(cx));
            editor.picker_highlight = 0;
            let focus = editor.model_search.read(cx).focus();
            window.focus(&focus, cx);
            let rows = self.picker_rows(cx);
            if let Some(editor) = self.agent_settings.editor.as_mut()
                && let Some(route) = editor.route.as_ref()
                && let Some(index) = rows
                    .iter()
                    .position(|row| AgentRoute::of_candidate(&row.item) == *route)
            {
                editor.picker_highlight = index;
                editor.picker_list.scroll_to_reveal_item(index);
            }
        } else {
            let focus = editor.model_focus.clone();
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    fn close_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.agent_settings.editor.as_mut() else {
            return;
        };
        if editor.picker_open {
            editor.picker_open = false;
            let focus = editor.model_focus.clone();
            window.focus(&focus, cx);
            cx.notify();
        }
    }

    fn step_model_picker(&mut self, key: &str, cx: &mut Context<Self>) {
        let rows = self.picker_rows(cx);
        let Some(editor) = self.agent_settings.editor.as_mut() else {
            return;
        };
        let current = editor.picker_highlight.min(rows.len().saturating_sub(1));
        if let Some(next) = next_picker_highlight(Some(current), rows.len(), key) {
            editor.picker_highlight = next;
            editor.picker_list.scroll_to_reveal_item(next);
            cx.notify();
        }
    }

    /// Take the clicked row, or the highlighted one when `index` is `None`.
    fn choose_picker_route(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.picker_rows(cx);
        let Some(editor) = self.agent_settings.editor.as_mut() else {
            return;
        };
        if !editor.picker_open {
            return;
        }
        let index = index.unwrap_or(editor.picker_highlight);
        let Some(row) = rows.get(index.min(rows.len().saturating_sub(1))) else {
            return;
        };
        editor.route = Some(AgentRoute::of_candidate(&row.item));
        editor.picker_open = false;
        editor.error = None;
        // The search field is about to unmount; keep focus on the control
        // that reopens the picker.
        let focus = editor.model_focus.clone();
        window.focus(&focus, cx);
        cx.notify();
    }

    /// The picker's rows for the current query, memoized per query and index.
    fn picker_rows(&self, cx: &App) -> Rc<Vec<Scored<ModelCandidate>>> {
        let Some(editor) = self.agent_settings.editor.as_ref() else {
            return Rc::new(Vec::new());
        };
        // The field's content is plain memory; reading it is per-frame safe.
        let query = editor.model_search.read(cx).content().to_owned();
        let source = Rc::as_ptr(&self.delegation_route_index) as usize;
        if let Some(memo) = editor.picker_memo.borrow().as_ref()
            && memo.query == query
            && memo.source == source
        {
            return memo.rows.clone();
        }
        // Uncapped: every provider's routes must stay reachable, and the
        // list is virtualized so its length costs nothing per frame.
        let rows = Rc::new(composer_complete::filter_models_with_cap(
            &self.delegation_route_index,
            &query,
            &mut self.agent_settings.matcher.borrow_mut(),
            usize::MAX,
        ));
        editor.picker_list.reset_with_uniform_height(rows.len(), px(MODEL_PICKER_ROW_HEIGHT));
        *editor.picker_memo.borrow_mut() = Some(PickerMemo {
            query,
            source,
            rows: rows.clone(),
        });
        rows
    }
}

// ── Rendering ──────────────────────────────────────────────────────────────

impl Waku {
    pub(super) fn render_agent_settings(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let editing = self.agent_settings.editor.is_some();
        let add = settings_button(
            "agents-add",
            "icons/plus.svg",
            tr!("agents.add_agent"),
            &self.agent_settings.add_focus,
            editing,
            theme,
        )
        .when(!editing, |button| {
            button.on_activation(cx, |this, window, cx| {
                this.open_sub_agent_editor(None, window, cx)
            })
        });
        let intro = div()
            .mt(px(15.0))
            .flex()
            .items_center()
            .gap(px(24.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(sp(12.5))
                    .line_height(sp(18.0))
                    .text_color(theme.text_secondary)
                    .child(tr!("agents.description")),
            )
            .child(add);
        let body = if editing {
            self.render_agent_editor(theme, window, cx)
        } else {
            self.render_agent_list(theme, cx)
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(intro)
            .child(body)
            .into_any_element()
    }

    fn render_agent_list(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let card = div()
            .mt(px(15.0))
            .w_full()
            .flex()
            .flex_col()
            .rounded(px(13.0))
            .overflow_hidden()
            .bg(theme.raised);
        if self.state.sub_agents.is_empty() {
            return card
                .child(
                    div()
                        .min_h(px(60.0))
                        .px(px(20.0))
                        .py(px(14.0))
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .text_size(sp(12.5))
                        .text_color(theme.text_tertiary)
                        .child(icon("icons/bot.svg", 15.0, theme.text_tertiary))
                        .child(tr!("agents.empty")),
                )
                .into_any_element();
        }
        let mut card = card;
        for (index, agent) in self.state.sub_agents.iter().enumerate() {
            if index > 0 {
                card = card.child(div().mx(px(20.0)).h(px(1.0)).bg(theme.border));
            }
            card = card.child(self.render_agent_row(agent, theme, cx));
        }
        card.into_any_element()
    }

    fn render_agent_row(&self, agent: &SubAgent, theme: Theme, cx: &mut Context<Self>) -> Div {
        let id = agent.id;
        let available = self
            .sub_agent_index
            .iter()
            .any(|candidate| candidate.agent.as_ref().is_some_and(|known| known.id == id));
        let route = self.route_summary(&AgentRoute::of_agent(agent));
        let instructions = agent
            .instructions
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned);
        let edit_focus = self.agent_settings.row_focus(format!("edit-{id}"), cx);
        let delete_focus = self.agent_settings.row_focus(format!("delete-{id}"), cx);
        let edit = icon_button(
            SharedString::from(format!("agent-edit-{id}")),
            "icons/pencil.svg",
            &edit_focus,
            theme,
        )
        .tooltip(Tooltip::text(tr!(
            "agents.edit_named",
            name = agent.name.clone()
        )))
        .on_activation(cx, move |this, window, cx| {
            this.open_sub_agent_editor(Some(id), window, cx)
        });
        let delete = icon_button(
            SharedString::from(format!("agent-delete-{id}")),
            "icons/trash.svg",
            &delete_focus,
            theme,
        )
        .tooltip(Tooltip::text(tr!(
            "agents.delete_named",
            name = agent.name.clone()
        )))
        .on_activation(cx, move |this, window, cx| {
            this.delete_sub_agent(id, window, cx)
        });
        div()
            .w_full()
            .min_h(px(60.0))
            .px(px(20.0))
            .py(px(12.0))
            .flex()
            .items_center()
            .gap(px(14.0))
            .child(
                div()
                    .size(px(32.0))
                    .flex_none()
                    .rounded(px(8.0))
                    .bg(theme.overlay_strong)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon("icons/bot.svg", 15.0, theme.text_tertiary)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(sp(13.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(SharedString::from(format!("^{}", agent.name))),
                            )
                            .when(!available, |row| {
                                row.child(
                                    div()
                                        .id(SharedString::from(format!("agent-unavailable-{id}")))
                                        .flex_none()
                                        .h(px(18.0))
                                        .px(px(6.0))
                                        .rounded(px(4.0))
                                        .border_1()
                                        .border_color(theme.border)
                                        .flex()
                                        .items_center()
                                        .gap(px(4.0))
                                        .text_size(sp(11.5))
                                        .text_color(theme.warning)
                                        .tooltip(Tooltip::text(tr!("agents.unavailable_tooltip")))
                                        .child(icon("icons/alert.svg", 10.0, theme.warning))
                                        .child(tr!("agents.unavailable")),
                                )
                            }),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(sp(12.5))
                            .text_color(theme.text_secondary)
                            .child(SharedString::from(route)),
                    )
                    .when_some(instructions, |column, line| {
                        column.child(
                            div()
                                .truncate()
                                .text_size(sp(12.5))
                                .text_color(theme.text_ghost)
                                .child(SharedString::from(line)),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(edit)
                    .child(delete),
            )
    }

    /// "Model · reasoning — channel" for a route, from the catalogue when it
    /// knows the model and from raw IDs when it does not (yet).
    fn route_summary(&self, route: &AgentRoute) -> String {
        let candidate = self
            .delegation_route_index
            .iter()
            .find(|candidate| AgentRoute::of_candidate(candidate) == *route);
        let model = match candidate {
            Some(candidate) => candidate.model_label(),
            None => match route.reasoning_effort.as_deref() {
                Some(effort) => format!("{} · {effort}", route.model_id),
                None => route.model_id.clone(),
            },
        };
        let source = route_source_label(route.provider, route.sub_provider.as_deref());
        format!("{model} — {source}")
    }

    fn render_agent_editor(
        &self,
        theme: Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(editor) = self.agent_settings.editor.as_ref() else {
            return div().into_any_element();
        };
        let title = if editor.editing.is_some() {
            tr!("agents.edit_agent")
        } else {
            tr!("agents.new_agent")
        };
        let model_label = editor
            .route
            .as_ref()
            .map(|route| self.route_summary(route))
            .unwrap_or_else(|| tr!("agents.choose_model"));
        let model_chip = div()
            .id("agent-model-chip")
            .track_focus(&editor.model_focus)
            .tab_index(0)
            .h(px(28.0))
            .px(px(8.0))
            .max_w(px(430.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(if editor.picker_open {
                theme.accent
            } else {
                theme.border_strong
            })
            .bg(theme.inset)
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_default()
            .text_size(sp(12.5))
            .text_color(if editor.route.is_some() {
                theme.text
            } else {
                theme.text_ghost
            })
            .focus_visible(|style| style.border_color(theme.accent))
            .hover(|style| style.bg(theme.overlay))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(model_label)),
            )
            .child(icon(
                if editor.picker_open {
                    "icons/chevron-down.svg"
                } else {
                    "icons/chevron-right.svg"
                },
                11.0,
                theme.text_tertiary,
            ))
            .on_activation(cx, |this, window, cx| this.toggle_model_picker(window, cx));

        let mut model_field = div().flex().flex_col().gap(px(6.0)).child(model_chip);
        if editor.picker_open {
            model_field = model_field.child(self.render_model_picker(editor, theme, cx));
        }

        let cancel = settings_button(
            "agent-editor-cancel",
            "icons/x.svg",
            tr!("common.cancel"),
            &editor.cancel_focus,
            false,
            theme,
        )
        .on_activation(cx, |this, window, cx| {
            this.close_sub_agent_editor(window, cx)
        });
        let save = settings_button(
            "agent-editor-save",
            "icons/check.svg",
            tr!("agents.save"),
            &editor.save_focus,
            false,
            theme,
        )
        .text_color(theme.text)
        .border_color(theme.accent)
        .on_activation(cx, |this, window, cx| {
            this.save_sub_agent_editor(window, cx)
        });

        div()
            .mt(px(15.0))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .rounded(px(13.0))
            .bg(theme.raised)
            .px(px(20.0))
            .py(px(16.0))
            .child(
                div()
                    .text_size(sp(13.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(title),
            )
            .child(editor_field(
                tr!("agents.name"),
                tr!("agents.name_description"),
                TextField::new("agent-name-field", editor.name.clone())
                    .max_w(px(430.0))
                    .into_any_element(),
                theme,
            ))
            .child(editor_field(
                tr!("agents.model"),
                tr!("agents.model_description"),
                model_field.into_any_element(),
                theme,
            ))
            .child(editor_field(
                tr!("agents.instructions"),
                tr!("agents.instructions_description"),
                div()
                    .h(px(120.0))
                    .px(px(8.0))
                    .py(px(6.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(
                        if editor.instructions.read(cx).is_visually_focused(window) {
                            theme.accent
                        } else {
                            theme.border_strong
                        },
                    )
                    .bg(theme.inset)
                    .text_size(sp(12.5))
                    .line_height(sp(18.0))
                    .text_color(theme.text)
                    .child(editor.instructions.clone())
                    .into_any_element(),
                theme,
            ))
            .when_some(editor.error.clone(), |form, error| {
                form.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(sp(12.5))
                        .text_color(theme.warning)
                        .child(icon("icons/alert.svg", 12.0, theme.warning))
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(cancel)
                    .child(save),
            )
            .into_any_element()
    }

    fn render_model_picker(
        &self,
        editor: &AgentEditor,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = self.picker_rows(cx);
        let highlight = editor.picker_highlight.min(rows.len().saturating_sub(1));
        let list = if rows.is_empty() {
            let empty = if self.delegation_route_index.is_empty() {
                tr!("agents.no_models")
            } else {
                tr!("agents.no_matching_models")
            };
            div()
                .p(px(4.0))
                .child(
                    div()
                        .min_h(px(MODEL_PICKER_ROW_HEIGHT))
                        .px(px(8.0))
                        .py(px(6.0))
                        .flex()
                        .items_center()
                        .text_size(sp(12.5))
                        .text_color(theme.text_tertiary)
                        .child(empty),
                )
                .into_any_element()
        } else {
            let height =
                (rows.len() as f32 * MODEL_PICKER_ROW_HEIGHT + 8.0).min(MODEL_PICKER_MAX_HEIGHT);
            let route = editor.route.clone();
            let weak = cx.entity().downgrade();
            // Keyboard reveals also move the list; resync so only a wheel
            // event's own movement counts as consumed.
            editor
                .picker_wheel_offset
                .set(editor.picker_list.scroll_px_offset_for_scrollbar().y);
            let wheel_list = editor.picker_list.clone();
            let wheel_offset = editor.picker_wheel_offset.clone();
            div()
                .id("agent-model-picker-list")
                .w_full()
                .h(px(height))
                .flex_none()
                .py(px(4.0))
                .px(px(4.0))
                // The page behind scrolls too; keep the gesture here until
                // the list reaches an edge.
                .on_scroll_wheel(move |_, _, cx| {
                    crate::ui::contain_list_scroll(&wheel_list, &wheel_offset, cx)
                })
                .child(
                    list(editor.picker_list.clone(), move |index, _window, _cx| {
                        let Some(row) = rows.get(index) else {
                            return div().into_any_element();
                        };
                        let candidate = &row.item;
                        let selected = route
                            .as_ref()
                            .is_some_and(|route| AgentRoute::of_candidate(candidate) == *route);
                        let weak = weak.clone();
                        div()
                            .id(("agent-model-row", index))
                            .w_full()
                            .h(px(MODEL_PICKER_ROW_HEIGHT))
                            .px(px(8.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .cursor_default()
                            .when(index == highlight, |row| row.bg(theme.overlay_strong))
                            .hover(|row| row.bg(theme.overlay))
                            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.choose_picker_route(Some(index), window, cx);
                                });
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(sp(12.5))
                                    .text_color(theme.text)
                                    .child(SharedString::from(candidate.model_label())),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .max_w(px(160.0))
                                    .truncate()
                                    .text_size(sp(12.5))
                                    .text_color(theme.text_tertiary)
                                    .child(SharedString::from(route_source_label(
                                        candidate.provider,
                                        candidate.sub_provider.as_deref(),
                                    ))),
                            )
                            .child(div().size(px(12.0)).flex_none().when(selected, |slot| {
                                slot.child(icon("icons/check.svg", 12.0, theme.text_secondary))
                            }))
                            .into_any_element()
                    })
                    .size_full(),
                )
                .into_any_element()
        };
        div()
            .key_context(MODEL_PICKER_CONTEXT)
            .on_action(cx.listener(|this, _: &SelectNextEntry, _, cx| {
                this.step_model_picker("down", cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousEntry, _, cx| {
                this.step_model_picker("up", cx);
            }))
            .on_action(cx.listener(|this, _: &DismissMenu, window, cx| {
                this.close_model_picker(window, cx);
            }))
            .max_w(px(430.0))
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface)
            .overflow_hidden()
            .child(
                div()
                    .p(px(6.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        TextField::new("agent-model-search", editor.model_search.clone())
                            .icon("icons/search.svg", 13.0)
                            .w_full(),
                    ),
            )
            .child(list)
            .into_any_element()
    }
}

/// "Pi · Deepseek": the provider that runs the route, then the channel it
/// calls through. The provider leads because it is what must be installed
/// and switched on; a channel that only repeats the provider is dropped.
pub(super) fn route_source_label(provider: ProviderKind, sub_provider: Option<&str>) -> String {
    let provider_name = provider.short_name();
    match sub_provider.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) if !name.eq_ignore_ascii_case(provider_name) => {
            format!("{provider_name} · {name}")
        }
        _ => provider_name.to_owned(),
    }
}

/// A labelled form row: title, caption, then the control.
fn editor_field(title: String, caption: String, control: AnyElement, theme: Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(5.0))
        .child(
            div()
                .text_size(sp(12.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(title),
        )
        .child(
            div()
                .text_size(sp(12.5))
                .line_height(sp(16.0))
                .text_color(theme.text_tertiary)
                .child(caption),
        )
        .child(div().mt(px(3.0)).child(control))
}

/// An outlined settings button with a leading icon.
fn settings_button(
    id: &'static str,
    icon_path: &'static str,
    label: String,
    focus: &FocusHandle,
    disabled: bool,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .track_focus(focus)
        .tab_index(0)
        .flex_none()
        .min_h(px(30.0))
        .px(px(11.0))
        .rounded(px(7.0))
        .border_1()
        .border_color(theme.border_strong)
        .flex()
        .items_center()
        .gap(px(6.0))
        .cursor_default()
        .text_size(sp(12.5))
        .text_color(theme.text_secondary)
        .focus_visible(|style| style.border_color(theme.accent))
        .when(disabled, |style| style.opacity(0.45))
        .when(!disabled, |style| {
            style
                .hover(|style| style.bg(theme.overlay))
                .active(|style| style.bg(theme.overlay_strong))
        })
        .child(icon(icon_path, 12.0, theme.text_tertiary))
        .child(label)
}

/// A square icon-only button; callers attach a tooltip naming its action.
fn icon_button(
    id: SharedString,
    icon_path: &'static str,
    focus: &FocusHandle,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .track_focus(focus)
        .tab_index(0)
        .size(px(28.0))
        .rounded(px(7.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_default()
        .border_1()
        .border_color(gpui::transparent_black())
        .focus_visible(|style| style.border_color(theme.accent))
        .hover(|style| style.bg(theme.overlay))
        .active(|style| style.bg(theme.overlay_strong))
        .child(icon(icon_path, 13.0, theme.text_tertiary))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str) -> SubAgent {
        SubAgent {
            id: Uuid::new_v4(),
            name: name.into(),
            provider: ProviderKind::Codex,
            model_id: "gpt-5".into(),
            sub_provider: None,
            reasoning_effort: Some("high".into()),
            instructions: String::new(),
        }
    }

    #[test]
    fn route_source_names_the_provider_before_its_channel() {
        assert_eq!(
            route_source_label(ProviderKind::Pi, Some("Deepseek")),
            "Pi · Deepseek"
        );
        assert_eq!(route_source_label(ProviderKind::Pi, Some(" pi ")), "Pi");
        assert_eq!(route_source_label(ProviderKind::Pi, Some("  ")), "Pi");
        assert_eq!(route_source_label(ProviderKind::Codex, None), "Codex");
    }

    #[test]
    fn agent_names_must_be_unique_markers() {
        let agents = vec![agent("Reviewer")];
        assert_eq!(
            validate_sub_agent_name("  ", &agents, None),
            Err(SubAgentNameError::Empty)
        );
        assert_eq!(
            validate_sub_agent_name("a]b", &agents, None),
            Err(SubAgentNameError::InvalidCharacter)
        );
        assert_eq!(
            validate_sub_agent_name("reviewer", &agents, None),
            Err(SubAgentNameError::Duplicate)
        );
        // Renaming an agent to its own name (in another case) is fine.
        assert_eq!(
            validate_sub_agent_name("REVIEWER", &agents, Some(agents[0].id)),
            Ok(())
        );
        assert_eq!(validate_sub_agent_name("Security", &agents, None), Ok(()));
    }

    #[test]
    fn a_route_round_trips_through_its_candidate() {
        let agent = agent("Reviewer");
        let candidate = composer_complete::sub_agent_candidates(
            std::slice::from_ref(&agent),
            &[(
                ProviderKind::Codex,
                ProviderModel {
                    id: "gpt-5".into(),
                    name: "GPT-5".into(),
                    sub_provider: None,
                    is_default: false,
                    reasoning_efforts: vec![crate::model::ProviderModelOption::new("high", "High")],
                    default_reasoning_effort: None,
                    service_tiers: Vec::new(),
                    default_service_tier: None,
                    context_windows: Vec::new(),
                    default_context_window: None,
                },
            )],
        );
        assert_eq!(candidate.len(), 1);
        assert_eq!(
            AgentRoute::of_candidate(&candidate[0]),
            AgentRoute::of_agent(&agent)
        );
        assert_eq!(candidate[0].model_label(), "GPT-5 · High");
    }
}
