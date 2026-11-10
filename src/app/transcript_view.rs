use super::right_panel::{DiffRowStyle, render_diff_code_row};
use super::*;

const CHANGED_FILES_PREVIEW_LIMIT: usize = 3;
/// Keep one virtualized transcript row bounded even when a generator touches
/// hundreds of files. The full immutable list remains one click away in the
/// right panel.
const CHANGED_FILES_EXPANDED_LIMIT: usize = 12;
/// An expanded edit stays one transcript row tall; past this the diff scrolls
/// in place, the same as long command output.
const ACTIVITY_DIFF_MAX_HEIGHT: f32 = 400.0;
/// Width of the label column in a tool call's input fields: room for a
/// two-word argument name before it truncates.
const ACTIVITY_FIELD_LABEL_WIDTH: f32 = 120.0;
/// Aligns a hunk separator with the line numbers in the rows below it; see
/// `DiffRowStyle::ACTIVITY`.
const ACTIVITY_DIFF_GUTTER_WIDTH: f32 = 52.0;

#[derive(Clone, Debug)]
struct ConversationNavigationRailSnapshot {
    visible: bool,
    /// Shared with the `Raku` cache: the turns only change when the row-kinds
    /// fingerprint moves, so the per-frame equality check here is a pointer
    /// comparison rather than a walk over every turn's snippets.
    turns: Rc<Vec<TranscriptNavigationTurn>>,
    viewport_height: f32,
    active_turn: Option<Uuid>,
    reset_generation: u64,
    theme_is_dark: bool,
}

impl PartialEq for ConversationNavigationRailSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.visible == other.visible
            && Rc::ptr_eq(&self.turns, &other.turns)
            && self.viewport_height == other.viewport_height
            && self.active_turn == other.active_turn
            && self.reset_generation == other.reset_generation
            && self.theme_is_dark == other.theme_is_dark
    }
}

impl Default for ConversationNavigationRailSnapshot {
    fn default() -> Self {
        Self {
            visible: false,
            turns: Rc::new(Vec::new()),
            viewport_height: 0.0,
            active_turn: None,
            reset_generation: 0,
            theme_is_dark: true,
        }
    }
}

pub(super) struct ConversationNavigationRail {
    raku: Option<WeakEntity<Raku>>,
    snapshot: ConversationNavigationRailSnapshot,
    turn_list_state: ListState,
    turn_indexes: HashMap<Uuid, usize>,
    hovered_turn: Option<Uuid>,
    focused_turn: Option<Uuid>,
    focus_handles: HashMap<Uuid, FocusHandle>,
    visual_state: NavigationRailVisualState,
    transition_from: NavigationRailVisualState,
    animation_generation: u64,
}

impl ConversationNavigationRail {
    pub(super) fn new() -> Self {
        let turn_list_state = ListState::new(0, ListAlignment::Top, px(48.0))
            .with_uniform_item_height(px(NAVIGATION_RAIL_TURN_HEIGHT));
        turn_list_state.set_scroll_handler(|_, window, _| window.refresh());
        Self {
            raku: None,
            snapshot: ConversationNavigationRailSnapshot::default(),
            turn_list_state,
            turn_indexes: HashMap::new(),
            hovered_turn: None,
            focused_turn: None,
            focus_handles: HashMap::new(),
            visual_state: NavigationRailVisualState::default(),
            transition_from: NavigationRailVisualState::default(),
            animation_generation: 0,
        }
    }

    pub(super) fn set_raku(&mut self, raku: WeakEntity<Raku>) {
        self.raku = Some(raku);
    }

    fn set_snapshot(
        &mut self,
        snapshot: ConversationNavigationRailSnapshot,
        cx: &mut Context<Self>,
    ) {
        if self.snapshot == snapshot {
            return;
        }
        let reset = self.snapshot.reset_generation != snapshot.reset_generation;
        let turn_identity_changed = self.snapshot.turns.len() != snapshot.turns.len()
            || self
                .snapshot
                .turns
                .iter()
                .zip(snapshot.turns.iter())
                .any(|(previous, next)| previous.message_id != next.message_id);
        let active_turn_changed = self.snapshot.active_turn != snapshot.active_turn;
        if reset {
            self.hovered_turn = None;
            self.focused_turn = None;
            self.focus_handles.clear();
            self.visual_state = NavigationRailVisualState::default();
            self.transition_from = NavigationRailVisualState::default();
            self.animation_generation = self.animation_generation.wrapping_add(1);
        } else if turn_identity_changed {
            self.focus_handles.retain(|message_id, _| {
                snapshot
                    .turns
                    .iter()
                    .any(|turn| turn.message_id == *message_id)
            });
            if self
                .focused_turn
                .is_some_and(|message_id| !self.focus_handles.contains_key(&message_id))
            {
                self.focused_turn = None;
            }
        }
        if reset || turn_identity_changed {
            self.turn_list_state
                .reset_with_uniform_height(snapshot.turns.len(), px(NAVIGATION_RAIL_TURN_HEIGHT));
            self.turn_indexes.clear();
            self.turn_indexes.extend(
                snapshot
                    .turns
                    .iter()
                    .enumerate()
                    .map(|(index, turn)| (turn.message_id, index)),
            );
        }
        self.snapshot = snapshot;
        if (reset || turn_identity_changed || active_turn_changed)
            && let Some(active_index) = self
                .snapshot
                .active_turn
                .and_then(|message_id| self.turn_indexes.get(&message_id).copied())
        {
            self.turn_list_state.scroll_to_reveal_item(active_index);
        }
        cx.notify();
    }
}

impl Raku {
    // ── Transcript ─────────────────────────────────────────────────────────

    pub(super) fn transcript_control_focus(
        &self,
        key: impl Into<String>,
        cx: &mut App,
    ) -> FocusHandle {
        self.transcript_control_focuses
            .borrow_mut()
            .entry(key.into())
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    pub(super) fn render_transcript(
        &mut self,
        window: &mut Window,
        chat_viewport_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.prefetch_checkpoint_refs(cx);
        self.sync_transcript_rows();
        self.sync_follow_up_row();
        self.sync_transcript_layout_width(window);
        self.apply_pending_transcript_search_reveal(window, cx);
        let search_bar = self.render_transcript_search_bar(chat_viewport_width, cx);
        let transcript_rows = self.active_transcript_rows().clone();
        // A scrollbar drag owns the position for as long as it lasts, and the
        // bar writes offsets straight into the list rather than through its
        // scroll handler, so nothing else reports one. Release following as the
        // thumb is taken — otherwise the pins below haul the view back on every
        // frame of the drag — and hand back the same question a wheel scroll
        // asks when the thumb is let go: did the reader come to rest on the
        // tail?
        let scrollbar_dragging = self.transcript_scrollbar.is_grabbed();
        if self
            .transcript_scrollbar_dragging
            .replace(scrollbar_dragging)
            != scrollbar_dragging
        {
            if scrollbar_dragging {
                self.transcript_anchor_following.set(false);
                self.transcript_is_scrolled.set(true);
                self.transcript_tail_recheck.set(false);
            } else {
                self.transcript_tail_recheck.set(true);
            }
        }
        let anchor_end_space = self.update_transcript_anchor_end_space(window);
        if self.transcript_anchor_following.get()
            && anchor_end_space <= Pixels::ZERO
            && self
                .selected_transcript_anchor_row()
                .is_some_and(|anchor_row| anchor_row + 1 < transcript_rows.item_count())
        {
            transcript_rows.scroll_to(ListOffset {
                item_ix: transcript_rows.item_count(),
                offset_in_item: Pixels::ZERO,
            });
            self.transcript_is_scrolled.set(false);
        }
        let entity = cx.entity().downgrade();
        let scrollbar_handle = transcript_rows.clone();
        let viewport_bounds = transcript_rows.viewport_bounds();
        let transcript_scrollable = viewport_bounds.size.height > Pixels::ZERO
            && transcript_rows.max_offset_for_scrollbar().y > px(0.5);
        let viewport_bottom = viewport_bounds.bottom();
        let tail_bottom = transcript_rows
            .item_count()
            .checked_sub(1)
            .and_then(|last_row| transcript_rows.bounds_for_item(last_row))
            .map(|bounds| bounds.bottom());
        // Scrolling back down onto the tail by hand re-engages following, just
        // as the affordance below does. GPUI re-engages its own tail pin when a
        // bottom-aligned list reaches the end, but a turn renders through the
        // top-aligned anchored list and the pin there is ours, so without this
        // the reader is stranded mid-stream after a round trip up and back —
        // watching the reply grow past the bottom edge with no way but the
        // button to rejoin it.
        if self.transcript_tail_recheck.get()
            && let Some(rests_at_tail) =
                transcript_rests_at_tail(viewport_bottom, tail_bottom, anchor_end_space)
        {
            self.transcript_tail_recheck.set(false);
            if rests_at_tail {
                self.pin_transcript_to_tail();
            }
        }
        let scroll_to_bottom_visible = should_show_scroll_to_bottom(
            self.transcript_is_scrolled.get(),
            self.transcript_anchor_following.get(),
            transcript_scrollable,
            viewport_bottom,
            tail_bottom,
            anchor_end_space,
        )
        .unwrap_or_else(|| self.transcript_scroll_to_bottom_visible.get());
        self.transcript_scroll_to_bottom_visible
            .set(scroll_to_bottom_visible);
        let scroll_to_bottom = scroll_to_bottom_visible.then(|| {
            let theme = Theme::current(cx);
            let focus = self.transcript_control_focus("transcript-scroll-to-bottom", cx);
            div()
                .id("transcript-scroll-to-bottom-layer")
                .absolute()
                .left_0()
                .bottom(px(8.0))
                .w_full()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("transcript-scroll-to-bottom")
                        .track_focus(&focus)
                        .tab_index(0)
                        .size(px(32.0))
                        .rounded_full()
                        .border_1()
                        .border_color(theme.border_strong)
                        .bg(theme.composer)
                        .shadow_xs()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_default()
                        .focus_visible(|style| style.border_color(theme.accent))
                        .hover(|style| style.bg(theme.raised))
                        .active(|style| style.bg(theme.overlay_strong))
                        .child(icon("icons/arrow-down.svg", 16.0, theme.text))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.scroll_transcript_to_bottom(cx);
                            cx.stop_propagation();
                        }))
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                this.scroll_transcript_to_bottom(cx);
                                cx.stop_propagation();
                            }
                        })),
                )
        });
        const NAVIGATION_RAIL_ENABLED: bool = true;
        let navigation_rail = NAVIGATION_RAIL_ENABLED.then(|| {
            let viewport_size = viewport_bounds.size;
            let navigation_turns = self.navigation_turns();
            let navigation_rail_visible = should_show_navigation_rail(
                transcript_scrollable,
                navigation_turns.len(),
                chat_viewport_width,
            );
            let scroll_top_row = transcript_rows.logical_scroll_top().item_ix;
            let turn_rows = navigation_turns
                .iter()
                .map(|turn| turn.row_index)
                .collect::<Vec<_>>();
            let active_turn = active_navigation_turn_index(
                &turn_rows,
                scroll_top_row,
                !self.transcript_is_scrolled.get(),
            )
            .map(|index| navigation_turns[index].message_id);
            let navigation_rail_snapshot = ConversationNavigationRailSnapshot {
                visible: navigation_rail_visible,
                turns: navigation_turns,
                viewport_height: f32::from(viewport_size.height),
                active_turn,
                reset_generation: self.navigation_rail_reset_generation.get(),
                theme_is_dark: Theme::current(cx).is_dark,
            };
            if self.navigation_rail.read(cx).snapshot != navigation_rail_snapshot {
                self.navigation_rail.update(cx, |rail, cx| {
                    rail.set_snapshot(navigation_rail_snapshot, cx)
                });
            }
            self.navigation_rail.clone().cached(
                StyleRefinement::default()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
            )
        });
        let transcript_focus = self.transcript_focus.clone();
        div()
            .font_family(self.fonts.transcript.clone())
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .key_context("Transcript")
            .track_focus(&transcript_focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    // A focusable row control under the click (the Edit &
                    // resend field, a button) already took focus in this
                    // bubble pass and prevented default. Taking it back here
                    // would leave a clicked text field unable to type.
                    if !window.default_prevented() {
                        window.focus(&this.transcript_focus, cx);
                    }
                }),
            )
            // Painted before any row, so the frame's selection registry holds
            // exactly the text elements this frame put on screen, in order.
            .child(md::render::frame_reset(self.transcript_selection.clone()))
            .child(
                list(transcript_rows, move |index, window, cx| {
                    entity
                        .upgrade()
                        .map(|entity| {
                            entity.update(cx, |this, cx| this.transcript_row(index, window, cx))
                        })
                        .unwrap_or_else(|| div().into_any_element())
                })
                .size_full()
                .pb(anchor_end_space),
            )
            .children(navigation_rail)
            .children(scroll_to_bottom)
            .child(scrollbar::vertical(
                &scrollbar_handle,
                &self.transcript_scrollbar,
            ))
            .child(self.transcript_selection_input())
            .children(search_bar)
            .into_any_element()
    }

    fn scroll_transcript_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.sync_transcript_rows();
        self.pin_transcript_to_tail();
        cx.notify();
    }

    /// Re-engage tail following from wherever the transcript sits now.
    ///
    /// What actually survives a growing reply is GPUI's past-the-end anchor,
    /// not the follow flag: `scroll_to_end` parks the list on `item_count`, and
    /// the layout walks backwards from there, so the last row's bottom stays
    /// against the viewport as that row grows. The flag alone only re-pins in
    /// the phase where the anchor's end space has already collapsed to zero.
    fn pin_transcript_to_tail(&self) {
        self.transcript_anchor_following
            .set(self.transcript_anchor.get().is_some());
        self.active_transcript_rows().scroll_to_end();
        self.transcript_is_scrolled.set(false);
    }

    /// Copy the transcript's text selection.
    ///
    /// This is the fallback leg of the copy shortcut: the composer holds focus almost
    /// always, so it handles the keystroke first and propagates when it has
    /// nothing selected of its own.
    pub(super) fn copy_selection_action(
        &mut self,
        _: &CopySelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let reviewing_diff = self.right_panel_visible
            && self
                .right_panel_active_surface
                .and_then(|index| self.right_panel_surfaces.get(index))
                .is_some_and(|surface| matches!(surface, RightPanelSurface::Diff));
        let reviewing_background_work = self.right_panel_visible
            && self
                .right_panel_active_surface
                .and_then(|index| self.right_panel_surfaces.get(index))
                .is_some_and(|surface| matches!(surface, RightPanelSurface::BackgroundWork { .. }));
        let selected = reviewing_diff
            .then(|| {
                self.right_panel_diff_selection
                    .selection
                    .borrow()
                    .selected_text()
            })
            .flatten()
            .or_else(|| {
                reviewing_background_work
                    .then(|| {
                        self.state
                            .selected_session
                            .and_then(|session_id| self.background_work.get(&session_id))
                            .and_then(BackgroundWorkRegistry::selected_text)
                    })
                    .flatten()
            })
            .or_else(|| self.toast_selection.selection.borrow().selected_text())
            .or_else(|| self.skills_selection.selection.borrow().selected_text())
            .or_else(|| self.side_chat.selection.selection.borrow().selected_text())
            .or_else(|| self.transcript_selection.selection.borrow().selected_text());
        match selected {
            Some(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            None => cx.propagate(),
        }
    }

    /// A zero-size canvas that installs the frame's selection mouse listeners.
    /// One set for the whole transcript: the registry already knows every
    /// painted element's geometry, so per-element listeners would be redundant.
    fn transcript_selection_input(&self) -> impl IntoElement {
        let selection = self.transcript_selection.clone();
        canvas(
            |_, _, _| (),
            move |_, _, window, _| md::render::install_selection_input(window, &selection),
        )
        .absolute()
        .w(px(0.0))
        .h(px(0.0))
    }

    pub(super) fn toast_selection_input(&self) -> impl IntoElement {
        let selection = self.toast_selection.clone();
        canvas(
            |_, _, _| (),
            move |_, _, window, _| md::render::install_selection_input(window, &selection),
        )
        .absolute()
        .w(px(0.0))
        .h(px(0.0))
    }
}

impl Render for ConversationNavigationRail {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.snapshot.visible {
            return div().into_any_element();
        }
        let theme = Theme::current(cx);
        let turns = self.snapshot.turns.clone();
        let turn_count = turns.len();
        let viewport_height = self.snapshot.viewport_height;
        if turn_count == 0 {
            return div().into_any_element();
        }
        let rail_height = navigation_rail_height(turn_count, viewport_height);
        if rail_height <= 0.0 {
            return div().into_any_element();
        }
        let rail_top = (viewport_height - rail_height).max(0.0) / 2.0;
        // The rail keeps a true one-to-one scroll position for every turn. Its
        // `ListState` only asks the builder for visible ticks plus overdraw, so
        // hover and scroll work remain bounded by the viewport even for a very
        // long conversation.
        let emphasized_turn = self.hovered_turn.or_else(|| {
            window
                .last_input_was_keyboard()
                .then_some(self.focused_turn)
                .flatten()
        });
        let emphasized_turn_index =
            emphasized_turn.and_then(|message_id| self.turn_indexes.get(&message_id).copied());
        let active_turn_index = self
            .snapshot
            .active_turn
            .and_then(|message_id| self.turn_indexes.get(&message_id).copied());
        let visual_state = NavigationRailVisualState { emphasized_turn };
        let previous_visual_state = self.visual_state;
        if previous_visual_state != visual_state {
            self.transition_from = previous_visual_state;
            self.visual_state = visual_state;
            self.animation_generation = self.animation_generation.wrapping_add(1);
        }
        let transition_from = self.transition_from;
        let from_emphasized_turn_index = transition_from
            .emphasized_turn
            .and_then(|message_id| self.turn_indexes.get(&message_id).copied());
        let animation_generation = self.animation_generation;
        let entity = cx.entity().downgrade();
        let turn_list_state = self.turn_list_state.clone();
        let tick_list = list(turn_list_state.clone(), move |turn_index, window, cx| {
            entity
                .upgrade()
                .map(|entity| {
                    entity.update(cx, |this, cx| {
                        this.render_navigation_rail_tick(
                            turn_index,
                            from_emphasized_turn_index,
                            emphasized_turn_index,
                            active_turn_index,
                            animation_generation,
                            window,
                            cx,
                        )
                    })
                })
                .unwrap_or_else(|| div().into_any_element())
        })
        .size_full();

        let (show_top_fade, show_bottom_fade) = navigation_rail_fade_visibility(
            self.turn_list_state.scroll_px_offset_for_scrollbar().y,
            self.turn_list_state.max_offset_for_scrollbar().y,
        );
        let transparent_surface = theme.surface.opacity(0.0);

        let rail = div()
            .id("conversation-navigation-rail")
            .absolute()
            .left(px(NAVIGATION_RAIL_LEFT))
            .top(px(rail_top))
            .w(px(NAVIGATION_RAIL_WIDTH))
            .h(px(rail_height))
            .relative()
            .overflow_hidden()
            .tab_index(0)
            .tab_group()
            .tab_stop(false)
            // The rail is an independent scroll surface over the transcript.
            // Its list handles the wheel first; stop the same gesture here so
            // it never falls through to the transcript, including when the
            // rail is already at either end.
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(tick_list)
            .when(show_top_fade, |rail| {
                rail.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .w_full()
                        .h(px(NAVIGATION_RAIL_FADE_HEIGHT))
                        .bg(linear_gradient(
                            180.0,
                            linear_color_stop(theme.surface, 0.0),
                            linear_color_stop(transparent_surface, 1.0),
                        )),
                )
            })
            .when(show_bottom_fade, |rail| {
                rail.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .w_full()
                        .h(px(NAVIGATION_RAIL_FADE_HEIGHT))
                        .bg(linear_gradient(
                            180.0,
                            linear_color_stop(transparent_surface, 0.0),
                            linear_color_stop(theme.surface, 1.0),
                        )),
                )
            });

        let preview = emphasized_turn_index.map(|turn_index| {
            let turn = &turns[turn_index];
            let scroll_top = -self.turn_list_state.scroll_px_offset_for_scrollbar().y;
            let preview_height = 126.0;
            let max_preview_top = (viewport_height - preview_height - 12.0).max(12.0);
            let preview_top = (rail_top + turn_index as f32 * NAVIGATION_RAIL_TURN_HEIGHT
                - f32::from(scroll_top)
                + NAVIGATION_RAIL_TURN_HEIGHT / 2.0
                - preview_height / 2.0)
                .clamp(12.0, max_preview_top);
            div()
                .absolute()
                .left(px(NAVIGATION_RAIL_LEFT
                    + NAVIGATION_RAIL_WIDTH
                    + NAVIGATION_RAIL_CONTENT_GAP))
                .top(px(preview_top))
                .w(px(320.0))
                .max_h(px(preview_height))
                .overflow_hidden()
                .rounded(px(14.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.raised)
                .shadow_lg()
                .px(px(15.0))
                .py(px(12.0))
                .flex()
                .flex_col()
                .gap(px(7.0))
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_size(sp(14.0))
                        .line_height(sp(20.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(SharedString::from(turn.prompt.clone())),
                )
                .when(!turn.response.is_empty(), |preview| {
                    preview.child(
                        div()
                            .w_full()
                            .max_h(px(60.0))
                            .overflow_hidden()
                            .whitespace_normal()
                            .text_size(sp(13.0))
                            .line_height(sp(20.0))
                            .text_color(theme.text_tertiary)
                            .child(SharedString::from(turn.response.clone())),
                    )
                })
        });

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(rail)
            .children(preview)
            .into_any_element()
    }
}

impl ConversationNavigationRail {
    #[allow(clippy::too_many_arguments)]
    fn render_navigation_rail_tick(
        &mut self,
        turn_index: usize,
        from_emphasized_turn_index: Option<usize>,
        emphasized_turn_index: Option<usize>,
        active_turn_index: Option<usize>,
        animation_generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(turn) = self.snapshot.turns.get(turn_index) else {
            return div().into_any_element();
        };
        let message_id = turn.message_id;
        let theme = Theme::current(cx);
        let focus_handle = self.navigation_rail_focus_handle(message_id, window, cx);
        let from_width = NAVIGATION_RAIL_TICK_WIDTH
            * navigation_rail_scale(turn_index, from_emphasized_turn_index);
        let to_width =
            NAVIGATION_RAIL_TICK_WIDTH * navigation_rail_scale(turn_index, emphasized_turn_index);
        let prominent =
            active_turn_index == Some(turn_index) || emphasized_turn_index == Some(turn_index);
        let tick_color = if prominent {
            if theme.is_dark {
                rgb(0xFFFFFF).into()
            } else {
                theme.text
            }
        } else {
            theme.text_ghost.opacity(NAVIGATION_RAIL_INACTIVE_OPACITY)
        };
        let click_focus = focus_handle.clone();
        let animation_id = SharedString::from(format!(
            "conversation-navigation-tick-animation-{message_id}-{animation_generation}"
        ));
        let tick = div()
            .h(px(NAVIGATION_RAIL_TICK_HEIGHT))
            .rounded_full()
            .bg(tick_color)
            .with_animation(
                animation_id,
                Animation::new(NAVIGATION_RAIL_ANIMATION_DURATION).with_easing(ease_out_quint()),
                move |element, delta| element.w(px(from_width + (to_width - from_width) * delta)),
            );

        div()
            .id(SharedString::from(format!(
                "conversation-navigation-turn-hit-{message_id}"
            )))
            .w(px(NAVIGATION_RAIL_WIDTH))
            .h(px(NAVIGATION_RAIL_TURN_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .cursor_default()
            .on_hover(cx.listener(move |this, hovering: &bool, _, cx| {
                if *hovering {
                    this.hovered_turn = Some(message_id);
                } else if this.hovered_turn == Some(message_id) {
                    this.hovered_turn = None;
                }
                cx.notify();
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                click_focus.focus(window, cx);
                this.activate_turn(message_id, cx);
            }))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "conversation-navigation-turn-focus-{message_id}"
                    )))
                    .w(px(NAVIGATION_RAIL_TICK_WIDTH + 4.0))
                    .h(px(8.0))
                    .ml(px(-2.0))
                    .pl(px(2.0))
                    .flex()
                    .items_center()
                    .rounded(px(4.0))
                    .track_focus(&focus_handle)
                    .tab_index(turn_index as isize)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .on_key_down(cx.listener(move |this, event, window, cx| {
                        this.navigation_rail_key_down(message_id, event, window, cx);
                    }))
                    .child(tick),
            )
            .into_any_element()
    }

    fn navigation_rail_focus_handle(
        &mut self,
        message_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> FocusHandle {
        if let Some(focus_handle) = self.focus_handles.get(&message_id).cloned() {
            return focus_handle;
        }

        let focus_handle = cx.focus_handle();
        cx.on_focus(&focus_handle, window, move |this: &mut Self, _, cx| {
            this.focused_turn = Some(message_id);
            cx.notify();
        })
        .detach();
        cx.on_blur(&focus_handle, window, move |this: &mut Self, _, cx| {
            if this.focused_turn == Some(message_id) {
                this.focused_turn = None;
            }
            cx.notify();
        })
        .detach();
        self.focus_handles.insert(message_id, focus_handle.clone());
        focus_handle
    }

    fn navigation_rail_key_down(
        &mut self,
        message_id: Uuid,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let turns = &self.snapshot.turns;
        let turn_count = turns.len();
        if turn_count == 0 {
            return;
        }
        let Some(turn_index) = self.turn_indexes.get(&message_id).copied() else {
            return;
        };

        let target_turn = match event.keystroke.key.as_str() {
            "up" => Some(turn_index.saturating_sub(1)),
            "down" => Some((turn_index + 1).min(turn_count - 1)),
            "home" => Some(0),
            "end" => Some(turn_count - 1),
            "enter" | "space" => {
                self.activate_turn(message_id, cx);
                cx.stop_propagation();
                return;
            }
            _ => None,
        };
        let Some(target_turn) = target_turn else {
            return;
        };
        self.turn_list_state.scroll_to_reveal_item(target_turn);
        let target_message_id = turns[target_turn].message_id;
        let focus_handle = self.navigation_rail_focus_handle(target_message_id, window, cx);
        focus_handle.focus(window, cx);
        cx.notify();
        cx.stop_propagation();
    }

    fn activate_turn(&self, message_id: Uuid, cx: &mut Context<Self>) {
        if let Some(raku) = &self.raku {
            let _ = raku.update(cx, |raku, cx| {
                raku.scroll_to_navigation_turn(message_id, cx)
            });
        }
    }
}

impl Raku {
    fn scroll_to_navigation_turn(&mut self, message_id: Uuid, cx: &mut Context<Self>) {
        let row_index = self
            .navigation_turns()
            .iter()
            .find(|turn| turn.message_id == message_id)
            .map(|turn| turn.row_index);
        let Some(row_index) = row_index else {
            return;
        };

        self.transcript_anchor_following.set(false);
        self.active_transcript_rows().scroll_to(ListOffset {
            item_ix: row_index,
            offset_in_item: Pixels::ZERO,
        });
        self.transcript_is_scrolled.set(true);
        cx.notify();
    }

    /// Open or close one turn-block disclosure, holding the reader's place.
    ///
    /// Re-measuring the row changes its height, and a bottom-aligned list keeps
    /// its pixel offset across that change — which lands the viewport past the
    /// end of the content and shows nothing until you scroll. Capturing the
    /// logical position before the change and restoring it after keeps the row
    /// exactly where it was.
    fn toggle_block_disclosure(
        &mut self,
        block_index: usize,
        cx: &mut Context<Self>,
        apply: impl FnOnce(&mut Self),
    ) {
        self.pin_transcript_for_disclosure();
        apply(self);
        // `remeasure_transcript_block` preserves the reader's scroll position
        // across the row's height change on its own.
        self.remeasure_transcript_block(block_index);
        cx.notify();
    }

    pub(super) fn toggle_activities(
        &mut self,
        block_index: usize,
        current: bool,
        cx: &mut Context<Self>,
    ) {
        self.toggle_block_disclosure(block_index, cx, |this| {
            this.activities_expanded.insert(block_index, !current);
        });
    }

    pub(super) fn toggle_activity_item(&mut self, id: Uuid, current: bool, cx: &mut Context<Self>) {
        let block_index = self
            .selected_transcript_blocks()
            .iter()
            .position(|block| block.activities.iter().any(|activity| activity.id == id));
        let Some(block_index) = block_index else {
            return;
        };
        self.toggle_block_disclosure(block_index, cx, |this| {
            this.expanded_activity_items.insert(id, !current);
            if current {
                // Collapsed: the rows would only be rebuilt from the same
                // changes if it reopens, so do not keep them alive for every
                // edit the session ever made.
                this.activity_diffs.borrow_mut().remove(&id);
                this.activity_diff_viewports.borrow_mut().remove(&id);
                this.activity_reads.borrow_mut().remove(&id);
                this.activity_inputs.borrow_mut().remove(&id);
            }
        });
    }

    /// Opens a file a tool changed in the right panel's viewer.
    ///
    /// Sits inside a row that toggles on click, so it stops the press from
    /// reaching that toggle, and answers the keyboard on its own.
    fn render_activity_open_file_button(
        &self,
        id: String,
        path: String,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focus = self.transcript_control_focus(id.clone(), cx);
        let key_path = path.clone();
        div()
            .id(SharedString::from(id))
            .track_focus(&focus)
            .tab_index(0)
            .size(px(20.0))
            .flex_none()
            .rounded(px(5.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|button| button.bg(theme.overlay_strong))
            .hover(|button| button.bg(theme.overlay_strong))
            .child(icon(
                "icons/file-bottom-left-arrow.svg",
                14.0,
                theme.text_ghost,
            ))
            .tooltip(Tooltip::text(tr!("activity.open_file")))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.open_activity_file(&path, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.open_activity_file(&key_path, cx);
                    cx.stop_propagation();
                }
            }))
            .into_any_element()
    }

    /// Diff rows for an expanded file-change activity, built on first sight and
    /// held until the activity collapses or its changes are replaced.
    fn activity_diff_rows(&self, activity: &ActivityItem) -> Rc<activity_diff::Diff> {
        if let Some(diff) = self.activity_diffs.borrow().get(&activity.id) {
            return diff.clone();
        }
        let diff = Rc::new(activity_diff::build(activity));
        self.activity_diffs
            .borrow_mut()
            .insert(activity.id, diff.clone());
        diff
    }

    /// Numbered code for an expanded read activity, built on first sight and
    /// held until the activity collapses or its output is replaced.
    fn activity_read_preview(
        &self,
        activity: &ActivityItem,
    ) -> Option<Rc<activity_read::ReadPreview>> {
        let source_len = activity.output.as_deref()?.len();
        if let Some(preview) = self.activity_reads.borrow().get(&activity.id)
            && preview.source_len == source_len
        {
            return Some(preview.clone());
        }
        let preview = Rc::new(activity_read::build(
            activity,
            self.selected_workspace_path(),
        )?);
        self.activity_reads
            .borrow_mut()
            .insert(activity.id, preview.clone());
        Some(preview)
    }

    /// Labeled fields for an expanded activity's arguments, parsed on first
    /// sight and held until the activity collapses or its arguments change.
    fn activity_input(&self, activity: &ActivityItem) -> Option<Rc<activity_input::Input>> {
        let arguments = activity.arguments.as_deref()?.trim();
        if let Some(input) = self.activity_inputs.borrow().get(&activity.id)
            && input.source_len == arguments.len()
        {
            return Some(input.clone());
        }
        let input = Rc::new(activity_input::build(arguments)?);
        self.activity_inputs
            .borrow_mut()
            .insert(activity.id, input.clone());
        Some(input)
    }

    pub(super) fn toggle_turn_fold(
        &mut self,
        turn_id: Uuid,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        self.pin_transcript_for_disclosure();
        let scroll_top = self.active_transcript_rows().logical_scroll_top();
        let previous_kinds = self.transcript_row_kinds.borrow().clone();
        let anchor_kind = previous_kinds.get(scroll_top.item_ix).copied();
        if expanded {
            self.expanded_turns.remove(&turn_id);
        } else {
            self.expanded_turns.insert(turn_id);
        }
        self.transcript_anchor_following.set(false);
        self.splice_transcript_rows_after_visibility_change(&previous_kinds);

        let next_kinds = self.transcript_row_kinds.borrow();
        let anchored_target =
            anchor_kind.and_then(|kind| next_kinds.iter().position(|candidate| *candidate == kind));
        let target = anchored_target.or_else(|| {
            next_kinds
                .iter()
                .position(|kind| *kind == TranscriptRowKind::TurnFold(turn_id))
        });
        drop(next_kinds);
        if let Some(item_ix) = target {
            self.active_transcript_rows().scroll_to(ListOffset {
                item_ix,
                offset_in_item: if anchored_target.is_some() {
                    scroll_top.offset_in_item
                } else {
                    Pixels::ZERO
                },
            });
            self.transcript_is_scrolled.set(true);
        }
        cx.notify();
    }

    /// A single transcript row, self-centered to the content column so the
    /// list can measure it at its true wrap width. Current-turn reasoning and
    /// activity blocks are anchored at the exact boundary between assistant
    /// text segments where their provider events arrived.
    pub(super) fn user_message_action_for_message(
        &self,
        message_index: usize,
    ) -> Option<UserMessageAction> {
        let session = self.selected_session()?;
        let message = session.messages.get(message_index)?;
        if message.role != MessageRole::User
            || !matches!(session.status, SessionStatus::Idle | SessionStatus::Failed)
        {
            return None;
        }
        let turn_id = message.turn_id?;
        let turn = session.turns.iter().find(|turn| turn.id == turn_id)?;
        if !session.provider.capabilities().conversation.rollback {
            return None;
        }
        // A steer joins the running turn as another user message. Provider
        // history rewinds by turn, so only its opening prompt is editable.
        if !message_opens_turn(&session.messages, message_index) {
            return None;
        }
        let retained_turn_count = turn.turn_count.saturating_sub(1);
        // Cache only — the ref lives in git, and this runs for every visible
        // user message on every frame. `prefetch_checkpoint_refs` fills the
        // cache off-thread and notifies.
        let has_ref = self
            .checkpoint_ref_cache
            .borrow()
            .get(&(session.id, retained_turn_count))
            .copied()
            .unwrap_or(false);
        let workspace = MessageEditWorkspace::for_checkpoint(turn.checkpoint.as_ref(), has_ref)?;
        let rollback_turns = session.provider_turns_after(retained_turn_count);
        if rollback_turns > 0 && session.provider_cursor.is_none() {
            return None;
        }
        Some(UserMessageAction {
            session_id: session.id,
            message_id: message.id,
            turn_count: turn.turn_count,
            workspace,
        })
    }

    /// Forget cached checkpoint-ref existence after refs changed. The next
    /// transcript frame schedules a fresh background prefetch.
    pub(super) fn invalidate_checkpoint_refs(&self) {
        self.checkpoint_ref_cache.borrow_mut().clear();
        self.checkpoint_ref_generation
            .set(self.checkpoint_ref_generation.get().wrapping_add(1));
    }

    /// Resolve the selected session's checkpoint refs on the background
    /// executor — one `git for-each-ref` per session per invalidation — and
    /// cache which retained turn counts have one. The rewind affordance
    /// appears once the result lands and notifies.
    fn prefetch_checkpoint_refs(&self, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let generation = self.checkpoint_ref_generation.get();
        if self.checkpoint_ref_prefetch.get() == Some((session.id, generation)) {
            return;
        }
        let Some(project_path) = self
            .workspace_path_for_session(session)
            .map(std::path::Path::to_path_buf)
        else {
            return;
        };
        let session_id = session.id;
        let retained_turn_counts = session
            .turns
            .iter()
            .map(|turn| turn.turn_count.saturating_sub(1))
            .collect::<Vec<_>>();
        self.checkpoint_ref_prefetch
            .set(Some((session_id, generation)));
        let workspace = raku_client::WorkspaceClient::new(self.daemon.client());
        cx.spawn(async move |this, cx| {
            let existing = cx
                .background_executor()
                .spawn(async move {
                    match workspace.request(raku_client::WorkspaceOperation::SessionTurnRefs {
                        cwd: project_path,
                        session_id,
                    }) {
                        Ok(raku_client::WorkspaceResult::TurnRefs { turn_counts }) => {
                            turn_counts.into_iter().collect::<HashSet<_>>()
                        }
                        Ok(_) | Err(_) => HashSet::new(),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.checkpoint_ref_generation.get() != generation {
                    return;
                }
                let mut cache = this.checkpoint_ref_cache.borrow_mut();
                for turn_count in retained_turn_counts {
                    cache.insert((session_id, turn_count), existing.contains(&turn_count));
                }
                drop(cache);
                cx.notify();
            });
        })
        .detach();
    }

    /// The rewind target that retries `message_index`'s failed latest turn,
    /// available exactly where Edit & resend could rewind its opening prompt.
    pub(super) fn retry_action_for_message(
        &self,
        message_index: usize,
    ) -> Option<UserMessageAction> {
        let opening_index = failed_latest_turn_opening(self.selected_session()?, message_index)?;
        self.user_message_action_for_message(opening_index)
    }

    pub(super) fn assistant_message_action_for_message(
        &self,
        message_index: usize,
    ) -> Option<AssistantMessageAction> {
        let session = self.selected_session()?;
        let message = session.messages.get(message_index)?;
        if message.role != MessageRole::Assistant
            || assistant_response_footer_index(session, message_index) != Some(message_index)
            || !matches!(session.status, SessionStatus::Idle | SessionStatus::Failed)
            || !session.provider.capabilities().conversation.fork
            || session
                .provider_cursor
                .as_ref()
                .is_none_or(|cursor| cursor.provider() != session.provider)
        {
            return None;
        }
        let turn_id = message.turn_id?;
        let turn = session
            .turns
            .iter()
            .find(|turn| turn.id == turn_id && turn.provider_turn_started)?;
        let pending_turn = self.response_fork_preparations.get(&session.id).copied();
        Some(AssistantMessageAction {
            session_id: session.id,
            turn_count: turn.turn_count,
            enabled: pending_turn.is_none(),
            preparing: pending_turn == Some(turn.turn_count),
        })
    }

    /// `metrics` rescaled to the transcript and code font size settings.
    pub(super) fn transcript_markdown_metrics(&self, metrics: MarkdownMetrics) -> MarkdownMetrics {
        metrics.scaled(
            self.state.transcript_font_size.px(),
            self.state.code_font_size,
        )
    }

    /// `metrics` for markdown outside the transcript (toasts, skill pages):
    /// prose stays at the fixed chrome size, code follows its setting.
    pub(super) fn chrome_markdown_metrics(&self, metrics: MarkdownMetrics) -> MarkdownMetrics {
        metrics.scaled(
            raku_client::persistence::DEFAULT_UI_FONT_SIZE,
            self.state.code_font_size,
        )
    }

    /// Mono text scale for plain tool-output sections: the code font size
    /// itself, the way every dedicated code surface takes it.
    fn activity_mono_text(&self) -> (f32, f32) {
        let size = self.state.code_font_size;
        (size, (size * 1.5 * 2.0).round() / 2.0)
    }

    /// The markdown render context for one transcript row. Element keys are
    /// scoped to the row, so a virtualized remount recreates the same keys and
    /// an in-progress selection survives scrolling.
    fn markdown_ctx<'a>(
        &self,
        row: String,
        palette: &'a MarkdownPalette,
        metrics: MarkdownMetrics,
        animate_streaming: bool,
    ) -> MarkdownCtx<'a> {
        MarkdownCtx::new(row, palette, metrics, self.transcript_selection.clone())
            .with_font_family(self.fonts.transcript.clone())
            .with_math_enabled(self.state.render_math)
            .with_link_handler(self.markdown_link_handler.clone())
            .with_image_renderer(self.markdown_image_renderer.clone())
            .with_file_links(self.markdown_file_links.clone())
            .with_streaming_animation(animate_streaming)
    }

    /// The menu handle for `id`, created on first use.
    ///
    /// Every menu holds the composer's *visual* focus while it is open, so
    /// opening one never looks like it defocused the input — the composer owns
    /// real focus almost all the time, and the menu has to take it to see keys.
    pub(super) fn menu_handle(
        &self,
        id: impl Into<SharedString>,
        cx: &mut App,
    ) -> ContextMenuHandle {
        self.menu_handle_with(id, cx, |_, _, _| {})
    }

    /// [`Self::menu_handle`] with an extra toggle observer, run after the
    /// composer's. `extra` is only consulted the first time a given id is seen.
    pub(super) fn menu_handle_with(
        &self,
        id: impl Into<SharedString>,
        cx: &mut App,
        extra: impl Fn(bool, &mut Window, &mut App) + 'static,
    ) -> ContextMenuHandle {
        let id = id.into();
        if let Some(handle) = self.menus.borrow().get(&id) {
            return handle.clone();
        }
        let composer = self.composer.clone();
        let handle = ContextMenuHandle::new(cx)
            .on_toggle(move |open, window, cx| {
                composer.update(cx, |composer, cx| {
                    if open {
                        composer.preserve_visual_focus_for_context_menu(window, cx);
                    } else {
                        composer.release_visual_focus_for_context_menu(window, cx);
                    }
                });
            })
            .on_toggle(extra);
        self.menus.borrow_mut().insert(id, handle.clone());
        handle
    }

    pub(super) fn transcript_row(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let raku = cx.entity().downgrade();
        // Both from the cache `sync_transcript_rows` refreshed at the top of
        // this frame. Recomputing the row list here would rebuild the whole
        // transcript's row kinds — several allocations proportional to the
        // session — once for every visible row, every frame.
        let (row_count, kind) = {
            let kinds = self.transcript_row_kinds.borrow();
            let kind = kinds
                .get(index)
                .copied()
                .unwrap_or(TranscriptRowKind::Message(index));
            (kinds.len(), kind)
        };
        let palette = MarkdownPalette::from_theme(&theme);
        let response_turn_id = self
            .selected_session()
            .and_then(|session| response_row_turn_id(session, kind));
        let starts_followup_turn = match kind {
            TranscriptRowKind::Message(message_index) => {
                self.selected_session().is_some_and(|session| {
                    message_starts_followup_turn(&session.messages, message_index)
                })
            }
            TranscriptRowKind::TurnBlock(_)
            | TranscriptRowKind::TurnFold(_)
            | TranscriptRowKind::ResponseFooter(_, _)
            | TranscriptRowKind::ChangedFiles(_)
            | TranscriptRowKind::WorkingIndicator => false,
        };
        // Providers separate consecutive tool calls with whitespace-only text
        // blocks (Claude Code emits `"\n\n"`). Such a message has nothing to
        // show, but its body and padding would otherwise open a large empty
        // band between the activity groups on either side of it.
        let blank_assistant = matches!(
            kind,
            TranscriptRowKind::Message(message_index)
                if self.selected_session().is_some_and(|session| {
                    session.messages.get(message_index).is_some_and(|message| {
                        message.role == MessageRole::Assistant
                            && message.visible_content().trim().is_empty()
                            && message.attachments.is_empty()
                    })
                })
        );
        let inner = match kind {
            TranscriptRowKind::Message(_) if blank_assistant => div().into_any_element(),
            TranscriptRowKind::Message(message_index) => self
                .selected_session()
                .and_then(|session| session.messages.get(message_index))
                .cloned()
                .map(|message| {
                    let copied = self.copied_message_feedback.contains_key(&message.id);
                    let assistant_footer = self.assistant_response_footer_cached(message_index);
                    let assistant_message_action =
                        self.assistant_message_action_for_message(message_index);
                    let user_message_action = self.user_message_action_for_message(message_index);
                    let message_edit = user_message_action.and_then(|action| {
                        self.message_edit
                            .as_ref()
                            .filter(|edit| {
                                edit.session_id == action.session_id
                                    && edit.message_id == action.message_id
                            })
                            .map(|edit| MessageEditRender {
                                input: edit.input.clone(),
                                cancel_focus: self.transcript_control_focus(
                                    format!("cancel-message-edit-{}", message.id),
                                    cx,
                                ),
                                submit_focus: self.transcript_control_focus(
                                    format!("submit-message-edit-{}", message.id),
                                    cx,
                                ),
                            })
                    });
                    let user_message_control =
                        user_message_action.map(|action| UserMessageControl {
                            action,
                            focus: self.transcript_control_focus(
                                format!("user-message-action-{}", message.id),
                                cx,
                            ),
                        });
                    let attachment_menus = (0..message.attachments.len())
                        .map(|index| {
                            self.menu_handle(
                                format!("message-{}-attachment-{index}", message.id),
                                cx,
                            )
                        })
                        .collect();
                    let attachment_images = message
                        .attachments
                        .iter()
                        .map(|attachment| {
                            if !attachment.is_image {
                                return None;
                            }
                            let Some(reference) = attachment.blob_reference.as_deref() else {
                                return None;
                            };
                            self.image_for_reference(
                                reference,
                                Some(&attachment.path),
                                Some(&attachment.name),
                                cx,
                            )
                        })
                        .collect();
                    let attachments_can_reveal = true;
                    let menu = self.menu_handle(format!("message-{}", message.id), cx);
                    let user_message_viewport = (message.role == MessageRole::User).then(|| {
                        self.user_message_viewports
                            .borrow_mut()
                            .entry(message.id)
                            .or_default()
                            .clone()
                    });
                    let metrics =
                        self.transcript_markdown_metrics(if message.role == MessageRole::User {
                            MarkdownMetrics::USER_MESSAGE
                        } else {
                            MarkdownMetrics::BODY
                        });
                    let animate_streaming = message.streaming && !cx.reduce_motion();
                    let mut ctx = self
                        .markdown_ctx(
                            format!("message-{}", message.id),
                            &palette,
                            metrics,
                            animate_streaming,
                        )
                        .with_context_menu(menu.clone());
                    // A settled reply's `sh run` fences can be run by the user.
                    if message.role == MessageRole::Assistant && !message.streaming {
                        ctx = ctx.with_code_run(self.shell_runs.hooks(message.id));
                    }
                    if let Some(highlights) = self.transcript_search_highlights(message_index) {
                        ctx = ctx.with_search_highlights(highlights);
                    }
                    // Quotes that started a side chat: dashed and clickable,
                    // once the message has settled into its final layout.
                    if !message.streaming {
                        ctx = ctx.with_inline_anchors(self.side_chat_anchors_for(message.id));
                    }
                    // Human and assistant messages share the Markdown path.
                    // Parse only visible rows rather than doing work for every
                    // driver delta or every off-screen prompt.
                    let mut markdown = self.message_markdown.borrow_mut();
                    let view = matches!(message.role, MessageRole::User | MessageRole::Assistant)
                        .then(|| {
                            let view = markdown.entry(message.id).or_default();
                            // A reply's closing `follow-up` fence is a card,
                            // never code in the transcript.
                            let text = if message.role == MessageRole::Assistant {
                                super::follow_up::split_follow_up(message.visible_content()).0
                            } else {
                                message.visible_content()
                            };
                            view.set_text(text, message.streaming);
                            &*view
                        });
                    let rendered = render_message(
                        MessageRender {
                            theme: &theme,
                            message: &message,
                            assistant_footer_copy_content: assistant_footer.copy_content,
                            assistant_footer_time: assistant_footer.time,
                            copied,
                            assistant_message_action,
                            user_message_control,
                            user_message_viewport: user_message_viewport.as_ref(),
                            message_edit,
                            attachment_menus,
                            attachment_images,
                            attachments_can_reveal,
                            markdown: view,
                            ctx: &ctx,
                            menu,
                            raku,
                            error_expanded: self.expanded_error_details.contains(&message.id),
                            error_focus: self.transcript_control_focus(
                                format!("message-error-details-{}", message.id),
                                cx,
                            ),
                        },
                        window,
                        cx,
                    );
                    if animate_streaming && view.is_some_and(MarkdownView::is_fading) {
                        // Advance the dissolve from the shared pulse clock,
                        // not `request_animation_frame`: chunks land every
                        // stream commit, so a fade is active for essentially
                        // the whole response and a display-rate re-arm held
                        // the window at 120 Hz — and every one of those
                        // frames rebuilds each visible row. ~30 fps across a
                        // 120-400 ms dissolve is visually equivalent at a
                        // quarter of the redraws, the same trade the loaders
                        // make, and the lease parks once the last chunk
                        // settles. Leasing `current_view` (the transcript
                        // pane) keeps the tick from busting sibling islands.
                        motion::pulse_lease(window.current_view(), cx);
                    }
                    rendered
                })
                .unwrap_or_else(|| div().into_any_element()),
            TranscriptRowKind::TurnBlock(block_index) => self
                .selected_transcript_blocks()
                .get(block_index)
                .map(|block| {
                    self.render_activities_row(&block.activities, block_index, &theme, window, cx)
                })
                .unwrap_or_else(|| div().into_any_element()),
            TranscriptRowKind::TurnFold(turn_id) => self.render_turn_fold_row(turn_id, &theme, cx),
            TranscriptRowKind::ResponseFooter(turn_id, message_index) => {
                self.render_response_footer_row(turn_id, message_index, &theme, window, cx)
            }
            TranscriptRowKind::ChangedFiles(turn_id) => self
                .render_changed_files_row(turn_id, &theme, cx)
                .unwrap_or_else(|| div().into_any_element()),
            TranscriptRowKind::WorkingIndicator => self.render_working_indicator_row(&theme, cx),
        };
        let row = div()
            .id(("transcript-row", index))
            .w_full()
            .flex()
            .justify_center()
            .px(px(20.0));
        // Vertical spacing lives inside the hover target so a response's rows
        // tile without gaps: crossing from the body to its footer must not
        // hide the footer for a frame. Hover still tracks the centered
        // content column, not the full-width row, so the gutters beside a
        // response do not reveal its footer.
        let mut content = div()
            .id(("transcript-row-content", index))
            .w_full()
            .max_w(px(CONTENT_MAX_WIDTH))
            .min_w_0()
            .py(px(8.0))
            .when(blank_assistant, |element| element.py(px(0.0)))
            .when(
                matches!(kind, TranscriptRowKind::ResponseFooter(_, _)),
                |element| element.pt(px(0.0)),
            )
            .when(index == 0 && !blank_assistant, |element| {
                element.pt(px(22.0))
            })
            .when(starts_followup_turn, |element| {
                element.pt(px(FOLLOWUP_TURN_TOP_GAP))
            })
            .when(index + 1 == row_count && !blank_assistant, |element| {
                element.pb(px(22.0))
            })
            .child(inner);
        if let Some(turn_id) = response_turn_id {
            let hover_target = (turn_id, kind);
            content = content.on_hover(cx.listener(move |this, hovering: &bool, _, cx| {
                let previous = this.hovered_response_row;
                if *hovering {
                    this.hovered_response_row = Some(hover_target);
                } else if previous == Some(hover_target) {
                    this.hovered_response_row = None;
                }
                if this.hovered_response_row != previous {
                    cx.notify();
                }
            }));
        }
        row.child(content).into_any_element()
    }

    fn render_response_footer_row(
        &self,
        turn_id: Uuid,
        message_index: usize,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(message) = self
            .selected_session()
            .and_then(|session| session.messages.get(message_index))
            .cloned()
        else {
            return div().into_any_element();
        };
        let footer = self.assistant_response_footer_cached(message_index);
        let Some(copy_content) = footer.copy_content else {
            return div().into_any_element();
        };
        let copied = self.copied_message_feedback.contains_key(&message.id);
        let action = self.assistant_message_action_for_message(message_index);
        let retry = self.retry_action_for_message(message_index);
        let controls = MessageFooterControls {
            copy: self.transcript_control_focus(format!("copy-message-{}", message.id), cx),
            fork: self.transcript_control_focus(format!("fork-response-{}", message.id), cx),
            retry: self.transcript_control_focus(format!("retry-turn-{}", message.id), cx),
        };
        // A failed turn's next step is to retry it, so that footer stays
        // visible instead of waiting for hover.
        let force_visible = retry.is_some()
            || controls.copy.is_focused(window)
            || controls.fork.is_focused(window)
            || controls.retry.is_focused(window)
            || self
                .hovered_response_row
                .is_some_and(|(hovered_turn_id, _)| hovered_turn_id == turn_id);
        let group_name = SharedString::from(format!("assistant-response-footer-{turn_id}"));
        let mut column = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .group(group_name.clone());
        // The decision belongs to the reply's text, so it comes before the
        // turn's file stats and the footer actions.
        if let Some(card) = self.render_follow_up(message_index, theme, cx) {
            column = column.child(card);
        }
        if let Some(changed_files) = self.render_changed_files_row(turn_id, theme, cx) {
            column = column.child(div().w_full().mb(px(3.0)).child(changed_files));
        }
        column
            .child(render_message_footer(
                theme,
                &message,
                footer.time.unwrap_or(message.created_at),
                copy_content,
                copied,
                group_name,
                force_visible,
                false,
                action,
                retry,
                None,
                footer.model_label,
                Some(controls),
                cx.entity().downgrade(),
            ))
            .into_any_element()
    }

    pub(super) fn toggle_changed_files(
        &mut self,
        turn_id: Uuid,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        self.pin_transcript_for_disclosure();
        if expanded {
            self.expanded_changed_files.remove(&turn_id);
        } else {
            self.expanded_changed_files.insert(turn_id);
        }
        self.remeasure_changed_files(turn_id);
        cx.notify();
    }

    /// The immutable file delta captured when a response settles. Small
    /// summaries stay useful at a glance; larger ones disclose in place and
    /// always offer the complete per-turn list in the right panel.
    fn render_changed_files_row(
        &self,
        turn_id: Uuid,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let Some(checkpoint) = self
            .selected_session()
            .and_then(|session| session.turns.iter().find(|turn| turn.id == turn_id))
            .and_then(|turn| turn.checkpoint.as_ref())
            .filter(|checkpoint| checkpoint.status == CheckpointStatus::Ready)
            .filter(|checkpoint| !checkpoint.files.is_empty())
        else {
            return None;
        };

        let files = checkpoint.files.as_slice();
        let additions = checkpoint.additions;
        let deletions = checkpoint.deletions;
        let expanded = self.expanded_changed_files.contains(&turn_id);
        let visible_limit = if expanded {
            CHANGED_FILES_EXPANDED_LIMIT
        } else {
            CHANGED_FILES_PREVIEW_LIMIT
        };
        let visible_count = files.len().min(visible_limit);
        let can_expand = files.len() > CHANGED_FILES_PREVIEW_LIMIT;
        let clipped = expanded && files.len() > CHANGED_FILES_EXPANDED_LIMIT;

        let review_focus =
            self.transcript_control_focus(format!("changed-files-review-{turn_id}"), cx);
        let review = div()
            .id(SharedString::from(format!(
                "changed-files-review-{turn_id}"
            )))
            .track_focus(&review_focus)
            .tab_index(0)
            .h(px(28.0))
            .px(px(10.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .items_center()
            .gap(px(5.0))
            .cursor_default()
            .text_size(sp(SMALL_UI_FONT_SIZE))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.text_secondary)
            .focus_visible(|style| style.border_color(theme.accent))
            .hover(|style| style.bg(theme.overlay_strong).text_color(theme.text))
            .active(|style| style.bg(theme.overlay))
            .child(icon("icons/file-diff.svg", 12.0, theme.text_tertiary))
            .child(tr_cow!("transcript.review_changes"))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_turn_diff(turn_id, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.open_turn_diff(turn_id, cx);
                    cx.stop_propagation();
                }
            }));

        let title = if files.len() == 1 {
            tr!("transcript.changed_file", count = files.len())
        } else {
            tr!("transcript.changed_files", count = files.len())
        };
        let mut card = div()
            .id(SharedString::from(format!("changed-files-card-{turn_id}")))
            .w_full()
            .min_w_0()
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border_strong)
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
                            .child(icon("icons/file-diff.svg", 16.0, theme.text_tertiary)),
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
                                    .text_size(sp(SMALL_UI_FONT_SIZE))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .text_size(sp(SMALL_UI_FONT_SIZE))
                                    .line_height(sp(14.0))
                                    .child(
                                        div()
                                            .text_color(theme.success)
                                            .child(format!("+{additions}")),
                                    )
                                    .child(
                                        div()
                                            .text_color(theme.danger)
                                            .child(format!("-{deletions}")),
                                    ),
                            ),
                    )
                    .child(review),
            );

        // The card is an outline on the transcript surface. `overflow_hidden`
        // does not clip child paint to its rounded corners, so the last row's
        // hover fill carries its own bottom radius (12px less the 1px border).
        let mut file_rows = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme.border);
        let row_count = visible_count + usize::from(can_expand);
        for (index, file) in files.iter().take(visible_count).enumerate() {
            let last = index + 1 == row_count;
            let row_focus =
                self.transcript_control_focus(format!("changed-file-{turn_id}-{}", file.path), cx);
            let path = file.path.clone();
            let key_path = file.path.clone();
            file_rows = file_rows.child(
                div()
                    .id(SharedString::from(format!(
                        "changed-file-{turn_id}-{}",
                        file.path
                    )))
                    .track_focus(&row_focus)
                    .tab_index(0)
                    .h(px(31.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_default()
                    .when(last, |row| row.rounded_b(px(11.0)))
                    .hover(|style| style.bg(theme.overlay))
                    .active(|style| style.bg(theme.overlay_strong))
                    .focus_visible(|style| style.bg(theme.overlay_strong))
                    .tooltip(Tooltip::text(file.path.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_turn_diff_file(turn_id, path.clone(), cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.open_turn_diff_file(turn_id, key_path.clone(), cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(sp(SMALL_UI_FONT_SIZE))
                            .text_color(theme.text_secondary)
                            .child(file.path.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(sp(SMALL_UI_FONT_SIZE))
                            .text_color(theme.success)
                            .child(format!("+{}", file.additions)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(sp(SMALL_UI_FONT_SIZE))
                            .text_color(theme.danger)
                            .child(format!("-{}", file.deletions)),
                    ),
            );
        }
        if can_expand {
            let toggle_focus =
                self.transcript_control_focus(format!("changed-files-toggle-{turn_id}"), cx);
            let label = if expanded {
                tr!("transcript.show_fewer_files")
            } else {
                tr!(
                    "transcript.show_more_files",
                    count = files.len() - CHANGED_FILES_PREVIEW_LIMIT
                )
            };
            file_rows = file_rows.child(
                div()
                    .id(SharedString::from(format!(
                        "changed-files-toggle-{turn_id}"
                    )))
                    .rounded_b(px(11.0))
                    .track_focus(&toggle_focus)
                    .tab_index(0)
                    .h(px(34.0))
                    .px(px(12.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_default()
                    .text_size(sp(SMALL_UI_FONT_SIZE))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text_secondary)
                    .focus_visible(|style| style.bg(theme.overlay_strong))
                    .hover(|style| style.bg(theme.overlay_strong).text_color(theme.text))
                    .active(|style| style.bg(theme.overlay))
                    .child(SharedString::from(label))
                    .when(clipped, |row| {
                        row.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_weight(FontWeight::NORMAL)
                                .text_color(theme.text_ghost)
                                .child(tr!(
                                    "transcript.showing_first_files",
                                    count = CHANGED_FILES_EXPANDED_LIMIT,
                                    total = files.len()
                                )),
                        )
                    })
                    .child(div().flex_1())
                    .child(icon(
                        if expanded {
                            "icons/chevron-down.svg"
                        } else {
                            "icons/chevron-right.svg"
                        },
                        11.0,
                        theme.text_tertiary,
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_changed_files(turn_id, expanded, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.toggle_changed_files(turn_id, expanded, cx);
                            cx.stop_propagation();
                        }
                    })),
            );
        }
        card = card.child(file_rows);

        Some(card.into_any_element())
    }

    /// Settled reasoning, tool activity, and interim assistant commentary are
    /// folded into a compact plain-text toggle while the terminal response stays visible.
    pub(super) fn render_turn_fold_row(
        &self,
        turn_id: Uuid,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.expanded_turns.contains(&turn_id);
        let focus = self.transcript_control_focus(format!("turn-fold-{turn_id}"), cx);
        let label = self
            .selected_session()
            .map(|session| turn_fold_label(session, turn_id))
            .unwrap_or_else(|| tr!("transcript.worked"));
        // Same voice as the activity summaries: prose size, secondary color.
        let metrics = self.transcript_markdown_metrics(MarkdownMetrics::BODY);
        let row_height = metrics.line_height.max(24.0);
        div()
            .w_full()
            .h(px(row_height))
            .flex()
            .items_center()
            .child(
                div()
                    .id(SharedString::from(format!("turn-fold-{turn_id}")))
                    .track_focus(&focus)
                    .tab_index(0)
                    .h(px(row_height))
                    // Padding gives the focus fill room; the negative margin
                    // keeps the glyphs on the body text's left edge.
                    .px(px(2.0))
                    .ml(px(-2.0))
                    .rounded(px(4.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .cursor_default()
                    .text_size(px(metrics.text_size))
                    .line_height(px(metrics.line_height))
                    .text_color(theme.text_secondary)
                    .focus_visible(|style| style.bg(theme.overlay).text_color(theme.text))
                    .hover(|style| style.text_color(theme.text))
                    .child(SharedString::from(label))
                    .child(icon(
                        if expanded {
                            "icons/chevron-down.svg"
                        } else {
                            "icons/chevron-right.svg"
                        },
                        (metrics.text_size * 0.7).round(),
                        theme.text_secondary,
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_turn_fold(turn_id, expanded, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        let key = event.keystroke.key.as_str();
                        if matches!(key, "enter" | "space")
                            || (key == "right" && !expanded)
                            || (key == "left" && expanded)
                        {
                            this.toggle_turn_fold(turn_id, expanded, cx);
                            cx.stop_propagation();
                        }
                    })),
            )
            .into_any_element()
    }

    /// The live turn's closing row: pulsing dots, what the agent is doing
    /// right now ("Running a command"), and the elapsed time. It is on screen
    /// from the moment the prompt lands — before the provider has produced a
    /// single chunk — and stays below whatever streams in until the turn
    /// settles into its "Worked for N" fold. While a live activity group sits
    /// directly above, the row also folds and unfolds that group.
    fn render_working_indicator_row(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let session = self.selected_session();
        let elapsed = session
            .and_then(|session| session.turns.last())
            .filter(|turn| turn.status == TurnStatus::Running)
            .map(|turn| unix_time().saturating_sub(turn.started_at))
            .unwrap_or(0);
        // A parked turn is waiting on detached work, not working.
        let parked = session.is_some_and(|session| session.status == SessionStatus::Background);
        let label = if parked {
            tr!("transcript.waiting_background")
        } else {
            session
                .and_then(live_activity_label)
                .unwrap_or_else(|| tr!("transcript.working"))
        };
        // The activity group the chevron controls: only the live one at the
        // tail, mirroring `render_activities_row`'s own liveness rule.
        let live_block = session.and_then(|session| {
            let block_index = session.transcript_blocks.len().checked_sub(1)?;
            let block = &session.transcript_blocks[block_index];
            (!block.activities.is_empty()
                && activity_group_is_live(
                    session
                        .active_turn_id()
                        .is_some_and(|turn_id| block.turn_id == Some(turn_id)),
                    true,
                    block.after_message,
                    session.messages.len(),
                ))
            .then_some(block_index)
        });
        // Same type as the activity summary above it, so the live label reads
        // as part of the same run of work.
        let metrics = self.transcript_markdown_metrics(MarkdownMetrics::BODY);
        let row_height = metrics.line_height.max(24.0);
        let content = div()
            .h(px(row_height))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(metrics.text_size))
            .line_height(px(metrics.line_height))
            .child(working_wave_dots(theme.text_tertiary))
            .child(
                div()
                    .text_color(theme.text_tertiary)
                    .child(SharedString::from(label)),
            )
            .when(!parked, |row| {
                row.child(
                    div()
                        .text_color(theme.text_tertiary)
                        .child(SharedString::from(format_working_elapsed(elapsed))),
                )
            });
        // The agent's task list sits right above the row that says what it
        // is doing; both exist exactly while the turn runs.
        let task_list = self.render_task_list(cx);
        let Some(block_index) = live_block else {
            return div()
                .w_full()
                .flex()
                .flex_col()
                .children(task_list)
                .child(
                    div()
                        .w_full()
                        .h(px(row_height))
                        .flex()
                        .items_center()
                        .child(content),
                )
                .into_any_element();
        };
        let expanded = self
            .activities_expanded
            .get(&block_index)
            .copied()
            .unwrap_or(false);
        let focus = self.transcript_control_focus("working-indicator".to_string(), cx);
        div()
            .w_full()
            .flex()
            .flex_col()
            .children(task_list)
            .child(
                div()
                    .w_full()
                    .h(px(row_height))
                    .flex()
                    .items_center()
                    .child(
                div()
                    .id("working-indicator")
                    .track_focus(&focus)
                    .tab_index(0)
                    .h(px(row_height))
                    // Padding gives the focus fill room; the negative margin
                    // keeps the spinner on the body text's left edge.
                    .px(px(2.0))
                    .ml(px(-2.0))
                    .rounded(px(4.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_default()
                    .focus_visible(|style| style.bg(theme.overlay))
                    .child(content)
                    .child(icon(
                        if expanded {
                            "icons/chevron-down.svg"
                        } else {
                            "icons/chevron-right.svg"
                        },
                        (metrics.text_size * 0.7).round(),
                        theme.text_tertiary,
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_activities(block_index, expanded, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        let key = event.keystroke.key.as_str();
                        if matches!(key, "enter" | "space")
                            || (key == "right" && !expanded)
                            || (key == "left" && expanded)
                        {
                            this.toggle_activities(block_index, expanded, cx);
                            cx.stop_propagation();
                        }
                    })),
                    ),
            )
            .into_any_element()
    }

    /// The turn's tool activity as a disclosure: the summary line toggles the
    /// row list, and each row with detail expands to its full content.
    fn show_activity_section_copied(
        &mut self,
        activity_id: Uuid,
        section_kind: ActivityDisclosureSectionKind,
        cx: &mut Context<Self>,
    ) {
        self.copied_activity_generation = self.copied_activity_generation.wrapping_add(1);
        let generation = self.copied_activity_generation;
        let key = (activity_id, section_kind);
        self.copied_activity_feedback.insert(key, generation);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |this, cx| {
                if this.copied_activity_feedback.get(&key) == Some(&generation) {
                    this.copied_activity_feedback.remove(&key);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Copy for a section that has no header row of its own.
    ///
    /// It floats over the block's top-right corner and stays quiet until the
    /// pointer arrives or the keyboard reaches it, so a command card reads as
    /// a command rather than as a form. Opacity — not visibility — hides it,
    /// which keeps the control in the tab order while it is invisible.
    #[allow(clippy::too_many_arguments)]
    fn render_activity_section_copy(
        &self,
        id: Uuid,
        section_kind: ActivityDisclosureSectionKind,
        content: impl Into<SharedString>,
        group: SharedString,
        surface: Hsla,
        top: f32,
        right: f32,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = *theme;
        let content: SharedString = content.into();
        let copied = self
            .copied_activity_feedback
            .contains_key(&(id, section_kind));
        let focus = self
            .transcript_control_focus(format!("copy-activity-{}-{}", id, section_kind.id()), cx);
        let revealed = focus.is_focused(window);
        let name = section_kind.label().unwrap_or_default().to_lowercase();
        let tooltip = SharedString::from(if copied {
            tr!("common.copied")
        } else {
            tr!("common.copy_named", name = name)
        });
        let click_content = content.clone();
        let click_raku = cx.entity().downgrade();
        let key_content = content;
        let key_raku = click_raku.clone();
        div()
            .id(SharedString::from(format!(
                "copy-activity-{}-{}",
                id,
                section_kind.id()
            )))
            .track_focus(&focus)
            .tab_index(0)
            .absolute()
            .top(px(top))
            .right(px(right))
            .size(px(20.0))
            .rounded(px(5.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .bg(surface)
            .focus_visible(|button| button.bg(theme.overlay_strong))
            .hover(|button| button.bg(theme.overlay_strong))
            .when(!revealed, |button| {
                button
                    .opacity(0.0)
                    .group_hover(group, |button| button.opacity(1.0))
            })
            .child(icon(
                if copied {
                    "icons/check.svg"
                } else {
                    "icons/copy.svg"
                },
                11.0,
                theme.text_ghost,
            ))
            .tooltip(Tooltip::text(tooltip))
            .on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(click_content.to_string()));
                let _ = click_raku.update(cx, |this, cx| {
                    this.show_activity_section_copied(id, section_kind, cx);
                });
            })
            .on_key_down(move |event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(key_content.to_string()));
                    let _ = key_raku.update(cx, |this, cx| {
                        this.show_activity_section_copied(id, section_kind, cx);
                    });
                    cx.stop_propagation();
                }
            })
            .into_any_element()
    }

    pub(super) fn render_activities_row(
        &self,
        activities: &[ActivityItem],
        block_index: usize,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A completed child is not a group boundary: providers commonly emit
        // the next tool after the previous result. The group leaves the live
        // tail only when answer text is appended (so `after_message` falls
        // behind) or when the turn itself settles.
        let live_group = self.selected_session().is_some_and(|session| {
            session
                .transcript_blocks
                .get(block_index)
                .is_some_and(|block| {
                    activity_group_is_live(
                        session
                            .active_turn_id()
                            .is_some_and(|turn_id| block.turn_id == Some(turn_id)),
                        block_index + 1 == session.transcript_blocks.len(),
                        block.after_message,
                        session.messages.len(),
                    )
                })
        });
        let live_reasoning_id = (self
            .selected_session()
            .is_some_and(|session| session.status == SessionStatus::Working)
            && block_index + 1 == self.selected_transcript_blocks().len())
        .then(|| {
            activities
                .iter()
                .rev()
                .find(|activity| activity.reasoning.is_some() && !activity.complete)
                .map(|activity| activity.id)
        })
        .flatten();
        self.render_activity_cluster(
            activities,
            block_index,
            ActivityRenderContext {
                live_group,
                part: ActivityClusterPart::All,
                live_reasoning_id,
                expanded: self
                    .activities_expanded
                    .get(&block_index)
                    .copied()
                    .unwrap_or(false),
                expanded_items: &self.expanded_activity_items,
                markdown: &self.activity_markdown,
                window_starts: &self.reasoning_window_starts,
                viewports: &self.activity_scroll_viewports,
            },
            theme,
            window,
            cx,
        )
    }

    fn render_compaction_row(
        &self,
        activity: &ActivityItem,
        surface: &ActivityRenderContext<'_>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use raku_protocol::compaction::{CompactionStatus, CompactionTrigger};
        let item = activity.compaction.as_ref().expect("compaction row");
        if surface.part == ActivityClusterPart::Item {
            return div().into_any_element();
        }
        let id = activity.id;
        let expanded = surface.expanded_items.get(&id).copied().unwrap_or(false);
        let has_detail = item.summary.as_ref().is_some_and(|text| !text.is_empty())
            || item.error.is_some()
            || item.notice.is_some();
        let color = if item.status == CompactionStatus::Failed {
            theme.danger
        } else {
            theme.text_secondary
        };
        let title = tr!(match item.status {
            CompactionStatus::Running => "activity.compacting_context",
            CompactionStatus::Completed => "activity.compacted_context",
            CompactionStatus::Unconfirmed => "compaction.unconfirmed",
            CompactionStatus::Failed => "activity.compaction_failed",
            CompactionStatus::Interrupted => "compaction.interrupted",
        });
        let mut metadata = Vec::new();
        if let Some(trigger) = item.trigger {
            metadata.push(tr!(match trigger {
                CompactionTrigger::Auto => "compaction.auto",
                CompactionTrigger::Manual => "compaction.manual",
            }));
        }
        if let (Some(before), Some(after)) = (item.before_tokens, item.after_tokens) {
            metadata.push(format!(
                "{} → {}{}",
                crate::usage::format_tokens(before),
                if item.after_tokens_estimated {
                    "≈"
                } else {
                    ""
                },
                crate::usage::format_tokens(after)
            ));
        }
        if item.started_at_ms > 0
            && let Some(end) = item.completed_at_ms
        {
            metadata.push(format!(
                "{:.1}s",
                end.saturating_sub(item.started_at_ms) as f64 / 1000.0
            ));
        }
        let focus = self.transcript_control_focus(format!("compaction-{id}"), cx);
        let status_icon = match item.status {
            CompactionStatus::Running => {
                crate::ui::motion::spin_slow(icon("icons/loader-circle.svg", 13.0, color))
            }
            status => icon(
                match status {
                    CompactionStatus::Failed => "icons/x.svg",
                    CompactionStatus::Completed => "icons/check.svg",
                    CompactionStatus::Interrupted => "icons/stop.svg",
                    _ => "icons/info.svg",
                },
                13.0,
                color,
            )
            .into_any_element(),
        };
        let mut card = div()
            .w_full()
            .min_w_0()
            .my(px(8.0))
            .border_y_1()
            .border_color(theme.border)
            .py(px(6.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .id(SharedString::from(format!("compaction-toggle-{id}")))
                    .track_focus(&focus)
                    .tab_index(0)
                    .tab_stop(has_detail)
                    .min_h(px(30.0))
                    .px(px(6.0))
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(sp(12.0))
                    .text_color(color)
                    .focus_visible(|style| style.bg(theme.overlay))
                    .when(has_detail, |row| row.hover(|style| style.bg(theme.overlay)))
                    .child(status_icon)
                    .child(div().min_w_0().truncate().child(title))
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.text_tertiary)
                            .child(metadata.join(" · ")),
                    )
                    .when(has_detail, |row| {
                        row.child(icon(
                            if expanded {
                                "icons/chevron-down.svg"
                            } else {
                                "icons/chevron-right.svg"
                            },
                            10.0,
                            theme.text_tertiary,
                        ))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if has_detail {
                            this.toggle_activity_item(id, expanded, cx);
                        }
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if has_detail && matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.toggle_activity_item(id, expanded, cx);
                            cx.stop_propagation();
                        }
                    })),
            );
        if expanded && has_detail {
            let palette = MarkdownPalette::from_theme(theme);
            let ctx = self.markdown_ctx(
                format!("compaction-summary-{id}"),
                &palette,
                self.transcript_markdown_metrics(MarkdownMetrics::COMPACT),
                false,
            );
            let mut views = surface.markdown.borrow_mut();
            let view = views.entry(id).or_default();
            let summary = item.summary_for_display();
            let content = item
                .error
                .as_deref()
                .or(item.notice.as_deref())
                .or(summary.as_deref())
                .unwrap_or_default();
            view.set_text(content, false);
            card = card.child(
                div()
                    .min_w_0()
                    .px(px(8.0))
                    .py(px(6.0))
                    .children(md::render::markdown(view, &ctx)),
            );
        }
        card.into_any_element()
    }

    pub(super) fn render_activity_cluster(
        &self,
        activities: &[ActivityItem],
        block_index: usize,
        surface: ActivityRenderContext<'_>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let [activity] = activities
            && activity.kind == ActivityKind::ImageGeneration
            && (!activity.image_urls.is_empty()
                || activity.tool_name.as_deref() == Some("image_generation"))
        {
            return self.render_image_generation_row(activity, &surface, theme, cx);
        }
        if let [activity] = activities
            && activity.compaction.is_some()
        {
            return self.render_compaction_row(activity, &surface, theme, cx);
        }
        if let [activity] = activities
            && activity.kind == ActivityKind::Notice
        {
            return render_notice_row(activity, &surface, theme);
        }
        let ActivityRenderContext {
            live_group,
            live_reasoning_id,
            expanded,
            ..
        } = surface;
        // The live group has no header of its own: the working row below
        // names what is running and folds this group, so a second "Running …"
        // line would only repeat it.
        let cluster = if surface.part == ActivityClusterPart::Item || live_group {
            div().w_full().min_w_0()
        } else {
            let group_key = format!("activity-toggle-{block_index}");
            let header_title = activity_header_title(activities, live_group, live_reasoning_id);
            let group_running = activities.iter().any(|activity| !activity.complete);
            if !live_group && group_running {
                motion::pulse_lease_slow(window.current_view(), cx);
            }
            let header_focus = self.transcript_control_focus(group_key.clone(), cx);
            // The summary reads as part of the answer, so it takes the
            // transcript's prose size and leans on color for its hierarchy.
            let header_metrics = self.transcript_markdown_metrics(MarkdownMetrics::BODY);
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .id(SharedString::from(group_key))
                        .track_focus(&header_focus)
                        .tab_index(0)
                        .max_w_full()
                        .min_w_0()
                        .h(px(28.0))
                        .px(px(2.0))
                        .ml(px(-2.0))
                        .rounded(px(5.0))
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .text_size(px(header_metrics.text_size))
                        .line_height(px(header_metrics.line_height))
                        .text_color(theme.text_secondary)
                        .cursor_default()
                        .focus_visible(|style| style.bg(theme.overlay).text_color(theme.text))
                        .hover(|style| style.text_color(theme.text))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(header_title)),
                        )
                        .child(icon(
                            if expanded {
                                "icons/chevron-down.svg"
                            } else {
                                "icons/chevron-right.svg"
                            },
                            (header_metrics.text_size * 0.7).round(),
                            theme.text_secondary,
                        ))
                        .when(!live_group && group_running, |header| {
                            // The label already says "Running …"; the dot
                            // only adds motion to it.
                            header.child(pulse_dot(5.0, theme.accent))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_activities(block_index, expanded, cx);
                        }))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                this.toggle_activities(block_index, expanded, cx);
                                cx.stop_propagation();
                            }
                        })),
                )
        };
        if !expanded || surface.part == ActivityClusterPart::Header {
            return cluster.into_any_element();
        }
        // `Theme::overlay` is 5% alpha and GPUI's `opacity` multiplies it.
        let activity_hover_surface = theme.surface.blend(theme.overlay);
        let activity_active_surface = theme.surface.blend(theme.overlay_strong.opacity(0.72));
        let mut items = div()
            .min_w_0()
            .ml(px(6.0))
            .pl(px(12.0))
            .pb(px(2.0))
            .border_l_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap(px(2.0));
        for activity in activities {
            let id = activity.id;
            let background_work = self
                .state
                .selected_session
                .zip(activity.source_id.as_deref())
                .and_then(|(session_id, source_id)| {
                    self.background_work_for_activity(session_id, source_id)
                        .map(|item| (session_id, item.key.clone(), item.status))
                });
            let background_badge = background_work.map(|(session_id, key, status)| {
                let click_key = key.clone();
                let focus = self.transcript_control_focus(format!("activity-background-{id}"), cx);
                let color = work_status_color(status, *theme);
                div()
                    .id(SharedString::from(format!("activity-background-{id}")))
                    .track_focus(&focus)
                    .tab_index(0)
                    .h(px(20.0))
                    .px(px(6.0))
                    .rounded(px(5.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .flex_none()
                    .flex()
                    .items_center()
                    .cursor_default()
                    .text_size(sp(SMALL_UI_FONT_SIZE))
                    .text_color(color)
                    .focus_visible(|style| style.border_color(theme.accent))
                    .hover(|style| style.bg(theme.overlay_strong))
                    .child(work_status_label(status))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.open_background_work_surface(session_id, click_key.clone(), cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.open_background_work_surface(session_id, key.clone(), cx);
                            cx.stop_propagation();
                        }
                    }))
            });
            let reasoning = activity.reasoning.as_ref();
            let reasoning_live = live_reasoning_id == Some(id);
            // A read renders as the file it returned, so its raw arguments and
            // numbered output are never shown. Skipping the section build also
            // keeps a frame from cloning that output for nothing.
            let read_preview_available = reasoning.is_none()
                && activity.kind == ActivityKind::FileRead
                && !activity.failed
                && activity.image_urls.is_empty()
                && activity
                    .output
                    .as_deref()
                    .is_some_and(|output| !output.trim().is_empty());
            let sections = if reasoning.is_some() || read_preview_available {
                Vec::new()
            } else {
                activity_disclosure_sections(activity)
            };
            let preview = if reasoning.is_some() {
                String::new()
            } else {
                activity_preview(activity)
            };
            let action_label = activity_action_label(activity);
            let mut row_detail = activity_row_detail(activity, reasoning_live);
            let reasoning_duration = reasoning_row_duration(activity, reasoning_live);
            if row_detail.trim().is_empty() {
                row_detail = preview;
            }
            let has_row_detail = !row_detail.is_empty();
            let row_detail = SharedString::from(row_detail);
            let row_text = row_detail.clone();
            let monospace_detail = match activity.kind {
                ActivityKind::Command => {
                    activity.display_description.is_none() && activity.display_target.is_some()
                }
                ActivityKind::FileList | ActivityKind::FileSearch => {
                    activity.display_target.is_some()
                }
                _ => false,
            };
            // A read is named by the slice it returned, the way an editor's
            // status line names it, and reads as prose rather than as a path
            // pasted from a shell.
            let read_row = activity.kind == ActivityKind::FileRead;
            // Reads and edits name a file, so they read as a sentence — verb,
            // file, what it covered — with the disclosure right behind it.
            let packed_row = read_row || activity.kind == ActivityKind::FileChange;
            let read_range = read_row
                .then_some(activity.read_range)
                .flatten()
                .map(|range| range.label());
            let file_change_stats = activity_file_change_stats(activity);
            // One changed file is unambiguous, so the row itself can offer to
            // open it. A change touching several names each file in the diff
            // below, and each of those rows opens its own.
            let open_file_button = match activity.file_changes.as_slice() {
                [change] if activity.kind == ActivityKind::FileChange => {
                    Some(self.render_activity_open_file_button(
                        format!("activity-open-{id}"),
                        change.path.clone(),
                        theme,
                        cx,
                    ))
                }
                _ => None,
            };
            let shows_diff = reasoning.is_none() && activity_shows_diff(activity);
            let has_detail = reasoning
                .is_some_and(|reasoning| !reasoning.content.trim().is_empty())
                || read_preview_available
                || !sections.is_empty()
                || shows_diff
                || !activity.image_urls.is_empty();
            let item_expanded = has_detail
                && surface
                    .expanded_items
                    .get(&id)
                    .copied()
                    // Internal actions stay folded by default, even while
                    // their reasoning streams; the user opens them on demand.
                    .unwrap_or(false);
            let item_focus = self.transcript_control_focus(format!("activity-item-{id}"), cx);
            let item = div().w_full().min_w_0().flex().flex_col().child(
                div()
                    .id(SharedString::from(format!("activity-item-{id}")))
                    .h(px(30.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(5.0))
                    .text_size(sp(SMALL_UI_FONT_SIZE))
                    .line_height(sp(16.0))
                    .font_weight(FontWeight::NORMAL)
                    .when(has_detail, |element| {
                        element
                            .track_focus(&item_focus)
                            .tab_index(0)
                            .cursor_default()
                            .focus_visible(|element| element.bg(activity_hover_surface))
                            .hover(|element| element.bg(activity_hover_surface))
                            .active(|element| element.bg(activity_active_surface))
                    })
                    .when(!packed_row, |row| {
                        row.child(icon(
                            activity_icon(activity.kind),
                            12.0,
                            theme.text_tertiary,
                        ))
                    })
                    .child(
                        div()
                            .flex_none()
                            .when(!packed_row, |label| label.min_w(sp(44.0)))
                            .text_color(theme.text_secondary)
                            .child(SharedString::from(action_label)),
                    )
                    .when(has_row_detail, |element| {
                        element.child(
                            div()
                                .id(SharedString::from(format!("activity-target-{id}")))
                                .min_w_0()
                                .truncate()
                                .text_color(theme.text)
                                .when(packed_row, |detail| detail.flex_grow_0().flex_shrink_1())
                                .when(!packed_row, |detail| detail.flex_1())
                                .when(monospace_detail, |detail| {
                                    detail.font_family(md::render::MONO_FAMILY)
                                })
                                .tooltip(Tooltip::text(row_detail.clone()))
                                .child(row_detail),
                        )
                    })
                    .when(!has_row_detail, |row| row.child(div().flex_1()))
                    .children(read_range.map(|range| {
                        div()
                            .flex_none()
                            .text_color(theme.text_tertiary)
                            .child(SharedString::from(range))
                    }))
                    .when_some(file_change_stats, |row, (additions, deletions)| {
                        // A new file has nothing removed; `-0` would only be
                        // noise beside its count.
                        row.when(additions > 0 || deletions == 0, |row| {
                            row.child(
                                div()
                                    .flex_none()
                                    .text_color(theme.success)
                                    .child(SharedString::from(format!("+{additions}"))),
                            )
                        })
                        .when(deletions > 0, |row| {
                            row.child(
                                div()
                                    .flex_none()
                                    .text_color(theme.danger)
                                    .child(SharedString::from(format!("-{deletions}"))),
                            )
                        })
                    })
                    .when(has_detail && packed_row, |row| {
                        row.child(activity_chevron(item_expanded, theme))
                    })
                    // Packed rows keep their text at the start, so a spacer
                    // keeps any trailing status at the end of the row.
                    .when(packed_row && has_row_detail, |row| {
                        row.child(div().flex_1())
                    })
                    .when_some(reasoning_duration, |row, duration| {
                        row.child(
                            div()
                                .flex_none()
                                .text_color(theme.text_tertiary)
                                .child(SharedString::from(duration)),
                        )
                    })
                    .when(activity.failed, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .text_color(theme.danger)
                                .child(icon("icons/x.svg", 10.0, theme.danger))
                                .child(tr!("activity.status_failed")),
                        )
                    })
                    .when(
                        !activity.failed
                            && (reasoning_live || (reasoning.is_none() && !activity.complete)),
                        |row| {
                            row.child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.0))
                                    .text_color(theme.text_secondary)
                                    .child(pulse_dot(5.0, theme.accent))
                                    .child(tr!("activity.status_running")),
                            )
                        },
                    )
                    .children(background_badge)
                    .children(open_file_button)
                    .when(has_detail && !packed_row, |element| {
                        element.child(activity_chevron(item_expanded, theme))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if has_detail {
                            this.toggle_activity_item(id, item_expanded, cx);
                        }
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if has_detail && matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.toggle_activity_item(id, item_expanded, cx);
                            cx.stop_propagation();
                        }
                    })),
            );
            if !item_expanded {
                items = items.child(item);
                continue;
            }
            // Expanded detail adds no fill of its own, so thinking reads as
            // transcript prose. Still inset its children so diff bands and edge
            // fades stay inside GPUI's rectangular clipping.
            let mut detail = div()
                .min_w_0()
                .ml(px(28.0))
                .mr(px(8.0))
                .mt(px(2.0))
                .mb(px(4.0))
                .p(px(4.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(theme.border)
                .flex()
                .flex_col();
            if let Some(reasoning) = reasoning {
                // Reasoning remains model prose even though it now shares the
                // activity stream, so keep selectable markdown rather than
                // presenting it as monospace tool output.
                let mut palette = MarkdownPalette::from_theme(theme);
                palette.text = theme.text_secondary;
                palette.secondary = theme.text_tertiary;
                let ctx = self
                    .markdown_ctx(
                        format!("reasoning-{id}"),
                        &palette,
                        self.transcript_markdown_metrics(MarkdownMetrics::COMPACT),
                        reasoning_live && !cx.reduce_motion(),
                    )
                    .with_math_context_menu(self.menu_handle(format!("reasoning-math-{id}"), cx));
                let reasoning_viewport = surface
                    .viewports
                    .borrow_mut()
                    .entry(id)
                    .or_default()
                    .clone();
                let mut views = surface.markdown.borrow_mut();
                let view = views.entry(id).or_default();
                if reasoning_live {
                    let content = reasoning.content.trim_end();
                    let mut starts = surface.window_starts.borrow_mut();
                    let cached = starts.entry(id).or_insert(0);
                    let start = live_reasoning_window_anchor(*cached, content);
                    if start != *cached {
                        *cached = start;
                        *view = MarkdownView::seeded();
                    }
                    view.set_text(&content[start..], true);
                } else {
                    surface.window_starts.borrow_mut().remove(&id);
                    view.set_text(&reasoning.content, false);
                }
                let wheel_scroll = reasoning_viewport.scroll_handle.clone();
                let wheel_follow_tail = reasoning_viewport.follow_tail.clone();
                let markdown = if reasoning_live {
                    // The live peek pins to the tail of a growing document;
                    // building every block of a long think per pulse tick was
                    // the remaining 40%-CPU streaming path.
                    md::render::markdown_tail(view, &ctx, LIVE_REASONING_TAIL_BLOCKS)
                } else {
                    md::render::markdown(view, &ctx)
                };
                if reasoning_live && !cx.reduce_motion() && view.is_fading() {
                    // The reasoning dissolve rides the half-rate lease: fast
                    // thinking keeps a fade active for the whole phase, every
                    // tick rebuilds each visible transcript row, and 15 fps
                    // alpha on the dim compact peek is indistinguishable. The
                    // answer text keeps the full-rate dissolve.
                    motion::pulse_lease_slow(window.current_view(), cx);
                }
                detail = detail.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .relative()
                        .max_h(px(400.0))
                        .overflow_hidden()
                        .child(
                            div()
                                .id(SharedString::from(format!("reasoning-scroll-{id}")))
                                .w_full()
                                .min_w_0()
                                .max_h(px(400.0))
                                .overflow_y_scroll()
                                .track_scroll(&reasoning_viewport.scroll_handle)
                                .px(px(8.0))
                                .py(px(8.0))
                                .children(markdown)
                                .on_scroll_wheel(move |_, window, cx| {
                                    contain_scroll(&wheel_scroll, cx);
                                    let scroll = wheel_scroll.clone();
                                    let follow_tail = wheel_follow_tail.clone();
                                    window.defer(cx, move |_, _| {
                                        follow_tail.set(activity_scroll_at_bottom(&scroll));
                                    });
                                }),
                        )
                        .child(scrollbar::edge_fade(
                            reasoning_viewport.scroll_handle.clone(),
                            scrollbar::FadeEdge::Top,
                            theme.surface,
                        ))
                        .child(scrollbar::edge_fade(
                            reasoning_viewport.scroll_handle.clone(),
                            scrollbar::FadeEdge::Bottom,
                            theme.surface,
                        ))
                        .child(scrollbar::vertical(
                            &reasoning_viewport.scroll_handle,
                            &reasoning_viewport.scrollbar,
                        ))
                        .child(activity_scroll_guard(reasoning_viewport, reasoning_live)),
                );
            }
            if shows_diff {
                let diff = self.activity_diff_rows(activity);
                if !diff.is_empty() {
                    // One file is named above its code, the way a read is;
                    // several name themselves in headers inside the diff.
                    let path = match activity.file_changes.as_slice() {
                        [change] => Some(activity_read::display_path(
                            &change.path,
                            self.selected_workspace_path(),
                        )),
                        _ => None,
                    };
                    detail = detail.child(self.render_activity_diff(
                        id,
                        &diff,
                        path,
                        theme.surface,
                        theme,
                        cx,
                    ));
                }
            }
            if read_preview_available && let Some(preview) = self.activity_read_preview(activity) {
                detail = detail.child(self.render_activity_read_preview(
                    id,
                    &preview,
                    surface.viewports,
                    theme,
                ));
            } else if reasoning.is_none()
                && (!sections.is_empty() || !activity.image_urls.is_empty())
            {
                let palette = MarkdownPalette::from_theme(theme);
                let ctx = self.markdown_ctx(
                    format!("activity-{id}"),
                    &palette,
                    self.transcript_markdown_metrics(MarkdownMetrics::COMPACT),
                    false,
                );
                let (mono_size, mono_line) = self.activity_mono_text();
                let command = activity.kind == ActivityKind::Command;
                // A command reads as a terminal, so its whole card is mono. A
                // tool call reads as a form — labels in the interface's own
                // type, the values and output it carried in code type.
                let mut detail_card = div()
                    .w_full()
                    .min_w_0()
                    .when(shows_diff, |card| {
                        card.border_t_1().border_color(theme.border)
                    })
                    .when(command, |card| {
                        card.px(px(8.0))
                            .py(px(8.0))
                            .gap(px(8.0))
                            .font_family(md::render::MONO_FAMILY)
                            .text_size(px(mono_size))
                            .line_height(px(mono_line))
                    })
                    .when(!command, |card| {
                        card.px(px(10.0))
                            .py(px(10.0))
                            .gap(px(10.0))
                            .text_size(sp(SMALL_UI_FONT_SIZE))
                            .line_height(sp(18.0))
                    })
                    .flex()
                    .flex_col()
                    .text_color(theme.text_secondary)
                    .whitespace_normal()
                    .overflow_hidden();
                let mono_value = |content: SharedString| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_family(md::render::MONO_FAMILY)
                        .text_size(px(mono_size))
                        .line_height(px(mono_line))
                        .child(md::render::plain_text(
                            content,
                            md::render::MONO_FAMILY,
                            FontWeight::NORMAL,
                            theme.text_secondary,
                            &ctx,
                        ))
                };
                let field_row = |label: SharedString, value: AnyElement| {
                    div()
                        .w_full()
                        .min_w_0()
                        .min_h(px(mono_line.max(18.0)))
                        .flex()
                        .items_start()
                        .gap(px(12.0))
                        .child(
                            div()
                                .w(px(ACTIVITY_FIELD_LABEL_WIDTH))
                                .flex_none()
                                .truncate()
                                .text_color(theme.text_tertiary)
                                .child(label),
                        )
                        .child(value)
                };
                let mut shown_sections = 0usize;
                for section in sections {
                    let section_kind = section.kind;
                    let content = section.content;
                    if matches!(
                        section_kind,
                        ActivityDisclosureSectionKind::McpServer
                            | ActivityDisclosureSectionKind::ToolName
                    ) {
                        // The row already names the tool unless the provider
                        // gave the call a title of its own; repeating it
                        // here would only push the input down.
                        if activity.kind != ActivityKind::Tool
                            || row_text.contains(content.as_str())
                        {
                            continue;
                        }
                        let label = SharedString::from(section_kind.label().unwrap_or_default());
                        detail_card = detail_card.child(field_row(
                            label,
                            mono_value(content.into()).into_any_element(),
                        ));
                        shown_sections += 1;
                        continue;
                    }
                    let input = (section_kind == ActivityDisclosureSectionKind::Arguments)
                        .then(|| self.activity_input(activity))
                        .flatten();
                    let output_frame =
                        !command && section_kind == ActivityDisclosureSectionKind::Output;
                    let mut section_view = div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(if command { 3.0 } else { 6.0 }))
                        // Input and output are two halves of one call; a rule
                        // keeps the answer from reading as another argument.
                        .when(output_frame && shown_sections > 0, |section| {
                            section.pt(px(10.0)).border_t_1().border_color(theme.border)
                        });
                    shown_sections += 1;
                    if !command && let Some(label) = section_kind.label() {
                        let copy_content = content.clone();
                        let copied = self
                            .copied_activity_feedback
                            .contains_key(&(id, section_kind));
                        let copy_raku = cx.entity().downgrade();
                        let key_content = content.clone();
                        let key_raku = copy_raku.clone();
                        let copy_focus = self.transcript_control_focus(
                            format!("copy-activity-{}-{}", id, section_kind.id()),
                            cx,
                        );
                        let copy_tooltip = SharedString::from(if copied {
                            tr!("common.copied")
                        } else {
                            tr!("common.copy_named", name = label.to_lowercase())
                        });
                        section_view = section_view.child(
                            div()
                                .h(px(20.0))
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .font_weight(if command {
                                            FontWeight::MEDIUM
                                        } else {
                                            FontWeight::NORMAL
                                        })
                                        .text_color(theme.text_secondary)
                                        .child(label),
                                )
                                .when(!content.is_empty(), |header| {
                                    header.child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "copy-activity-{}-{}",
                                                id,
                                                section_kind.id()
                                            )))
                                            .track_focus(&copy_focus)
                                            .tab_index(0)
                                            .size(px(20.0))
                                            .rounded(px(5.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .cursor_default()
                                            .focus_visible(|button| button.bg(theme.overlay_strong))
                                            .hover(|button| button.bg(theme.overlay_strong))
                                            .child(icon(
                                                if copied {
                                                    "icons/check.svg"
                                                } else {
                                                    "icons/copy.svg"
                                                },
                                                11.0,
                                                theme.text_ghost,
                                            ))
                                            .tooltip(Tooltip::text(copy_tooltip.clone()))
                                            .on_click(move |_, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    copy_content.clone(),
                                                ));
                                                let _ = copy_raku.update(cx, |this, cx| {
                                                    this.show_activity_section_copied(
                                                        id,
                                                        section_kind,
                                                        cx,
                                                    );
                                                });
                                            })
                                            .on_key_down(move |event: &KeyDownEvent, _, cx| {
                                                if !event.keystroke.modifiers.modified()
                                                    && matches!(
                                                        event.keystroke.key.as_str(),
                                                        "enter" | "space"
                                                    )
                                                {
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(
                                                            key_content.to_string(),
                                                        ),
                                                    );
                                                    let _ = key_raku.update(cx, |this, cx| {
                                                        this.show_activity_section_copied(
                                                            id,
                                                            section_kind,
                                                            cx,
                                                        );
                                                    });
                                                    cx.stop_propagation();
                                                }
                                            }),
                                    )
                                }),
                        );
                    }
                    if !content.is_empty() {
                        if command && section_kind == ActivityDisclosureSectionKind::Command {
                            // The command is what this card is about, so it
                            // reads the way the shell that ran it did: a
                            // prompt, the program painted, its flags with it.
                            let group = SharedString::from(format!("activity-command-{id}"));
                            let copy = self.render_activity_section_copy(
                                id,
                                section_kind,
                                content.clone(),
                                group.clone(),
                                theme.surface,
                                4.0,
                                4.0,
                                theme,
                                window,
                                cx,
                            );
                            section_view = section_view.child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .relative()
                                    .group(group)
                                    .rounded(px(6.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .px(px(10.0))
                                    .py(px(8.0))
                                    .flex()
                                    .items_start()
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_color(theme.text_tertiary)
                                            .child("$"),
                                    )
                                    .child(div().flex_1().min_w_0().child(md::render::tinted_text(
                                        content,
                                        Some(md::highlight::Lang::Shell),
                                        md::render::MONO_FAMILY,
                                        FontWeight::NORMAL,
                                        theme.text_secondary,
                                        &ctx,
                                    )))
                                    .child(copy),
                            );
                        } else if let Some(input) = input {
                            for field in &input.fields {
                                section_view = section_view.child(field_row(
                                    field.label.clone(),
                                    mono_value(field.value.clone()).into_any_element(),
                                ));
                            }
                        } else if section_kind == ActivityDisclosureSectionKind::Output {
                            let group = SharedString::from(format!("activity-output-{id}"));
                            // A tool's output is framed like the code it
                            // usually is, and its header already offers copy.
                            let copy = command.then(|| {
                                self.render_activity_section_copy(
                                    id,
                                    section_kind,
                                    content.clone(),
                                    group.clone(),
                                    theme.surface,
                                    4.0,
                                    12.0,
                                    theme,
                                    window,
                                    cx,
                                )
                            });
                            let trimmed = content.trim_start();
                            let output_lang = (!command
                                && (trimmed.starts_with('{') || trimmed.starts_with('[')))
                            .then_some(md::highlight::Lang::Json);
                            let output_viewport = surface
                                .viewports
                                .borrow_mut()
                                .entry(id)
                                .or_default()
                                .clone();
                            let wheel_scroll = output_viewport.scroll_handle.clone();
                            let wheel_follow_tail = output_viewport.follow_tail.clone();
                            section_view = section_view.child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .relative()
                                    .group(group)
                                    .max_h(px(400.0))
                                    .when(!command, |frame| {
                                        frame.rounded(px(8.0)).border_1().border_color(theme.border)
                                    })
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "activity-output-scroll-{id}"
                                            )))
                                            .w_full()
                                            .min_w_0()
                                            .max_h(px(400.0))
                                            .overflow_y_scroll()
                                            .track_scroll(&output_viewport.scroll_handle)
                                            .when(command, |output| output.py(px(4.0)).pr(px(8.0)))
                                            .when(!command, |output| {
                                                output
                                                    .px(px(12.0))
                                                    .py(px(8.0))
                                                    .font_family(md::render::MONO_FAMILY)
                                                    .text_size(px(mono_size))
                                                    .line_height(px(mono_line))
                                            })
                                            .child(md::render::tinted_text(
                                                content,
                                                output_lang,
                                                md::render::MONO_FAMILY,
                                                FontWeight::NORMAL,
                                                theme.text_secondary,
                                                &ctx,
                                            ))
                                            .on_scroll_wheel(move |_, window, cx| {
                                                contain_scroll(&wheel_scroll, cx);
                                                let scroll = wheel_scroll.clone();
                                                let follow_tail = wheel_follow_tail.clone();
                                                window.defer(cx, move |_, _| {
                                                    follow_tail
                                                        .set(activity_scroll_at_bottom(&scroll));
                                                });
                                            }),
                                    )
                                    .child(scrollbar::edge_fade(
                                        output_viewport.scroll_handle.clone(),
                                        scrollbar::FadeEdge::Top,
                                        theme.surface,
                                    ))
                                    .child(scrollbar::edge_fade(
                                        output_viewport.scroll_handle.clone(),
                                        scrollbar::FadeEdge::Bottom,
                                        theme.surface,
                                    ))
                                    .child(scrollbar::vertical(
                                        &output_viewport.scroll_handle,
                                        &output_viewport.scrollbar,
                                    ))
                                    .child(activity_scroll_guard(
                                        output_viewport,
                                        !activity.complete,
                                    ))
                                    .children(copy),
                            );
                        } else {
                            section_view = section_view.child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .flex()
                                    .child(mono_value(content.clone().into())),
                            );
                        }
                    }
                    detail_card = detail_card.child(section_view);
                }
                for (image_index, image_url) in activity.image_urls.iter().enumerate() {
                    detail_card = detail_card.child(
                        self.render_activity_image(
                            image_url,
                            id,
                            image_index,
                            activity
                                .output
                                .as_deref()
                                .and_then(image_preview::image_path_for_reference),
                            theme,
                            cx,
                        ),
                    );
                }
                detail = detail.child(detail_card);
            }
            items = items.child(item.child(detail));
        }
        cluster.child(items).into_any_element()
    }

    /// The code a read returned, drawn as the file it came from: the path it
    /// names, then numbered, highlighted lines in one capped viewport so a
    /// long file stays one transcript row tall.
    fn render_activity_read_preview(
        &self,
        id: Uuid,
        preview: &activity_read::ReadPreview,
        viewports: &RefCell<HashMap<Uuid, ActivityScrollViewport>>,
        theme: &Theme,
    ) -> AnyElement {
        let viewport = viewports.borrow_mut().entry(id).or_default().clone();
        let wheel_scroll = viewport.scroll_handle.clone();
        let (mono_size, mono_line) = self.activity_mono_text();
        // The gutter is sized to the widest number the file reaches, so it
        // holds a 4-digit file as readily as a 2-digit one.
        let digits = preview
            .lines
            .iter()
            .map(|line| line.number)
            .max()
            .unwrap_or(0)
            .to_string()
            .len()
            .max(2) as f32;
        let gutter = (mono_size * 0.62 * digits + 24.0).round();
        let palette = MarkdownPalette::from_theme(theme);
        let code_font = font(md::render::MONO_FAMILY);
        let mut rows = div()
            .id(SharedString::from(format!("activity-read-scroll-{id}")))
            .w_full()
            .min_w_0()
            .max_h(px(ACTIVITY_DIFF_MAX_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&viewport.scroll_handle)
            .py(px(8.0))
            .flex()
            .flex_col()
            .font_family(md::render::MONO_FAMILY)
            .text_size(px(mono_size))
            .line_height(px(mono_line))
            .on_scroll_wheel(move |_, _, cx| contain_scroll(&wheel_scroll, cx));
        for (index, line) in preview.lines.iter().enumerate() {
            let flat = read_line_flat_text(line, theme, &palette, &code_font);
            let selectable = md::render::selectable_flat_text(
                &flat,
                crate::md::selection::TextKey::new(format!("activity-read-{id}-{index}"), 0),
                self.transcript_selection.clone(),
                theme.code_wash,
                theme.selection,
                false,
            );
            rows = rows.child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex_none()
                    .flex()
                    .items_start()
                    .child(
                        div()
                            .w(px(gutter))
                            .flex_none()
                            .pr(px(10.0))
                            .text_right()
                            .text_color(theme.text_ghost)
                            .child(line.label.clone()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .whitespace_normal()
                            .text_color(theme.text_secondary)
                            .child(selectable),
                    ),
            );
        }
        if preview.hidden_lines > 0 {
            let note = if preview.hidden_lines == 1 {
                tr!("diff.rows_hidden_one")
            } else {
                tr!("diff.rows_hidden", count = preview.hidden_lines)
            };
            rows = rows.child(
                div()
                    .w_full()
                    .min_w_0()
                    .px(px(12.0))
                    .py(px(4.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::from(note)),
            );
        }
        let mut card = div()
            .relative()
            .w_full()
            .min_w_0()
            .px(px(10.0))
            .py(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0));
        if !preview.path.is_empty() {
            card = card.child(activity_card_path(preview.path.clone(), theme));
        }
        card.child(
            div()
                .relative()
                .w_full()
                .min_w_0()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .overflow_hidden()
                .child(rows)
                .child(scrollbar::edge_fade(
                    viewport.scroll_handle.clone(),
                    scrollbar::FadeEdge::Top,
                    theme.surface,
                ))
                .child(scrollbar::edge_fade(
                    viewport.scroll_handle.clone(),
                    scrollbar::FadeEdge::Bottom,
                    theme.surface,
                ))
                .child(scrollbar::vertical(
                    &viewport.scroll_handle,
                    &viewport.scrollbar,
                )),
        )
        .into_any_element()
    }

    /// The diff for an expanded file-change activity.
    ///
    /// Rows bleed to the card's edges so a changed line reads as a band, and
    /// the whole diff sits in the same capped, faded viewport as command
    /// output — a large edit stays one transcript row tall.
    fn render_activity_diff(
        &self,
        id: Uuid,
        diff: &activity_diff::Diff,
        path: Option<String>,
        surface: Hsla,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let viewport = self
            .activity_diff_viewports
            .borrow_mut()
            .entry(id)
            .or_default()
            .clone();
        let wheel_scroll = viewport.scroll_handle.clone();
        let (diff_mono_size, diff_mono_line) = self.activity_mono_text();
        let mut rows = div()
            .id(SharedString::from(format!("activity-diff-scroll-{id}")))
            .w_full()
            .min_w_0()
            .max_h(px(ACTIVITY_DIFF_MAX_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&viewport.scroll_handle)
            // Bands stop short of the frame's rounded corners, which a
            // rectangular clip would otherwise let them poke through.
            .py(px(6.0))
            .flex()
            .flex_col()
            .font_family(md::render::MONO_FAMILY)
            .text_size(px(diff_mono_size))
            .line_height(px(diff_mono_line))
            .on_scroll_wheel(move |_, _, cx| contain_scroll(&wheel_scroll, cx));
        for (index, line) in diff.snapshot.lines.iter().enumerate() {
            rows = rows.child(self.render_activity_diff_row(
                id,
                index,
                line,
                &diff.snapshot,
                theme,
                cx,
            ));
        }
        if diff.hidden_rows > 0 {
            let note = if diff.hidden_rows == 1 {
                tr!("diff.rows_hidden_one")
            } else {
                tr!("diff.rows_hidden", count = diff.hidden_rows)
            };
            rows = rows.child(
                div()
                    .w_full()
                    .min_w_0()
                    .px(px(12.0))
                    .py(px(4.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::from(note)),
            );
        }
        let frame = div()
            .w_full()
            .min_w_0()
            .relative()
            .max_h(px(ACTIVITY_DIFF_MAX_HEIGHT))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .overflow_hidden()
            .child(rows)
            .child(scrollbar::edge_fade(
                viewport.scroll_handle.clone(),
                scrollbar::FadeEdge::Top,
                surface,
            ))
            .child(scrollbar::edge_fade(
                viewport.scroll_handle.clone(),
                scrollbar::FadeEdge::Bottom,
                surface,
            ))
            .child(scrollbar::vertical(
                &viewport.scroll_handle,
                &viewport.scrollbar,
            ));
        div()
            .w_full()
            .min_w_0()
            .px(px(10.0))
            .py(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .when_some(path.filter(|path| !path.is_empty()), |card, path| {
                card.child(activity_card_path(path, theme))
            })
            .child(frame)
            .into_any_element()
    }

    fn render_activity_diff_row(
        &self,
        id: Uuid,
        index: usize,
        line: &crate::review_diff::Line,
        snapshot: &crate::review_diff::Snapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::review_diff::LineKind;

        match &line.kind {
            LineKind::FileHeader => {
                let Some(file) = snapshot.files.get(line.file_index) else {
                    return div().into_any_element();
                };
                div()
                    .w_full()
                    .min_w_0()
                    .h(px(24.0))
                    .pl(px(10.0))
                    .pr(px(6.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .bg(theme.overlay)
                    .border_b_1()
                    .border_color(theme.border)
                    .text_color(theme.text_secondary)
                    .font_weight(FontWeight::MEDIUM)
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .child(SharedString::from(file.path.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::NORMAL)
                            .text_color(theme.success)
                            .child(SharedString::from(format!("+{}", file.additions))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::NORMAL)
                            .text_color(theme.danger)
                            .child(SharedString::from(format!("-{}", file.deletions))),
                    )
                    .child(self.render_activity_open_file_button(
                        format!("activity-diff-open-{id}-{}", line.file_index),
                        file.path.clone(),
                        theme,
                        cx,
                    ))
                    .into_any_element()
            }
            // Unchanged lines the provider left out of its patch. Nothing was
            // withheld locally, so this marks the break rather than offering
            // to expand it the way the Review panel does.
            LineKind::Gap(gap) => activity_diff_break_row(
                Some(tr!("diff.unmodified_lines", count = gap.count())),
                theme,
            ),
            LineKind::HunkHeader | LineKind::Meta => activity_diff_break_row(
                (!line.content.is_empty()).then(|| line.content.clone()),
                theme,
            ),
            LineKind::Context | LineKind::Addition | LineKind::Deletion => render_diff_code_row(
                line,
                index,
                &format!("activity-diff-{id}"),
                &self.transcript_selection,
                DiffRowStyle::activity(self.state.code_font_size),
                theme,
            ),
        }
    }
}

/// The file a read or edit card is about, named above its code.
fn activity_card_path(path: String, theme: &Theme) -> impl IntoElement {
    div()
        .w_full()
        .min_w_0()
        .truncate()
        .text_size(sp(SMALL_UI_FONT_SIZE))
        .line_height(sp(16.0))
        .text_color(theme.text)
        .child(SharedString::from(path))
}

/// The expand affordance a row ends with. Reads place it right behind their
/// text; every other row pins it to the end.
fn activity_chevron(expanded: bool, theme: &Theme) -> impl IntoElement {
    icon(
        if expanded {
            "icons/chevron-down.svg"
        } else {
            "icons/chevron-right.svg"
        },
        10.0,
        theme.text_tertiary,
    )
}

/// One numbered line of a read, painted from the tokens prepared when the
/// activity was expanded. A line without tokens still gets its plain run, so
/// the runs tile the text exactly.
fn read_line_flat_text(
    line: &activity_read::ReadLine,
    theme: &Theme,
    palette: &MarkdownPalette,
    code_font: &gpui::Font,
) -> md::render::FlatText {
    let mut runs = Vec::with_capacity(line.tokens.len() * 2 + 1);
    let mut offset = 0;
    let mut push = |len: usize, color: Hsla| {
        if len == 0 {
            return;
        }
        runs.push(TextRun {
            len,
            font: code_font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        });
    };
    for token in &line.tokens {
        if token.range.start > offset {
            push(token.range.start - offset, theme.text_secondary);
        }
        push(token.range.len(), palette.token(token.class));
        offset = token.range.end;
    }
    if offset < line.content.len() {
        push(line.content.len() - offset, theme.text_secondary);
    }
    md::render::FlatText {
        text: line.content.clone(),
        runs,
        links: Vec::new(),
        code_ranges: Vec::new(),
        math: None,
        active_link: None,
    }
}

/// The separator between two hunks of the same file.
fn activity_diff_break_row(label: Option<String>, theme: &Theme) -> AnyElement {
    div()
        .w_full()
        .min_w_0()
        .h(px(20.0))
        .flex_none()
        .flex()
        .items_center()
        .font_family(md::render::MONO_FAMILY)
        // Fixed like the Review panel's gap and hunk captions: a caption in
        // the code surface follows neither font setting.
        .text_size(sp(SMALL_UI_FONT_SIZE))
        .bg(theme.overlay)
        .text_color(theme.text_ghost)
        .child(
            div()
                .w(px(ACTIVITY_DIFF_GUTTER_WIDTH))
                .flex_none()
                .self_stretch()
                .flex()
                .items_center()
                .justify_center()
                .border_r_1()
                .border_color(theme.border)
                .child("⋯"),
        )
        .children(label.map(|label| {
            div()
                .min_w_0()
                .pl(px(12.0))
                .truncate()
                .child(SharedString::from(label))
        }))
        .into_any_element()
}

fn activity_scroll_at_bottom(scroll: &ScrollHandle) -> bool {
    let scrolled = -scroll.offset().y;
    scroll.max_offset().y - scrolled <= px(0.5)
}

fn activity_scroll_follow_state(
    following: bool,
    previous_scrolled: Option<Pixels>,
    previous_max_offset: Option<Pixels>,
    scrolled: Pixels,
    max_offset: Pixels,
) -> bool {
    let at_bottom = max_offset - scrolled <= px(0.5);
    let user_moved = previous_scrolled.zip(previous_max_offset).is_some_and(
        |(previous_scrolled, previous_max_offset)| {
            (scrolled - previous_scrolled).abs() > px(0.5)
                && (max_offset - previous_max_offset).abs() <= px(0.5)
        },
    );
    if user_moved || at_bottom {
        at_bottom
    } else {
        following
    }
}

/// Pure byte-window arithmetic shared by main and delegated reasoning peeks.
/// Every returned offset is a character boundary of `content`.
fn live_reasoning_window_anchor(cached: usize, content: &str) -> usize {
    // A restarted block can leave the cached start past the end of the new
    // content or inside a multibyte character; either way the window is
    // stale (`is_char_boundary` is false past the end too), so restart it.
    let cached = if content.is_char_boundary(cached) {
        cached
    } else {
        0
    };
    if content.len() - cached <= LIVE_REASONING_WINDOW_MAX {
        return cached;
    }
    // Slide: re-anchor near the tail, preferring a block boundary so the
    // window opens on whole markdown. The raw cut is an arbitrary byte
    // offset, so advance it to a character boundary before slicing; the
    // end of the string is always a boundary, so this terminates.
    let mut cut = content.len() - LIVE_REASONING_WINDOW_TARGET;
    while !content.is_char_boundary(cut) {
        cut += 1;
    }
    content[cut..]
        .find("\n\n")
        .map(|found| cut + found + 2)
        .unwrap_or(cut)
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ActivityClusterPart {
    All,
    Header,
    Item,
}

pub(super) struct ActivityRenderContext<'a> {
    pub(super) part: ActivityClusterPart,
    pub(super) live_group: bool,
    pub(super) live_reasoning_id: Option<Uuid>,
    pub(super) expanded: bool,
    pub(super) expanded_items: &'a HashMap<Uuid, bool>,
    pub(super) markdown: &'a RefCell<HashMap<Uuid, MarkdownView>>,
    pub(super) window_starts: &'a RefCell<HashMap<Uuid, usize>>,
    pub(super) viewports: &'a RefCell<HashMap<Uuid, ActivityScrollViewport>>,
}

fn activity_scroll_guard(viewport: ActivityScrollViewport, live: bool) -> impl IntoElement {
    canvas(
        move |_, window, cx| {
            let scrolled = -viewport.scroll_handle.offset().y;
            let max_offset = viewport.scroll_handle.max_offset().y;
            let following = activity_scroll_follow_state(
                viewport.follow_tail.get(),
                viewport.last_scrolled.get(),
                viewport.last_max_offset.get(),
                scrolled,
                max_offset,
            );
            viewport.follow_tail.set(following);
            viewport.last_scrolled.set(Some(scrolled));
            viewport.last_max_offset.set(Some(max_offset));
            if live && following && max_offset - scrolled > px(0.5) {
                viewport.scroll_handle.scroll_to_bottom();
                // Notify the enclosing island rather than `window.refresh()`:
                // a refresh busts every pane cache, and this fires on each
                // stream commit while a live viewport follows its tail.
                cx.notify(window.current_view());
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .w(px(0.0))
    .h(px(0.0))
}

#[cfg(test)]
mod navigation_rail_scroll_tests {
    use gpui::{
        Context, Entity, IntoElement, ListAlignment, ListState, Render, ScrollDelta,
        ScrollWheelEvent, TestAppContext, Window, div, list, point, px,
    };

    use super::*;

    struct NavigationRailScrollHarness {
        rail: Entity<ConversationNavigationRail>,
        transcript: ListState,
    }

    impl Render for NavigationRailScrollHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .relative()
                .child(
                    list(self.transcript.clone(), |_, _, _| {
                        div().h(px(24.0)).w_full().into_any_element()
                    })
                    .size_full(),
                )
                .child(self.rail.clone())
        }
    }

    #[gpui::test]
    fn scrolling_the_navigation_rail_does_not_scroll_the_transcript(cx: &mut TestAppContext) {
        let transcript =
            ListState::new(100, ListAlignment::Top, px(24.0)).with_uniform_item_height(px(24.0));
        transcript.scroll_to(ListOffset {
            item_ix: 20,
            offset_in_item: px(0.0),
        });
        let transcript_for_view = transcript.clone();
        let (harness, cx) = cx.add_window_view(move |_, cx| {
            let turns = (0..100)
                .map(|index| TranscriptNavigationTurn {
                    message_id: Uuid::new_v4(),
                    message_index: index,
                    row_index: index,
                    prompt: format!("Prompt {index}"),
                    response: String::new(),
                })
                .collect::<Vec<_>>();
            let rail = cx.new(|cx| {
                let mut rail = ConversationNavigationRail::new();
                rail.set_snapshot(
                    ConversationNavigationRailSnapshot {
                        visible: true,
                        turns: Rc::new(turns),
                        viewport_height: 600.0,
                        active_turn: None,
                        reset_generation: 0,
                        theme_is_dark: true,
                    },
                    cx,
                );
                rail
            });
            NavigationRailScrollHarness {
                rail,
                transcript: transcript_for_view,
            }
        });
        let rail = cx.read_entity(&harness, |harness, _| harness.rail.clone());
        let transcript_offset = transcript.logical_scroll_top();

        cx.simulate_event(ScrollWheelEvent {
            position: point(px(20.0), px(300.0)),
            delta: ScrollDelta::Pixels(point(px(0.0), px(60.0))),
            ..Default::default()
        });

        let transcript_offset_after_boundary_scroll = transcript.logical_scroll_top();
        assert!(
            transcript_offset_after_boundary_scroll.item_ix == transcript_offset.item_ix
                && transcript_offset_after_boundary_scroll.offset_in_item
                    == transcript_offset.offset_in_item,
            "a gesture at the rail boundary must not move the transcript",
        );

        cx.simulate_event(ScrollWheelEvent {
            position: point(px(20.0), px(300.0)),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-60.0))),
            ..Default::default()
        });

        let rail_offset =
            cx.read_entity(&rail, |rail, _| rail.turn_list_state.logical_scroll_top());
        assert!(
            rail_offset.item_ix > 0 || rail_offset.offset_in_item > px(0.0),
            "the wheel gesture should still scroll the navigation rail",
        );
        let transcript_offset_after_rail_scroll = transcript.logical_scroll_top();
        assert!(
            transcript_offset_after_rail_scroll.item_ix == transcript_offset.item_ix
                && transcript_offset_after_rail_scroll.offset_in_item
                    == transcript_offset.offset_in_item,
            "the navigation rail must contain the gesture instead of scrolling the transcript",
        );
    }
}

#[cfg(test)]
mod activity_scroll_tests {
    use super::*;

    #[test]
    fn user_scroll_pauses_following_until_the_tail_is_reached_again() {
        assert!(!activity_scroll_follow_state(
            true,
            Some(px(100.0)),
            Some(px(200.0)),
            px(70.0),
            px(200.0),
        ));
        assert!(!activity_scroll_follow_state(
            false,
            Some(px(70.0)),
            Some(px(200.0)),
            px(120.0),
            px(200.0),
        ));
        assert!(activity_scroll_follow_state(
            false,
            Some(px(120.0)),
            Some(px(200.0)),
            px(200.0),
            px(200.0),
        ));
    }

    #[test]
    fn growing_content_does_not_cancel_tail_following() {
        assert!(activity_scroll_follow_state(
            true,
            Some(px(200.0)),
            Some(px(200.0)),
            px(200.0),
            px(240.0),
        ));
        assert!(!activity_scroll_follow_state(
            false,
            Some(px(120.0)),
            Some(px(200.0)),
            px(120.0),
            px(240.0),
        ));
    }
}

#[cfg(test)]
mod live_reasoning_window_tests {
    use super::*;

    #[test]
    fn slide_lands_on_a_character_boundary_in_multibyte_content() {
        // A body of 3-byte chars with two ASCII bytes at the end leaves the
        // naive cut mid-character while both tuning consts are KiB multiples;
        // the guard assert keeps the test honest if they are ever retuned.
        let content = "界".repeat(LIVE_REASONING_WINDOW_MAX / 3 + 1) + "zz";
        assert!(content.len() > LIVE_REASONING_WINDOW_MAX);
        assert!(
            !content.is_char_boundary(content.len() - LIVE_REASONING_WINDOW_TARGET),
            "setup must place the naive cut mid-character to cover the panic",
        );
        let start = live_reasoning_window_anchor(0, &content);
        assert!(content.is_char_boundary(start));
        assert!(content.len() - start <= LIVE_REASONING_WINDOW_TARGET);
        assert!(!content[start..].is_empty());
    }

    #[test]
    fn stale_start_from_a_restarted_block_resets_to_zero() {
        let content = "思".repeat(64);
        assert!(!content.is_char_boundary(4));
        assert_eq!(live_reasoning_window_anchor(4, &content), 0);
        assert_eq!(live_reasoning_window_anchor(content.len() + 1, &content), 0);
    }

    #[test]
    fn stale_start_still_slides_when_the_new_content_is_long() {
        let content = "界".repeat(LIVE_REASONING_WINDOW_MAX);
        assert!(!content.is_char_boundary(5));
        let start = live_reasoning_window_anchor(5, &content);
        assert!(content.is_char_boundary(start));
        assert!(content.len() - start <= LIVE_REASONING_WINDOW_TARGET);
    }

    #[test]
    fn slide_reanchors_after_a_block_boundary_when_one_is_near() {
        let content = format!("{}\n\ntail", "a".repeat(LIVE_REASONING_WINDOW_MAX));
        let start = live_reasoning_window_anchor(0, &content);
        assert_eq!(&content[start..], "tail");
    }

    #[test]
    fn window_below_the_threshold_keeps_the_cached_start() {
        let content = "a".repeat(LIVE_REASONING_WINDOW_MAX);
        assert_eq!(live_reasoning_window_anchor(7, &content), 7);
    }
}

/// A provider notice — a retried request, a model switch, a warning — as a
/// quiet line of its own. It has nothing to expand, so it takes no focus: the
/// whole message wraps in place and the icon is never its only signal.
fn render_notice_row(
    activity: &ActivityItem,
    surface: &ActivityRenderContext<'_>,
    theme: &Theme,
) -> AnyElement {
    if surface.part == ActivityClusterPart::Item {
        return div().into_any_element();
    }
    let color = if activity.failed {
        theme.danger
    } else {
        theme.warning
    };
    let status_icon = if activity.complete {
        icon(
            if activity.failed {
                "icons/x.svg"
            } else {
                "icons/alert.svg"
            },
            13.0,
            color,
        )
        .into_any_element()
    } else {
        crate::ui::motion::spin_slow(icon("icons/loader-circle.svg", 13.0, theme.text_secondary))
    };
    div()
        .w_full()
        .min_w_0()
        .my(px(4.0))
        .px(px(6.0))
        .py(px(4.0))
        .flex()
        .items_start()
        .gap(px(8.0))
        .text_size(sp(12.0))
        .child(div().flex_none().pt(px(2.0)).child(status_icon))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .text_color(theme.text_tertiary)
                .child(
                    div()
                        .text_color(theme.text_secondary)
                        .child(activity.title.clone()),
                )
                .when_some(
                    activity.detail.clone().filter(|detail| !detail.is_empty()),
                    |row, detail| row.child(detail),
                ),
        )
        .into_any_element()
}
