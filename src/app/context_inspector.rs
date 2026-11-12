//! The Context inspector: what occupies the selected task's context window,
//! and exactly what Raku itself handed the provider — the launch command, the
//! system-prompt text it appends, the tools it serves, and the text it adds to
//! prompts.
//!
//! What a task's provider reports is kept per task, outliving the provider
//! process, and mirrored to a small file per task so a stopped task still
//! shows what was last recorded. Fetches, saves and loads all run off the UI
//! thread; frames only read the stored answer.

use std::path::PathBuf;

use raku_protocol::context_inspection::{
    ContextBreakdown, ContextCategory, ContextCategoryKind, ContextDetail, ContextSection,
    LaunchManifest, PromptInjection, estimate_tokens,
};
use serde::{Deserialize, Serialize};

use crate::ui::ActivationExt;
use crate::usage::format_tokens;

use super::*;

/// Prompt injections kept per task; only the newest matter for debugging.
const MAX_INJECTIONS: usize = 16;
/// Rows a drill-down list shows before "Show all".
const SECTION_PREVIEW_ROWS: usize = 8;
/// A provider that never answers must not leave "Measuring…" up forever.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Categorical slots for the window bar; a ninth used category folds into
/// "Other" rather than generating a hue.
const MAX_BAR_SLOTS: usize = 8;
/// Keyboard scroll step for the arrow keys.
const SCROLL_STEP: f32 = 48.0;

/// The recorded part of an inspection: what is saved to disk and copied as
/// JSON. Field names are the file format.
#[derive(Clone, Default, Deserialize, Serialize)]
struct ContextSnapshot {
    #[serde(default, rename = "launch", skip_serializing_if = "Option::is_none")]
    manifest: Option<LaunchManifest>,
    #[serde(default, skip_serializing_if = "VecDeque::is_empty")]
    injections: VecDeque<PromptInjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    breakdown: Option<ContextBreakdown>,
}

/// Keep writes for one task in order, including the final write when its
/// inspection leaves memory. Independent tasks never wait for each other.
#[derive(Default)]
struct ContextSnapshotWriter {
    pending: Option<gpui::Task<()>>,
}

impl ContextSnapshotWriter {
    fn save(
        &mut self,
        path: PathBuf,
        snapshot: ContextSnapshot,
        executor: &gpui::BackgroundExecutor,
    ) {
        let previous = self.pending.take();
        self.pending = Some(executor.spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            if let Err(error) = write_context_snapshot(&path, &snapshot) {
                eprintln!("raku: could not save the context snapshot: {error}");
            }
        }));
    }
}

impl Drop for ContextSnapshotWriter {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.detach();
        }
    }
}

/// What the inspector knows about one task.
#[derive(Default)]
pub(super) struct ContextInspection {
    recorded: ContextSnapshot,
    error: Option<String>,
    pending: bool,
    /// Numbers fetches so a timeout only fails the request it guards.
    generation: u64,
    snapshot_loading: bool,
    snapshot_dirty: bool,
    snapshot_writer: ContextSnapshotWriter,
}

impl ContextInspection {
    /// A new provider process: its launch replaces the last one, and the
    /// injections recorded for the old process no longer apply.
    fn set_manifest(&mut self, manifest: LaunchManifest) {
        self.recorded.manifest = Some(manifest);
        self.recorded.injections.clear();
        self.snapshot_dirty = true;
    }

    fn record_injection(&mut self, injection: PromptInjection) {
        let injections = &mut self.recorded.injections;
        if injections.len() == MAX_INJECTIONS {
            injections.pop_front();
        }
        injections.push_back(injection);
        self.snapshot_dirty = true;
    }

    fn set_breakdown(&mut self, breakdown: ContextBreakdown) {
        self.recorded.breakdown = Some(breakdown);
        self.error = None;
        self.pending = false;
        self.snapshot_dirty = true;
    }

    fn set_error(&mut self, error: String) {
        self.error = Some(error);
        self.pending = false;
    }

    fn stop_fetch(&mut self) {
        self.pending = false;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Fill in what a saved snapshot knows without overwriting anything this
    /// session has already recorded live.
    fn merge_saved(&mut self, saved: ContextSnapshot) {
        let recorded = &mut self.recorded;
        if recorded.manifest.is_none() {
            recorded.manifest = saved.manifest;
            if recorded.injections.is_empty() {
                recorded.injections = saved.injections;
            }
        }
        if recorded.breakdown.is_none() {
            recorded.breakdown = saved.breakdown;
        }
    }
}

/// View state of the right panel's Context tab. The data itself lives in
/// `Raku::context_inspections`; this only remembers how the tab is presented.
pub(super) struct ContextInspectorState {
    body_focus: FocusHandle,
    refresh_focus: FocusHandle,
    copy_focus: FocusHandle,
    /// Focus handles for the expandable rows, created as they first draw.
    toggle_focus: HashMap<SharedString, FocusHandle>,
    expanded: HashSet<SharedString>,
    scroll: ScrollHandle,
    scrollbar: Rc<ScrollbarState>,
}

impl ContextInspectorState {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            body_focus: cx.focus_handle(),
            refresh_focus: cx.focus_handle(),
            copy_focus: cx.focus_handle(),
            toggle_focus: HashMap::new(),
            expanded: HashSet::new(),
            scroll: ScrollHandle::new(),
            scrollbar: ScrollbarState::new(),
        }
    }
}

/// One file per task beside the app database: `temp/context/` in debug
/// builds, the application support directory in release.
fn context_snapshot_path(session_id: Uuid) -> Option<PathBuf> {
    Some(
        crate::persistence::StateStore::default_path()
            .parent()?
            .join("context")
            .join(format!("{session_id}.json")),
    )
}

fn write_context_snapshot(path: &Path, snapshot: &ContextSnapshot) -> std::io::Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let bytes = serde_json::to_vec(snapshot).map_err(std::io::Error::other)?;
    // Write beside and rename, so a crash never leaves a torn file.
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, path)
}

impl Raku {
    /// Open the selected task's Context tab in the right panel and measure.
    pub(super) fn open_context_panel(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        if self.state.selected_session != Some(session_id) {
            return;
        }
        self.ensure_context_snapshot_loaded(session_id, cx);
        self.fetch_context_breakdown(session_id, cx);
        self.open_right_panel_surface(RightPanelSurface::Context, cx);
    }

    pub(super) fn record_context_manifest(
        &mut self,
        session_id: Uuid,
        manifest: LaunchManifest,
        cx: &mut Context<Self>,
    ) {
        self.ensure_context_snapshot_loaded(session_id, cx);
        self.context_inspections
            .entry(session_id)
            .or_default()
            .set_manifest(manifest);
        self.save_context_snapshot(session_id, cx);
    }

    pub(super) fn record_context_injection(
        &mut self,
        session_id: Uuid,
        injection: PromptInjection,
        cx: &mut Context<Self>,
    ) {
        self.ensure_context_snapshot_loaded(session_id, cx);
        self.context_inspections
            .entry(session_id)
            .or_default()
            .record_injection(injection);
        self.save_context_snapshot(session_id, cx);
    }

    pub(super) fn record_context_breakdown(
        &mut self,
        session_id: Uuid,
        breakdown: ContextBreakdown,
        cx: &mut Context<Self>,
    ) {
        self.ensure_context_snapshot_loaded(session_id, cx);
        self.context_inspections
            .entry(session_id)
            .or_default()
            .set_breakdown(breakdown);
        self.save_context_snapshot(session_id, cx);
        cx.notify();
    }

    pub(super) fn record_context_error(
        &mut self,
        session_id: Uuid,
        error: String,
        cx: &mut Context<Self>,
    ) {
        self.context_inspections
            .entry(session_id)
            .or_default()
            .set_error(error);
        cx.notify();
    }

    pub(super) fn stop_context_fetch(&mut self, session_id: Uuid) {
        if let Some(inspection) = self.context_inspections.get_mut(&session_id) {
            inspection.stop_fetch();
        }
    }

    /// Mirror the task's recorded snapshot to disk off the UI thread. These
    /// events are rare — a launch, a first prompt, an explicit measure — so
    /// each one simply writes the whole small file.
    fn save_context_snapshot(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let (Some(inspection), Some(path)) = (
            self.context_inspections.get_mut(&session_id),
            context_snapshot_path(session_id),
        ) else {
            return;
        };
        // A live event can arrive before the saved record has loaded. Merge
        // that record first so this save cannot erase its last breakdown.
        if inspection.snapshot_loading || !inspection.snapshot_dirty {
            return;
        }
        inspection.snapshot_dirty = false;
        let snapshot = inspection.recorded.clone();
        inspection
            .snapshot_writer
            .save(path, snapshot, cx.background_executor());
    }

    /// Read a task's saved snapshot once per app run, off the UI thread.
    fn ensure_context_snapshot_loaded(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        if !self.context_snapshot_loads.insert(session_id) {
            return;
        }
        let Some(path) = context_snapshot_path(session_id) else {
            return;
        };
        self.context_inspections
            .entry(session_id)
            .or_default()
            .snapshot_loading = true;
        cx.spawn(async move |raku, cx| {
            let saved = cx
                .background_executor()
                .spawn(async move {
                    let bytes = std::fs::read(path).ok()?;
                    serde_json::from_slice::<ContextSnapshot>(&bytes).ok()
                })
                .await;
            let _ = raku.update(cx, |raku, cx| {
                // A task deleted while this read ran must stay deleted.
                let Some(inspection) = raku.context_inspections.get_mut(&session_id) else {
                    return;
                };
                if let Some(saved) = saved {
                    inspection.merge_saved(saved);
                }
                inspection.snapshot_loading = false;
                raku.save_context_snapshot(session_id, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Drop a deleted task's record and its file.
    pub(super) fn forget_context_snapshot(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let previous = self
            .context_inspections
            .remove(&session_id)
            .and_then(|mut inspection| inspection.snapshot_writer.pending.take());
        self.context_snapshot_loads.remove(&session_id);
        if let Some(path) = context_snapshot_path(session_id) {
            cx.background_executor()
                .spawn(async move {
                    if let Some(previous) = previous {
                        previous.await;
                    }
                    let _ = std::fs::remove_file(path);
                })
                .detach();
        }
    }

    /// Ask the live runtime for a fresh breakdown. Without a runtime there is
    /// nothing to ask; the tab shows what was recorded instead of starting a
    /// provider.
    fn fetch_context_breakdown(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let Some(runtime) = self.runtimes.get(&session_id) else {
            return;
        };
        let inspection = self.context_inspections.entry(session_id).or_default();
        if inspection.pending {
            return;
        }
        inspection.pending = true;
        inspection.generation += 1;
        let generation = inspection.generation;
        runtime.driver.context_breakdown(ContextDetail::Full);
        cx.spawn(async move |raku, cx| {
            cx.background_executor().timer(FETCH_TIMEOUT).await;
            let _ = raku.update(cx, |raku, cx| {
                if let Some(inspection) = raku
                    .context_inspections
                    .get_mut(&session_id)
                    .filter(|inspection| inspection.pending && inspection.generation == generation)
                {
                    inspection.set_error(tr!("context_inspector.fetch_timed_out"));
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn refresh_context_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(session_id) = self.state.selected_session {
            self.fetch_context_breakdown(session_id, cx);
        }
    }

    /// Copy everything the inspector knows as JSON, for bug reports.
    fn copy_context_inspection(&mut self, cx: &mut Context<Self>) {
        let Some(inspection) = self
            .state
            .selected_session
            .and_then(|session_id| self.context_inspections.get(&session_id))
        else {
            return;
        };
        if let Ok(text) = serde_json::to_string_pretty(&inspection.recorded) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn toggle_context_inspector_row(&mut self, key: SharedString, cx: &mut Context<Self>) {
        let expanded = &mut self.context_panel.expanded;
        if !expanded.remove(&key) {
            expanded.insert(key);
        }
        cx.notify();
    }

    /// The Context tab's body for the selected task.
    pub(super) fn render_context_surface(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(session) = self.selected_session() else {
            return div().into_any_element();
        };
        let session_id = session.id;
        let provider = session.provider;
        // What the usage meter saved with the task: the headline when no
        // breakdown has been recorded.
        let saved_usage = session.context_usage;
        let running = self.runtimes.contains_key(&session_id);
        let theme = Theme::current(cx);
        // A tab restored with its task has not asked for the saved snapshot
        // yet. The read is off-thread and happens once per task.
        self.ensure_context_snapshot_loaded(session_id, cx);
        let inspection = self.context_inspections.get(&session_id);

        // Expandable rows need focus handles before the body borrows state.
        let mut toggle_keys = Vec::new();
        if let Some(inspection) = inspection {
            if let Some(breakdown) = &inspection.recorded.breakdown {
                for section in &breakdown.sections {
                    if section.rows.len() > SECTION_PREVIEW_ROWS {
                        toggle_keys.push(section_key(section));
                    }
                }
            }
            if inspection
                .recorded
                .manifest
                .as_ref()
                .is_some_and(|manifest| !manifest.appended_instructions.is_empty())
            {
                toggle_keys.push(SharedString::new_static(INSTRUCTIONS_KEY));
            }
            for index in 0..inspection.recorded.injections.len() {
                toggle_keys.push(injection_key(index));
            }
        }
        for key in toggle_keys {
            self.context_panel
                .toggle_focus
                .entry(key)
                .or_insert_with(|| cx.focus_handle());
        }
        let inspection = self.context_inspections.get(&session_id);
        let state = &self.context_panel;

        let pending = running && inspection.is_some_and(|inspection| inspection.pending);
        let model = inspection
            .and_then(|inspection| inspection.recorded.breakdown.as_ref())
            .and_then(|breakdown| breakdown.model.clone());
        let header_detail = match model {
            Some(model) => format!("{} · {model}", provider.display_name()),
            None => provider.display_name().to_owned(),
        };

        let mut body = div()
            .id("context-inspector-scroll")
            .track_focus(&state.body_focus)
            .tab_index(0)
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&state.scroll)
            .px(px(16.0))
            .pt(px(4.0))
            .pb(px(18.0))
            .flex()
            .flex_col()
            .gap(px(18.0))
            .text_size(sp(SMALL_UI_FONT_SIZE))
            .text_color(theme.text);

        let recorded = inspection.map(|inspection| &inspection.recorded);
        let has_record = recorded.is_some_and(|recorded| {
            recorded.breakdown.is_some()
                || recorded.manifest.is_some()
                || !recorded.injections.is_empty()
        });
        if !running {
            body = body.child(notice(
                &theme,
                "icons/info.svg",
                theme.text_secondary,
                if has_record || saved_usage.is_some() {
                    tr!("context_inspector.stopped_recorded")
                } else {
                    tr!("context_inspector.stopped_nothing")
                },
            ));
        }
        if let Some(error) = inspection.and_then(|inspection| inspection.error.as_ref()) {
            body = body.child(notice(
                &theme,
                "icons/alert.svg",
                theme.danger,
                tr!("context_inspector.fetch_error", error = error),
            ));
        }
        match recorded.and_then(|recorded| recorded.breakdown.as_ref()) {
            Some(breakdown) => {
                body = body.children(breakdown_view(breakdown, state, &theme, cx));
            }
            None => {
                if let Some(usage) = saved_usage {
                    body = body.child(saved_usage_view(usage, &theme));
                }
                if pending {
                    body = body.child(
                        div()
                            .text_color(theme.text_secondary)
                            .child(tr!("context_inspector.measuring")),
                    );
                } else if running && inspection.is_none_or(|inspection| inspection.error.is_none())
                {
                    body = body.child(notice(
                        &theme,
                        "icons/info.svg",
                        theme.text_secondary,
                        tr!("context_inspector.not_measured"),
                    ));
                }
            }
        }
        if let Some(inspection) = inspection {
            body = body.children(raku_additions_view(inspection, state, &theme, cx));
            if let Some(manifest) = &inspection.recorded.manifest {
                body = body.child(launch_view(manifest, &theme));
            }
        }

        let header = div()
            .h(px(40.0))
            .px(px(16.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(sp(12.0))
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(header_detail)),
            )
            .when(
                pending && inspection.is_some_and(|i| i.recorded.breakdown.is_some()),
                |row| {
                    row.child(
                        div()
                            .flex_none()
                            .text_size(sp(12.0))
                            .text_color(theme.text_tertiary)
                            .child(tr!("context_inspector.measuring")),
                    )
                },
            )
            .child(header_button(
                "context-inspector-refresh",
                &state.refresh_focus,
                "icons/rotate-cw.svg",
                tr!("context_inspector.refresh_plain"),
                running && !pending,
                &theme,
                cx,
                |raku, _, cx| raku.refresh_context_panel(cx),
            ))
            .child(header_button(
                "context-inspector-copy",
                &state.copy_focus,
                "icons/copy.svg",
                tr!("context_inspector.copy_json"),
                has_record,
                &theme,
                cx,
                |raku, _, cx| raku.copy_context_inspection(cx),
            ));

        let scroll = state.scroll.clone();
        let wheel_scroll = state.scroll.clone();
        div()
            .id("context-inspector-panel")
            .size_full()
            .flex()
            .flex_col()
            .on_key_down(move |event: &KeyDownEvent, _, cx| {
                if keyboard_scroll(&scroll, event) {
                    cx.stop_propagation();
                }
            })
            .child(header)
            .child(div().mx(px(12.0)).h(px(1.0)).flex_none().bg(theme.border))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .on_scroll_wheel(move |_, _, cx| contain_scroll(&wheel_scroll, cx))
                    .child(body)
                    .child(scrollbar::vertical(&state.scroll, &state.scrollbar)),
            )
            .into_any_element()
    }
}

const INSTRUCTIONS_KEY: &str = "raku:instructions";

fn section_key(section: &ContextSection) -> SharedString {
    SharedString::from(format!("section:{}", section.title))
}

fn injection_key(index: usize) -> SharedString {
    SharedString::from(format!("injection:{index}"))
}

/// Arrow, page and home/end keys scroll the inspector body. Returns whether
/// the key was consumed.
fn keyboard_scroll(scroll: &ScrollHandle, event: &KeyDownEvent) -> bool {
    if event.keystroke.modifiers.modified() {
        return false;
    }
    let offset = scroll.offset();
    let max = scroll.max_offset().y;
    let page = (scroll.bounds().size.height - px(SCROLL_STEP)).max(px(SCROLL_STEP));
    let target = match event.keystroke.key.as_str() {
        "down" => offset.y - px(SCROLL_STEP),
        "up" => offset.y + px(SCROLL_STEP),
        "pagedown" => offset.y - page,
        "pageup" => offset.y + page,
        "home" => px(0.0),
        "end" => -max,
        _ => return false,
    };
    scroll.set_offset(point(offset.x, target.clamp(-max, px(0.0))));
    true
}

#[allow(clippy::too_many_arguments)]
fn header_button(
    id: &'static str,
    focus: &FocusHandle,
    icon_path: &'static str,
    label: String,
    enabled: bool,
    theme: &Theme,
    cx: &mut Context<Raku>,
    activate: impl Fn(&mut Raku, &mut Window, &mut Context<Raku>) + 'static,
) -> Stateful<Div> {
    let color = if enabled {
        theme.text_secondary
    } else {
        theme.text_ghost
    };
    let button = div()
        .id(id)
        .track_focus(focus)
        .when(enabled, |button| button.tab_index(0))
        // A 28px target around a 14px glyph.
        .size(px(28.0))
        .flex_none()
        .rounded(px(7.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_default()
        .focus_visible(|style| style.border_1().border_color(theme.accent))
        .tooltip(Tooltip::text(label))
        .child(icon(icon_path, 14.0, color));
    if enabled {
        let hover = theme.overlay_strong;
        button
            .hover(move |style| style.bg(hover))
            .on_activation(cx, activate)
    } else {
        button
    }
}

fn notice(theme: &Theme, icon_path: &'static str, tint: Hsla, text: String) -> Div {
    div()
        .flex()
        .items_start()
        .gap(px(8.0))
        .px(px(12.0))
        .py(px(10.0))
        .rounded(px(9.0))
        .bg(theme.overlay)
        .child(div().pt(px(1.0)).child(icon(icon_path, 13.0, tint)))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .line_height(sp(17.0))
                .text_color(theme.text_secondary)
                .child(text),
        )
}

fn section_heading(theme: &Theme, title: String, trailing: Option<String>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .pb(px(6.0))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_color(theme.text_tertiary)
                .font_weight(FontWeight::MEDIUM)
                .child(title),
        )
        .children(trailing.map(|trailing| {
            div()
                .flex_none()
                .text_color(theme.text_tertiary)
                .child(trailing)
        }))
}

/// One label/value row: label truncates first so the numbers stay visible.
fn value_row(
    theme: &Theme,
    swatch: Option<Swatch>,
    label: String,
    detail: Option<String>,
    value: String,
) -> Div {
    div()
        .min_h(px(24.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .children(swatch.map(|swatch| swatch.render(theme)))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(div().min_w_0().flex_shrink(1.0).truncate().child(label))
                .children(detail.map(|detail| {
                    div()
                        .min_w_0()
                        .flex_shrink(1.0)
                        .truncate()
                        .text_color(theme.text_tertiary)
                        .child(detail)
                })),
        )
        .child(
            div()
                .flex_none()
                .text_color(theme.text_secondary)
                .child(value),
        )
}

#[derive(Clone, Copy)]
enum Swatch {
    Fill(Hsla),
    /// Out of the window: drawn hollow so it reads apart from the bar's parts.
    Hollow(Hsla),
}

impl Swatch {
    fn render(self, _theme: &Theme) -> Div {
        let base = div().size(px(9.0)).flex_none().rounded(px(2.0));
        match self {
            Self::Fill(color) => base.bg(color),
            Self::Hollow(color) => base.border_1().border_color(color),
        }
    }
}

/// The reference categorical palette, light and dark steps, in fixed order.
fn category_color(slot: usize, theme: &Theme) -> Hsla {
    const LIGHT: [u32; MAX_BAR_SLOTS] = [
        0x2a78d6, 0xeb6834, 0x1baf7a, 0xeda100, 0xe87ba4, 0x008300, 0x6250d6, 0xe34948,
    ];
    const DARK: [u32; MAX_BAR_SLOTS] = [
        0x3987e5, 0xd95926, 0x199e70, 0xc98500, 0xd55181, 0x008300, 0x9085e9, 0xe66767,
    ];
    let palette = if theme.is_dark { DARK } else { LIGHT };
    rgb(palette[slot.min(MAX_BAR_SLOTS - 1)]).into()
}

/// One part of the window bar.
struct BarPart {
    name: String,
    tokens: u64,
    color: Hsla,
}

/// Used categories take categorical slots in the provider's order; past the
/// eighth they fold into "Other". Free space and the compaction buffer are
/// neutral. Deferred rows are not in the window, so not in the bar.
fn bar_parts(categories: &[ContextCategory], theme: &Theme) -> Vec<BarPart> {
    let used: Vec<_> = categories
        .iter()
        .filter(|category| category.kind == ContextCategoryKind::Used && category.tokens > 0)
        .collect();
    let mut parts = Vec::new();
    let fold = used.len() > MAX_BAR_SLOTS;
    let named = if fold { MAX_BAR_SLOTS - 1 } else { used.len() };
    for (slot, category) in used.iter().take(named).enumerate() {
        parts.push(BarPart {
            name: category.name.clone(),
            tokens: category.tokens,
            color: category_color(slot, theme),
        });
    }
    if fold {
        parts.push(BarPart {
            name: tr!("context_inspector.other"),
            tokens: used[named..].iter().map(|category| category.tokens).sum(),
            color: category_color(MAX_BAR_SLOTS - 1, theme),
        });
    }
    for category in categories {
        let color = match category.kind {
            ContextCategoryKind::Buffer => theme.text_ghost,
            ContextCategoryKind::Free => theme.overlay_strong,
            _ => continue,
        };
        if category.tokens > 0 {
            parts.push(BarPart {
                name: category.name.clone(),
                tokens: category.tokens,
                color,
            });
        }
    }
    parts
}

fn percent_of(tokens: u64, whole: u64) -> String {
    if whole == 0 {
        return String::new();
    }
    let percent = tokens as f64 * 100.0 / whole as f64;
    if percent > 0.0 && percent < 1.0 {
        "<1%".to_owned()
    } else {
        format!("{percent:.0}%")
    }
}

fn measured_ago(measured_at_ms: u64) -> String {
    let now = unix_time_millis();
    let seconds = now.saturating_sub(measured_at_ms) / 1000;
    if seconds < 5 {
        tr!("context_inspector.measured_now")
    } else if seconds < 60 {
        tr!("context_inspector.measured_seconds", seconds = seconds)
    } else {
        tr!("context_inspector.measured_minutes", minutes = seconds / 60)
    }
}

fn breakdown_view(
    breakdown: &ContextBreakdown,
    state: &ContextInspectorState,
    theme: &Theme,
    cx: &mut Context<Raku>,
) -> Vec<AnyElement> {
    let mut views = Vec::new();

    // Headline: occupancy against the window.
    let headline = match (breakdown.total_tokens, breakdown.window) {
        (Some(total), Some(window)) => tr!(
            "context_inspector.headline_window",
            used = format_tokens(total),
            window = format_tokens(window),
            percent = percent_of(total.min(window), window)
        ),
        (Some(total), None) => tr!("context_inspector.headline", used = format_tokens(total)),
        (None, Some(window)) => tr!(
            "context_inspector.headline_unknown",
            window = format_tokens(window)
        ),
        (None, None) => tr!("context_inspector.headline_none"),
    };
    let mut summary = div().flex().flex_col().gap(px(4.0)).child(
        div()
            .flex()
            .items_baseline()
            .gap(px(10.0))
            .child(
                div()
                    .text_size(sp(18.0))
                    .font_weight(FontWeight::MEDIUM)
                    .child(headline),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex_none()
                    .text_color(theme.text_tertiary)
                    .child(measured_ago(breakdown.measured_at_ms)),
            ),
    );
    if let Some(threshold) = breakdown.auto_compact_threshold {
        summary = summary.child(div().text_color(theme.text_secondary).child(tr!(
            "context_inspector.auto_compact_at",
            tokens = format_tokens(threshold)
        )));
    }
    views.push(summary.into_any_element());

    // The window bar and its legend, which doubles as the table view.
    if !breakdown.categories.is_empty() {
        let parts = bar_parts(&breakdown.categories, theme);
        let whole = breakdown
            .window
            .unwrap_or_else(|| parts.iter().map(|part| part.tokens).sum());
        let mut bar = div()
            .id("context-inspector-bar")
            .h(px(8.0))
            .w_full()
            .flex()
            .gap(px(2.0));
        for (index, part) in parts.iter().enumerate() {
            if part.tokens == 0 {
                continue;
            }
            let label = format!(
                "{} · {} · {}",
                part.name,
                format_tokens(part.tokens),
                percent_of(part.tokens, whole)
            );
            let mut segment = div()
                .id(("context-inspector-bar-part", index))
                .h_full()
                .min_w(px(2.0))
                .flex_basis(px(0.0))
                .flex_shrink(1.0)
                .rounded_full()
                .bg(part.color)
                .tooltip(Tooltip::text(label));
            // Grow weights are the token shares, so widths stay proportional.
            segment.style().flex_grow = Some(part.tokens as f32);
            bar = bar.child(segment);
        }
        let mut legend = div().flex().flex_col();
        for part in &parts {
            legend = legend.child(value_row(
                theme,
                Some(Swatch::Fill(part.color)),
                part.name.clone(),
                None,
                format!(
                    "{}  {}",
                    format_tokens(part.tokens),
                    percent_of(part.tokens, whole)
                ),
            ));
        }
        let deferred: Vec<_> = breakdown
            .categories
            .iter()
            .filter(|category| category.kind == ContextCategoryKind::Deferred)
            .collect();
        for category in deferred {
            legend = legend.child(value_row(
                theme,
                Some(Swatch::Hollow(theme.text_tertiary)),
                category.name.clone(),
                Some(tr!("context_inspector.not_in_window")),
                format_tokens(category.tokens),
            ));
        }
        views.push(
            div()
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(bar)
                .child(legend)
                .into_any_element(),
        );
    }

    // The latest model call's counters.
    let split = breakdown.token_split;
    if !split.is_empty() {
        let mut rows = div().flex().flex_col().child(section_heading(
            theme,
            tr!("context_inspector.section_last_call"),
            None,
        ));
        for (label, value) in [
            ("context_inspector.split_input", split.input),
            ("context_inspector.split_cache_read", split.cache_read),
            ("context_inspector.split_cache_write", split.cache_write),
            ("context_inspector.split_output", split.output),
            ("context_inspector.split_reasoning", split.reasoning),
        ] {
            if let Some(value) = value {
                rows = rows.child(value_row(
                    theme,
                    None,
                    tr!(label),
                    None,
                    format_tokens(value),
                ));
            }
        }
        views.push(rows.into_any_element());
    }

    for (index, section) in breakdown.sections.iter().enumerate() {
        let key = section_key(section);
        let expanded = state.expanded.contains(&key);
        let total = section.total_tokens();
        let mut rows = div().flex().flex_col().child(section_heading(
            theme,
            section.title.clone(),
            (total > 0).then(|| format_tokens(total)),
        ));
        let shown = if expanded {
            section.rows.len()
        } else {
            section.rows.len().min(SECTION_PREVIEW_ROWS)
        };
        for row in &section.rows[..shown] {
            let value = if row.tokens == 0 && total == 0 {
                String::new()
            } else {
                format_tokens(row.tokens)
            };
            rows = rows.child(value_row(
                theme,
                None,
                row.label.clone(),
                row.detail.clone(),
                value,
            ));
        }
        if section.rows.len() > SECTION_PREVIEW_ROWS
            && let Some(focus) = state.toggle_focus.get(&key)
        {
            let label = if expanded {
                tr!("context_inspector.show_fewer")
            } else {
                tr!("context_inspector.show_all", count = section.rows.len())
            };
            rows = rows.child(toggle_row(
                ("context-inspector-section-toggle", index),
                focus,
                label,
                expanded,
                key,
                theme,
                cx,
            ));
        }
        views.push(rows.into_any_element());
    }
    views
}

fn toggle_row(
    id: impl Into<gpui::ElementId>,
    focus: &FocusHandle,
    label: String,
    expanded: bool,
    key: SharedString,
    theme: &Theme,
    cx: &mut Context<Raku>,
) -> Stateful<Div> {
    let hover = theme.overlay;
    div()
        .id(id)
        .track_focus(focus)
        .tab_index(0)
        .mt(px(2.0))
        .h(px(26.0))
        .px(px(6.0))
        .ml(px(-6.0))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .gap(px(5.0))
        .cursor_default()
        .text_color(theme.text_secondary)
        .hover(move |style| style.bg(hover))
        .focus_visible(|style| style.border_1().border_color(theme.accent))
        .child(icon(
            if expanded {
                "icons/chevron-down.svg"
            } else {
                "icons/chevron-right.svg"
            },
            11.0,
            theme.text_tertiary,
        ))
        .child(label)
        .on_activation(cx, move |raku, _, cx| {
            raku.toggle_context_inspector_row(key.clone(), cx)
        })
}

/// The headline a stopped task can still show from the usage its meter
/// saved, before any breakdown has been recorded.
fn saved_usage_view(usage: ContextUsage, theme: &Theme) -> Div {
    let headline = match usage.window.filter(|window| *window > 0) {
        Some(window) => tr!(
            "context_inspector.headline_window",
            used = format_tokens(usage.tokens.min(window)),
            window = format_tokens(window),
            percent = percent_of(usage.tokens.min(window), window)
        ),
        None => tr!(
            "context_inspector.headline",
            used = format_tokens(usage.tokens)
        ),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .text_size(sp(18.0))
                .font_weight(FontWeight::MEDIUM)
                .child(headline),
        )
        .child(
            div()
                .text_color(theme.text_tertiary)
                .child(tr!("context_inspector.saved_usage")),
        )
}

fn mono_block(theme: &Theme, text: String) -> Div {
    div()
        .w_full()
        .px(px(10.0))
        .py(px(8.0))
        .rounded(px(8.0))
        .bg(theme.code_wash)
        .font_family(md::render::MONO_FAMILY)
        .text_size(sp(12.0))
        .line_height(sp(18.0))
        .text_color(theme.text_secondary)
        .child(text)
}

/// What Raku itself adds on top of the provider's own context.
fn raku_additions_view(
    inspection: &ContextInspection,
    state: &ContextInspectorState,
    theme: &Theme,
    cx: &mut Context<Raku>,
) -> Option<AnyElement> {
    let manifest = inspection.recorded.manifest.as_ref();
    let instructions = manifest
        .map(|manifest| manifest.appended_instructions.as_str())
        .filter(|text| !text.is_empty());
    let tools = manifest
        .map(|manifest| manifest.raku_tools.as_slice())
        .unwrap_or_default();
    if instructions.is_none() && tools.is_empty() && inspection.recorded.injections.is_empty() {
        return None;
    }
    let mut view = div().flex().flex_col().gap(px(4.0)).child(section_heading(
        theme,
        tr!("context_inspector.section_raku"),
        None,
    ));
    if let Some(instructions) = instructions {
        let key = SharedString::new_static(INSTRUCTIONS_KEY);
        let expanded = state.expanded.contains(&key);
        view = view.child(value_row(
            theme,
            None,
            tr!("context_inspector.appended_instructions"),
            Some(tr!(
                "context_inspector.chars",
                count = instructions.chars().count()
            )),
            tr!(
                "context_inspector.estimate",
                tokens = format_tokens(estimate_tokens(instructions))
            ),
        ));
        if let Some(focus) = state.toggle_focus.get(&key) {
            view = view.child(toggle_row(
                "context-inspector-instructions-toggle",
                focus,
                if expanded {
                    tr!("context_inspector.hide_text")
                } else {
                    tr!("context_inspector.show_text")
                },
                expanded,
                key,
                theme,
                cx,
            ));
        }
        if expanded {
            view = view.child(mono_block(theme, instructions.to_owned()));
        }
    }
    if !tools.is_empty() {
        view = view.child(value_row(
            theme,
            None,
            tr!("context_inspector.raku_tools"),
            None,
            tools.join(", "),
        ));
    }
    for (index, injection) in inspection.recorded.injections.iter().enumerate().rev() {
        let key = injection_key(index);
        let expanded = state.expanded.contains(&key);
        view = view.child(value_row(
            theme,
            None,
            injection.label.clone(),
            Some(tr!("context_inspector.injected")),
            tr!(
                "context_inspector.estimate",
                tokens = format_tokens(estimate_tokens(&injection.text))
            ),
        ));
        if let Some(focus) = state.toggle_focus.get(&key) {
            view = view.child(toggle_row(
                ("context-inspector-injection-toggle", index),
                focus,
                if expanded {
                    tr!("context_inspector.hide_text")
                } else {
                    tr!("context_inspector.show_text")
                },
                expanded,
                key,
                theme,
                cx,
            ));
        }
        if expanded {
            view = view.child(mono_block(theme, injection.text.clone()));
        }
    }
    Some(view.into_any_element())
}

fn launch_view(manifest: &LaunchManifest, theme: &Theme) -> AnyElement {
    let command = std::iter::once(manifest.program.as_str())
        .chain(manifest.args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ");
    let mut view = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(section_heading(
            theme,
            tr!("context_inspector.section_launch"),
            None,
        ))
        .child(mono_block(theme, command));
    if !manifest.env.is_empty() {
        let env = manifest
            .env
            .iter()
            .map(|(key, value)| format!("{key}={}", shell_quote(value)))
            .collect::<Vec<_>>()
            .join("\n");
        view = view
            .child(
                div()
                    .pt(px(4.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!("context_inspector.environment")),
            )
            .child(mono_block(theme, env));
    }
    view.into_any_element()
}

/// Quote an argument only when a shell would split or expand it.
fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c))
    {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(program: &str) -> LaunchManifest {
        LaunchManifest {
            program: program.into(),
            ..Default::default()
        }
    }

    fn breakdown(total: u64) -> ContextBreakdown {
        ContextBreakdown {
            detail: ContextDetail::Full,
            model: None,
            measured_at_ms: 0,
            total_tokens: Some(total),
            window: None,
            auto_compact_threshold: None,
            categories: Vec::new(),
            sections: Vec::new(),
            token_split: Default::default(),
        }
    }

    #[test]
    fn a_new_launch_drops_the_old_process_injections() {
        let mut inspection = ContextInspection::default();
        inspection.set_manifest(manifest("first"));
        inspection.record_injection(PromptInjection {
            label: "note".into(),
            text: "x".into(),
        });
        inspection.set_manifest(manifest("second"));
        assert!(inspection.recorded.injections.is_empty());
        assert_eq!(inspection.recorded.manifest.unwrap().program, "second");
    }

    #[test]
    fn a_saved_snapshot_never_overwrites_live_data() {
        let mut inspection = ContextInspection::default();
        inspection.set_breakdown(breakdown(5));
        inspection.merge_saved(ContextSnapshot {
            manifest: Some(manifest("saved")),
            injections: VecDeque::new(),
            breakdown: Some(breakdown(1)),
        });
        assert_eq!(inspection.recorded.breakdown.unwrap().total_tokens, Some(5));
        assert_eq!(inspection.recorded.manifest.unwrap().program, "saved");
    }

    #[test]
    fn a_live_launch_keeps_the_saved_breakdown_without_restoring_old_injections() {
        let mut inspection = ContextInspection::default();
        inspection.set_manifest(manifest("new"));
        inspection.merge_saved(ContextSnapshot {
            manifest: Some(manifest("old")),
            injections: VecDeque::from([PromptInjection {
                label: "old prompt".into(),
                text: "old text".into(),
            }]),
            breakdown: Some(breakdown(42)),
        });
        assert_eq!(inspection.recorded.manifest.unwrap().program, "new");
        assert!(inspection.recorded.injections.is_empty());
        assert_eq!(
            inspection.recorded.breakdown.unwrap().total_tokens,
            Some(42)
        );
    }

    #[test]
    fn stopping_a_measurement_keeps_the_last_recorded_context() {
        let mut inspection = ContextInspection::default();
        inspection.set_manifest(manifest("claude"));
        inspection.set_breakdown(breakdown(42));
        inspection.record_injection(PromptInjection {
            label: "note".into(),
            text: "recorded text".into(),
        });
        inspection.pending = true;
        inspection.generation = 3;
        inspection.stop_fetch();
        assert!(!inspection.pending);
        assert_eq!(inspection.generation, 4);
        assert_eq!(inspection.recorded.manifest.unwrap().program, "claude");
        assert_eq!(
            inspection.recorded.breakdown.unwrap().total_tokens,
            Some(42)
        );
        assert_eq!(inspection.recorded.injections[0].text, "recorded text");
    }

    #[gpui::test]
    fn snapshot_writes_finish_in_order_even_after_the_writer_is_dropped(
        cx: &mut gpui::TestAppContext,
    ) {
        let path = std::env::temp_dir()
            .join(format!("raku-context-{}", Uuid::new_v4()))
            .join("snapshot.json");
        let (release, wait) = futures::channel::oneshot::channel::<()>();
        let mut writer = ContextSnapshotWriter {
            pending: Some(cx.background_executor.spawn(async move {
                let _ = wait.await;
            })),
        };
        for total in [1, 42] {
            writer.save(
                path.clone(),
                ContextSnapshot {
                    breakdown: Some(breakdown(total)),
                    ..Default::default()
                },
                &cx.background_executor,
            );
        }
        drop(writer);
        cx.run_until_parked();
        assert!(!path.exists());
        release.send(()).unwrap();
        cx.run_until_parked();
        let saved: ContextSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.breakdown.unwrap().total_tokens, Some(42));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn snapshots_round_trip_through_their_file() {
        let path = std::env::temp_dir()
            .join(format!("raku-context-{}", Uuid::new_v4()))
            .join("snapshot.json");
        let snapshot = ContextSnapshot {
            manifest: Some(manifest("claude")),
            injections: VecDeque::from([PromptInjection {
                label: "title".into(),
                text: "language".into(),
            }]),
            breakdown: Some(breakdown(42)),
        };
        write_context_snapshot(&path, &snapshot).unwrap();
        let read: ContextSnapshot = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(read.manifest, snapshot.manifest);
        assert_eq!(read.injections, snapshot.injections);
        assert_eq!(read.breakdown, snapshot.breakdown);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn injections_keep_only_the_newest() {
        let mut inspection = ContextInspection::default();
        for index in 0..MAX_INJECTIONS + 3 {
            inspection.record_injection(PromptInjection {
                label: index.to_string(),
                text: String::new(),
            });
        }
        assert_eq!(inspection.recorded.injections.len(), MAX_INJECTIONS);
        assert_eq!(inspection.recorded.injections.front().unwrap().label, "3");
    }

    #[test]
    fn a_breakdown_clears_the_error_and_pending_state() {
        let mut inspection = ContextInspection {
            pending: true,
            error: Some("old".into()),
            ..Default::default()
        };
        inspection.set_breakdown(ContextBreakdown {
            detail: ContextDetail::Full,
            model: None,
            measured_at_ms: 0,
            total_tokens: Some(1),
            window: None,
            auto_compact_threshold: None,
            categories: Vec::new(),
            sections: Vec::new(),
            token_split: Default::default(),
        });
        assert!(!inspection.pending);
        assert!(inspection.error.is_none());
    }

    #[test]
    fn shell_quote_leaves_plain_args_alone() {
        assert_eq!(shell_quote("--model"), "--model");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn percent_marks_slivers_instead_of_rounding_to_zero() {
        assert_eq!(percent_of(1, 1000), "<1%");
        assert_eq!(percent_of(0, 1000), "0%");
        assert_eq!(percent_of(500, 1000), "50%");
        assert_eq!(percent_of(5, 0), "");
    }
}
