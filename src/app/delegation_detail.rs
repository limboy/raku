//! Session-scoped delegated transcripts, using the main transcript's renderers.

use std::time::Duration;

use waku_client::delegation::{
    DelegatedTaskEvent, DelegatedTaskEvents, DelegatedTaskStatus, DelegatedTaskSummary,
    DelegationGroupInfo,
};

use super::streaming::{
    append_session_reasoning, append_session_reasoning_block, complete_session_reasoning,
    finish_session_messages, finish_session_reasoning_block, start_session_reasoning,
    update_session_activity,
};
use super::transcript_view::{ActivityClusterPart, ActivityRenderContext};
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DetailIdentity {
    session_id: Uuid,
    group_id: Uuid,
    task_id: Uuid,
}

const DETAIL_POLL_INTERVAL: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DetailRow {
    Message(usize),
    Activities(usize),
    Activity(usize, usize),
}

struct DetailOutput {
    session: AgentSession,
    phase: Option<StreamPhase>,
    cursor: u64,
    expected_attempt: Option<Uuid>,
    attempt: Option<Uuid>,
}

enum DetailEventRead {
    Replay {
        output: DetailOutput,
        has_more: bool,
    },
    Page(DelegatedTaskEvents),
    Failed {
        replay: Option<DetailOutput>,
    },
}

impl DetailEventRead {
    fn from_page(page: DelegatedTaskEvents, replay: Option<DetailOutput>) -> Self {
        if let Some(mut output) = replay {
            let has_more = page.has_more;
            output.append_page(page);
            Self::Replay { output, has_more }
        } else {
            Self::Page(page)
        }
    }
}

struct DetailReadProgress {
    replay: Option<DetailOutput>,
    has_more: bool,
    notify: bool,
}

impl Default for DetailOutput {
    fn default() -> Self {
        Self {
            session: AgentSession::new(Uuid::nil(), ProviderKind::Codex),
            phase: None,
            cursor: 0,
            expected_attempt: None,
            attempt: None,
        }
    }
}

impl DetailOutput {
    fn append_page(&mut self, page: DelegatedTaskEvents) -> bool {
        let mut changed = false;
        for event in page.events {
            if event.sequence <= self.cursor {
                continue;
            }
            self.cursor = event.sequence;
            changed |= self.append_event(event);
        }
        self.cursor = self.cursor.max(page.next_sequence);
        changed
    }

    fn append_event(&mut self, event: DelegatedTaskEvent) -> bool {
        if event.event.kind == "delegationAttemptStarted" {
            let Some(attempt) = event
                .event
                .payload
                .get("attemptId")
                .and_then(serde_json::Value::as_str)
                .and_then(|id| Uuid::parse_str(id).ok())
            else {
                return false;
            };
            if self
                .expected_attempt
                .is_some_and(|expected| expected != attempt)
            {
                return false;
            }
            self.session = AgentSession::new(Uuid::nil(), ProviderKind::Codex);
            self.phase = None;
            self.attempt = Some(attempt);
            return true;
        }
        if self.expected_attempt.is_some() && self.attempt != self.expected_attempt {
            return false;
        }
        let timestamp_ms = event.created_at_ms;
        let Ok(event) = waku_client::event_from_wire(event.event) else {
            return false;
        };
        match event {
            DriverEvent::TextDelta(delta) if !delta.is_empty() => {
                if self.phase == Some(StreamPhase::Reasoning) {
                    complete_session_reasoning(&mut self.session);
                }
                let id = self.session.id;
                append_text_delta_to_session(
                    std::slice::from_mut(&mut self.session),
                    id,
                    self.phase == Some(StreamPhase::Text),
                    delta,
                );
                self.phase = Some(StreamPhase::Text);
            }
            DriverEvent::ReasoningDelta(delta) => {
                if !append_session_reasoning(&mut self.session, self.phase, delta, timestamp_ms) {
                    return false;
                }
                self.phase = Some(StreamPhase::Reasoning);
            }
            DriverEvent::ReasoningStarted { id, timestamp_ms } => {
                start_session_reasoning(&mut self.session, self.phase, id, timestamp_ms);
                self.phase = Some(StreamPhase::Reasoning);
            }
            DriverEvent::ReasoningBlockDelta { id, delta } => {
                append_session_reasoning_block(
                    &mut self.session,
                    self.phase,
                    id,
                    delta,
                    timestamp_ms,
                );
                self.phase = Some(StreamPhase::Reasoning);
            }
            DriverEvent::ReasoningCompleted {
                id,
                timestamp_ms,
                content,
            } => {
                finish_session_reasoning_block(
                    &mut self.session,
                    self.phase,
                    id,
                    content,
                    timestamp_ms,
                );
            }
            DriverEvent::RichActivity(mut item) => {
                // Provider IDs are not UI identities across isolated task panes.
                item.id = Uuid::new_v4();
                let (_, inserted) = update_session_activity(&mut self.session, self.phase, item);
                if inserted {
                    self.phase = Some(StreamPhase::Activity);
                }
            }
            DriverEvent::Activity {
                id,
                kind,
                title,
                detail,
                complete,
            } => {
                let (_, inserted) = update_session_activity(
                    &mut self.session,
                    self.phase,
                    ActivityItem::new(id, kind, title, detail, complete),
                );
                if inserted {
                    self.phase = Some(StreamPhase::Activity);
                }
            }
            _ => return false,
        }
        true
    }

    fn settle(&mut self, conclusion: Option<&str>) {
        // The persisted conclusion is often the aggregate of *all* text deltas,
        // including commentary separated by tools/thinking. Keep those ordered
        // messages rather than replacing them with an aggregate Markdown blob.
        if let Some(conclusion) = conclusion.filter(|text| !text.trim().is_empty()) {
            let aggregate = self
                .session
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<String>();
            let last = self.session.messages.last();
            let displayed = aggregate.trim();
            let conclusion_text = conclusion.trim();
            let suffix_is_displayed =
                displayed
                    .strip_suffix(conclusion_text)
                    .is_some_and(|prefix| {
                        prefix.is_empty() || prefix.chars().last().is_some_and(char::is_whitespace)
                    });
            if !suffix_is_displayed
                && last.is_none_or(|message| message.content.trim() != conclusion_text)
            {
                self.session
                    .messages
                    .push(Message::new(MessageRole::Assistant, conclusion));
            }
        }
        finish_session_messages(&mut self.session);
        for block in &mut self.session.transcript_blocks {
            for activity in &mut block.activities {
                activity.complete = true;
            }
        }
        self.phase = None;
    }

    fn row_plan(&self) -> Vec<DetailRow> {
        let mut rows =
            Vec::with_capacity(self.session.messages.len() + self.session.transcript_blocks.len());
        let mut blocks = self.session.transcript_blocks.iter().enumerate().peekable();
        for index in 0..=self.session.messages.len() {
            while blocks
                .peek()
                .is_some_and(|(_, block)| block.after_message <= index)
            {
                rows.push(DetailRow::Activities(blocks.next().unwrap().0));
            }
            // Providers may emit only separator newlines between tool calls.
            // Keep their message indices for activity ordering, but don't give
            // invisible prose a selectable text row and its padding.
            if self
                .session
                .messages
                .get(index)
                .is_some_and(|message| !message.visible_content().trim().is_empty())
            {
                rows.push(DetailRow::Message(index));
            }
        }
        rows
    }
}

/// Only the visible task owns selection, disclosure and viewport geometry.
pub(super) struct DelegationDetailState {
    identity: DetailIdentity,
    scroll_id: SharedString,
    selection: TranscriptSelection,
    rows: Vec<DetailRow>,
    list: ListState,
    scrollbar: Rc<ScrollbarState>,
    focus: FocusHandle,
    pending_focus: bool,
    generation: Uuid,
    polling: bool,
    drained: bool,
    history_loaded: bool,
    output: DetailOutput,
    document_dirty: bool,
    rendered_terminal: bool,
    rendered_live: bool,
    rendered_conclusion: Option<String>,
    has_content: bool,
    groups_expanded: HashMap<usize, bool>,
    items_expanded: HashMap<Uuid, bool>,
    message_markdown: RefCell<HashMap<Uuid, MarkdownView>>,
    activity_markdown: RefCell<HashMap<Uuid, MarkdownView>>,
    reasoning_starts: RefCell<HashMap<Uuid, usize>>,
    activity_viewports: RefCell<HashMap<Uuid, ActivityScrollViewport>>,
}

impl DelegationDetailState {
    fn new(identity: DetailIdentity, cx: &mut App) -> Self {
        let list = ListState::new(0, ListAlignment::Top, px(512.0));
        list.set_follow_mode(gpui::FollowMode::Tail);
        list.set_scroll_handler(|_, window, _| window.refresh());
        Self {
            identity,
            scroll_id: SharedString::from(format!("delegation-detail-{}", identity.task_id)),
            selection: TranscriptSelection::default(),
            rows: Vec::new(),
            list,
            scrollbar: ScrollbarState::new(),
            focus: cx.focus_handle(),
            pending_focus: true,
            generation: Uuid::new_v4(),
            polling: false,
            drained: false,
            history_loaded: false,
            output: DetailOutput::default(),
            document_dirty: true,
            rendered_terminal: false,
            rendered_live: false,
            rendered_conclusion: None,
            has_content: false,
            groups_expanded: HashMap::new(),
            items_expanded: HashMap::new(),
            message_markdown: RefCell::new(HashMap::new()),
            activity_markdown: RefCell::new(HashMap::new()),
            reasoning_starts: RefCell::new(HashMap::new()),
            activity_viewports: RefCell::new(HashMap::new()),
        }
    }

    fn apply_read(
        &mut self,
        read: DetailEventRead,
        task: Option<&DelegatedTaskSummary>,
        terminal_at_request: bool,
    ) -> DetailReadProgress {
        let (has_more, changed) = match read {
            DetailEventRead::Replay {
                output,
                has_more: true,
            } => {
                self.polling = true;
                return DetailReadProgress {
                    replay: Some(output),
                    has_more: true,
                    notify: false,
                };
            }
            DetailEventRead::Replay {
                output,
                has_more: false,
            } => {
                self.output = output;
                self.history_loaded = true;
                (false, true)
            }
            DetailEventRead::Page(page) => (page.has_more, self.output.append_page(page)),
            // A transient failure must retain the unpublished replay cursor.
            DetailEventRead::Failed { replay } => {
                self.polling = true;
                return DetailReadProgress {
                    replay,
                    has_more: false,
                    notify: false,
                };
            }
        };
        // A read begun before settlement can miss events appended before the
        // terminal projection arrived. Only a read begun after observing that
        // projection can establish the final drained cursor.
        let drained =
            terminal_at_request && task.is_some_and(|task| task.status.is_terminal()) && !has_more;
        let drained_changed = self.drained != drained;
        self.drained = drained;
        self.document_dirty |= changed || drained_changed;
        if let Some(task) = task {
            self.sync_document(task);
        }
        self.polling = !drained;
        DetailReadProgress {
            replay: None,
            has_more,
            notify: changed || drained_changed,
        }
    }

    fn sync_document(&mut self, task: &DelegatedTaskSummary) {
        // Initial history is reduced in the background and published only
        // once caught up. Old thoughts must not replay as live, open peeks.
        if !self.history_loaded {
            return;
        }
        let terminal = task.status.is_terminal() && self.drained;
        let live = task.status == DelegatedTaskStatus::Running;
        let conclusion = terminal
            .then_some(task.final_conclusion.as_deref())
            .flatten();
        if !self.document_dirty
            && terminal == self.rendered_terminal
            && live == self.rendered_live
            && conclusion == self.rendered_conclusion.as_deref()
        {
            return;
        }
        if terminal {
            self.output.settle(conclusion);
        }
        self.rendered_terminal = terminal;
        self.rendered_live = live;
        self.sync_rows();
        self.has_content = !self.rows.is_empty();
        self.rendered_conclusion = conclusion.map(str::to_owned);
        self.document_dirty = false;
    }

    fn live_group(&self, index: usize) -> bool {
        activity_group_is_live(
            self.rendered_live,
            index + 1 == self.output.session.transcript_blocks.len(),
            self.output.session.transcript_blocks[index].after_message,
            self.output.session.messages.len(),
        )
    }

    fn live_reasoning_id(&self, block: usize) -> Option<Uuid> {
        self.live_group(block)
            .then(|| {
                self.output.session.transcript_blocks[block]
                    .activities
                    .iter()
                    .rev()
                    .find(|activity| activity.reasoning.is_some() && !activity.complete)
                    .map(|activity| activity.id)
            })
            .flatten()
    }

    fn sync_rows(&mut self) {
        let mut rows = Vec::new();
        for row in self.output.row_plan() {
            rows.push(row);
            if let DetailRow::Activities(block) = row
                && self
                    .groups_expanded
                    .get(&block)
                    .copied()
                    .unwrap_or(self.live_group(block))
            {
                rows.extend(
                    (0..self.output.session.transcript_blocks[block]
                        .activities
                        .len())
                        .map(|index| DetailRow::Activity(block, index)),
                );
            }
        }
        // Keep stable rows on both sides of a disclosure change. Replacing an
        // unchanged row resets an offset inside it, so content updates use
        // remeasurement instead of splicing the reader back to a row's top.
        let common = self
            .rows
            .iter()
            .zip(&rows)
            .take_while(|(a, b)| a == b)
            .count();
        let suffix = self.rows[common..]
            .iter()
            .rev()
            .zip(rows[common..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        if common + suffix < self.rows.len() || common + suffix < rows.len() {
            self.list.splice(
                common..self.rows.len() - suffix,
                rows.len() - common - suffix,
            );
        }
        self.rows = rows;
        // An update can also replace an earlier tool snapshot in-place.
        self.list.remeasure_items(0..self.rows.len());
    }
}

/// Snapshot paths cannot be resolved against the parent's workspace. External
/// links remain usable; local file actions are deliberately not installed.
pub(super) fn safe_detail_link_handler() -> md::render::LinkHandler {
    thread_local! {
        static HANDLER: md::render::LinkHandler = Rc::new(|target, _, cx| {
            if target.starts_with("https://") || target.starts_with("http://")
                || target.starts_with("mailto:")
            {
                cx.open_url(target);
            }
        });
    }
    HANDLER.with(Clone::clone)
}

fn cached_task(
    sessions: &HashMap<Uuid, Vec<DelegationGroupInfo>>,
    identity: DetailIdentity,
) -> Option<&DelegatedTaskSummary> {
    sessions
        .get(&identity.session_id)?
        .iter()
        .find(|group| {
            group.id == identity.group_id && group.parent_session_id == identity.session_id
        })?
        .tasks
        .iter()
        .find(|task| task.task_id == identity.task_id)
}

impl Waku {
    pub(super) fn open_delegated_task(
        &mut self,
        group_id: Uuid,
        task_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.selected_session().map(|session| session.id) else {
            return;
        };
        let identity = DetailIdentity {
            session_id,
            group_id,
            task_id,
        };
        let Some(task) = cached_task(&self.session_delegations, identity) else {
            return;
        };
        let title = task.target.display_label.clone();
        self.open_right_panel_surface(
            RightPanelSurface::DelegatedTask {
                group_id,
                task_id,
                title,
            },
            cx,
        );
        self.focus_active_delegation_detail(window, cx);
    }

    fn ensure_delegation_detail(&self, identity: DetailIdentity, cx: &mut Context<Self>) {
        let attempt =
            cached_task(&self.session_delegations, identity).and_then(|task| task.attempt_id);
        let mut cache = self.delegation_detail.borrow_mut();
        if !matches!(cache.as_ref(), Some(state) if state.identity == identity && state.output.expected_attempt == attempt)
        {
            let focus = cache
                .as_ref()
                .filter(|state| state.identity == identity)
                .map(|state| (state.focus.clone(), state.pending_focus));
            let mut state = DelegationDetailState::new(identity, cx);
            state.output.expected_attempt = attempt;
            if let Some((focus, pending)) = focus {
                state.focus = focus;
                state.pending_focus = pending;
            }
            *cache = Some(state);
        }
    }

    fn active_delegation_detail_identity(&self) -> Option<DetailIdentity> {
        if !self.right_panel_visible {
            return None;
        }
        let session_id = self.selected_session()?.id;
        let RightPanelSurface::DelegatedTask {
            group_id, task_id, ..
        } = self
            .right_panel_active_surface
            .and_then(|index| self.right_panel_surfaces.get(index))?
        else {
            return None;
        };
        Some(DetailIdentity {
            session_id,
            group_id: *group_id,
            task_id: *task_id,
        })
    }

    /// Started only by opening/focusing a detail or refreshing its projection.
    /// All blocking IPC runs on the background executor, never in render.
    pub(super) fn resume_active_delegation_detail(&self, cx: &mut Context<Self>) {
        let Some(identity) = self.active_delegation_detail_identity() else {
            return;
        };
        let Some(task) = cached_task(&self.session_delegations, identity) else {
            return;
        };
        self.ensure_delegation_detail(identity, cx);
        let (generation, mut replay) = {
            let mut cache = self.delegation_detail.borrow_mut();
            let state = cache.as_mut().expect("detail state ensured above");
            state.sync_document(task);
            if state.polling || (state.drained && task.status.is_terminal()) {
                return;
            }
            state.polling = true;
            let replay = (!state.history_loaded).then(|| DetailOutput {
                expected_attempt: state.output.expected_attempt,
                ..Default::default()
            });
            (state.generation, replay)
        };
        let client = self.daemon.client();
        let entity = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            loop {
                let replay_cursor = replay.as_ref().map(|output| output.cursor);
                let cursor = entity.update(cx, |waku, _| {
                    let active = waku.active_delegation_detail_identity() == Some(identity);
                    let terminal = cached_task(&waku.session_delegations, identity)
                        .is_some_and(|task| task.status.is_terminal());
                    let mut cache = waku.delegation_detail.borrow_mut();
                    let state = cache.as_mut().filter(|state| {
                        state.identity == identity && state.generation == generation
                    })?;
                    if !active {
                        state.polling = false;
                        return None;
                    }
                    Some((
                        replay_cursor.unwrap_or(state.output.cursor),
                        state.output.cursor,
                        terminal,
                    ))
                });
                let Ok(Some((after_sequence, published_sequence, terminal_at_request))) = cursor
                else {
                    return;
                };
                let client = client.clone();
                let pending_replay = replay.take();
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let response = client.request(
                            Uuid::nil(),
                            Uuid::nil(),
                            waku_client::Command::ReadDelegatedTaskEvents {
                                session_id: identity.session_id,
                                group_id: identity.group_id,
                                task_id: identity.task_id,
                                after_sequence,
                            },
                        );
                        match response {
                            Ok(waku_client::ResponsePayload::DelegatedTaskEvents { page }) => {
                                DetailEventRead::from_page(page, pending_replay)
                            }
                            _ => DetailEventRead::Failed {
                                replay: pending_replay,
                            },
                        }
                    })
                    .await;
                let mut keep_polling = false;
                let mut has_more = false;
                if entity
                    .update(cx, |waku, cx| {
                        let active = waku.active_delegation_detail_identity() == Some(identity);
                        let mut cache = waku.delegation_detail.borrow_mut();
                        let Some(state) = cache.as_mut().filter(|state| {
                            state.identity == identity
                                && state.generation == generation
                                && state.output.cursor == published_sequence
                        }) else {
                            return;
                        };
                        if !active {
                            state.polling = false;
                            return;
                        }
                        let task = cached_task(&waku.session_delegations, identity);
                        let progress = state.apply_read(result, task, terminal_at_request);
                        replay = progress.replay;
                        has_more = progress.has_more;
                        keep_polling = state.polling;
                        if progress.notify {
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
                if !has_more {
                    cx.background_executor().timer(DETAIL_POLL_INTERVAL).await;
                }
            }
        })
        .detach();
    }

    pub(super) fn focus_active_delegation_detail(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(identity) = self.active_delegation_detail_identity() else {
            return;
        };
        self.ensure_delegation_detail(identity, cx);
        if let Some(state) = self.delegation_detail.borrow_mut().as_mut() {
            window.focus(&state.focus, cx);
            state.pending_focus = false;
            // Revisiting rechecks even a previously drained terminal task.
            state.drained = false;
        }
        self.resume_active_delegation_detail(cx);
    }

    pub(super) fn render_delegation_detail(
        &mut self,
        group_id: Uuid,
        task_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let Some(session_id) = self.selected_session().map(|session| session.id) else {
            self.delegation_detail.borrow_mut().take();
            return div()
                .p(px(16.0))
                .text_color(theme.text_tertiary)
                .child(tr!("delegation.detail_unavailable"));
        };
        let identity = DetailIdentity {
            session_id,
            group_id,
            task_id,
        };
        self.ensure_delegation_detail(identity, cx);
        let mut cache = self.delegation_detail.borrow_mut();
        let state = cache.as_mut().expect("detail state ensured above");
        if state.pending_focus {
            window.focus(&state.focus, cx);
            state.pending_focus = false;
        }
        let mut content = div()
            .px(px(16.0))
            .pt(px(14.0))
            .pb(px(24.0))
            .text_color(theme.text);
        if let Some(task) = cached_task(&self.session_delegations, identity) {
            let status_color = super::delegation::task_status_color(&theme, task.status);
            let status_icon = icon(
                super::delegation::task_status_icon(task.status),
                13.0,
                status_color,
            );
            let status_icon = if task.status.is_terminal() {
                status_icon.into_any_element()
            } else {
                motion::spin(status_icon)
            };
            content = content
                .child(
                    div()
                        .text_size(sp(15.0))
                        .font_weight(FontWeight::MEDIUM)
                        .child(SharedString::from(task.target.display_label.clone())),
                )
                .child(
                    div()
                        .mt(px(4.0))
                        .text_size(sp(12.0))
                        .text_color(theme.text_tertiary)
                        .child(SharedString::from(format!(
                            "{} · {}",
                            task.target
                                .sub_provider
                                .as_deref()
                                .unwrap_or_else(|| task.target.provider.short_name()),
                            task.task_key,
                        ))),
                )
                .child(
                    div()
                        .mt(px(10.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(sp(12.5))
                        .text_color(status_color)
                        .child(status_icon)
                        .child(super::delegation::task_status_label(task.status)),
                );
            if let Some(activity) = super::delegation::active_task_activity(
                task.status,
                task.latest_activity.as_deref(),
            ) {
                content = content.child(
                    div()
                        .mt(px(6.0))
                        .text_size(sp(12.5))
                        .text_color(theme.text_secondary)
                        .child(SharedString::from(activity.to_owned())),
                );
            }
            if let Some(error) = task
                .error_message
                .as_deref()
                .filter(|text| !text.trim().is_empty())
            {
                content = content.child(
                    div()
                        .mt(px(12.0))
                        .text_size(sp(12.5))
                        .text_color(theme.warning)
                        .child(SharedString::from(error.to_owned())),
                );
            }
            if !state.has_content {
                state.selection.clear();
                content = content.child(
                    div()
                        .mt(px(18.0))
                        .text_size(sp(13.0))
                        .text_color(theme.text_tertiary)
                        .child(if task.status.is_terminal() && state.drained {
                            tr!("delegation.detail_no_result")
                        } else {
                            tr!("delegation.detail_waiting")
                        }),
                );
            }
        } else {
            content = content
                .text_color(theme.text_tertiary)
                .when(!state.has_content, |content| {
                    content.child(tr!("delegation.detail_unavailable"))
                });
        }
        let list_state = state.list.clone();
        let scrollbar_state = state.scrollbar.clone();
        let scroll_id = state.scroll_id.clone();
        let selection = state.selection.clone();
        let generation = state.generation;
        let selection_input = {
            let selection = state.selection.clone();
            canvas(
                |_, _, _| (),
                move |_, _, window, _| md::render::install_selection_input(window, &selection),
            )
            .absolute()
            .w(px(0.0))
            .h(px(0.0))
        };
        let focus = state.focus.clone();
        let copy_selection = state.selection.clone();
        let key_scroll = state.list.clone();
        let track_focus = state.focus.clone();
        // list() invokes its row callbacks later in layout. Never retain the
        // detail RefCell guard across those root-entity callbacks.
        drop(cache);
        let entity = cx.entity().downgrade();
        div()
            .flex_1()
            .min_h_0()
            .relative()
            .flex()
            .flex_col()
            .bg(theme.surface)
            .child(content)
            .child(
                div()
                    .id(scroll_id)
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .track_focus(&track_focus)
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            window.focus(&focus, cx);
                            this.resume_active_delegation_detail(cx);
                        }),
                    )
                    .on_action(move |_: &CopySelection, _, cx| {
                        if let Some(text) = copy_selection.selection.borrow().selected_text() {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        } else {
                            cx.propagate();
                        }
                    })
                    .on_key_down(cx.listener(move |_, event: &KeyDownEvent, _, cx| {
                        let modifiers = &event.keystroke.modifiers;
                        if modifiers.control
                            || modifiers.alt
                            || modifiers.platform
                            || (modifiers.shift && event.keystroke.key != "space")
                        {
                            return;
                        }
                        let offset = key_scroll.scroll_px_offset_for_scrollbar();
                        if let Some(next) = keyboard_scroll_offset(
                            event.keystroke.key.as_str(),
                            modifiers.shift,
                            offset.y,
                            key_scroll.max_offset_for_scrollbar().y,
                            key_scroll.viewport_bounds().size.height,
                        ) {
                            key_scroll.set_offset_from_scrollbar(point(offset.x, next));
                            cx.notify();
                            cx.stop_propagation();
                        }
                    }))
                    .child(md::render::frame_reset(selection))
                    .child(
                        list(list_state.clone(), move |index, window, cx| {
                            entity
                                .upgrade()
                                .map(|entity| {
                                    entity.update(cx, |this, cx| {
                                        this.delegation_detail_row(generation, index, window, cx)
                                    })
                                })
                                .unwrap_or_else(|| div().into_any_element())
                        })
                        .size_full()
                        .pb(px(24.0)),
                    )
                    .child(scrollbar::vertical(&list_state, &scrollbar_state)),
            )
            .child(selection_input)
    }

    pub(super) fn toggle_detail_group(
        &mut self,
        generation: Uuid,
        block: usize,
        current: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self
            .delegation_detail
            .borrow_mut()
            .as_mut()
            .filter(|state| state.generation == generation)
        {
            state.list.pause_following_tail();
            state.groups_expanded.insert(block, !current);
            state.sync_rows();
            cx.notify();
        }
    }

    pub(super) fn toggle_detail_item(
        &mut self,
        generation: Uuid,
        id: Uuid,
        current: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self
            .delegation_detail
            .borrow_mut()
            .as_mut()
            .filter(|state| state.generation == generation)
        {
            state.list.pause_following_tail();
            state.items_expanded.insert(id, !current);
            let block = state
                .output
                .session
                .transcript_blocks
                .iter()
                .position(|block| block.activities.iter().any(|activity| activity.id == id));
            if let Some(index) = block.and_then(|block| state.rows.iter()
                .position(|row| matches!(row, DetailRow::Activity(row_block, item)
                    if *row_block == block && state.output.session.transcript_blocks[block].activities[*item].id == id)))
            {
                state.list.splice(index..index + 1, 1);
            }
            cx.notify();
        }
    }

    fn delegation_detail_row(
        &self,
        generation: Uuid,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cache = self.delegation_detail.borrow();
        let Some(state) = cache
            .as_ref()
            .filter(|state| state.generation == generation)
        else {
            return div().into_any_element();
        };
        let theme = Theme::current(cx);
        let inner = match state.rows.get(index) {
            Some(DetailRow::Activities(block_index) | DetailRow::Activity(block_index, _)) => {
                let block = &state.output.session.transcript_blocks[*block_index];
                let live_group = state.live_group(*block_index);
                let live_reasoning_id = state.live_reasoning_id(*block_index);
                let (activities, part, expanded) = match state.rows[index] {
                    DetailRow::Activity(_, item) => (
                        &block.activities[item..item + 1],
                        ActivityClusterPart::Item,
                        true,
                    ),
                    _ => (
                        block.activities.as_slice(),
                        ActivityClusterPart::Header,
                        state
                            .groups_expanded
                            .get(block_index)
                            .copied()
                            .unwrap_or(live_group),
                    ),
                };
                self.render_activity_cluster(
                    activities,
                    *block_index,
                    ActivityRenderContext {
                        part,
                        detail_generation: Some(generation),
                        live_group,
                        live_reasoning_id,
                        expanded,
                        expanded_items: &state.items_expanded,
                        markdown: &state.activity_markdown,
                        window_starts: &state.reasoning_starts,
                        viewports: &state.activity_viewports,
                        selection: state.selection.clone(),
                    },
                    &theme,
                    window,
                    cx,
                )
            }
            Some(DetailRow::Message(message_index)) => {
                let message = &state.output.session.messages[*message_index];
                let palette = MarkdownPalette::from_theme(&theme);
                let menu = self.menu_handle(format!("detail-message-{}", message.id), cx);
                let ctx = MarkdownCtx::new(
                    format!("detail-message-{}", message.id),
                    &palette,
                    self.scaled_markdown_metrics(MarkdownMetrics::BODY),
                    state.selection.clone(),
                )
                .with_math_enabled(self.state.render_math)
                .with_math_context_menu(
                    self.menu_handle(format!("detail-message-math-{}", message.id), cx),
                )
                .with_context_menu(menu.clone())
                .with_link_handler(safe_detail_link_handler());
                let mut markdown = state.message_markdown.borrow_mut();
                let view = markdown.entry(message.id).or_default();
                view.set_text(message.visible_content(), message.streaming);
                render_message(
                    MessageRender {
                        theme: &theme,
                        message,
                        assistant_footer_copy_content: None,
                        assistant_footer_time: None,
                        copied: false,
                        assistant_message_action: None,
                        user_message_control: None,
                        user_message_viewport: None,
                        message_edit: None,
                        attachment_menus: Vec::new(),
                        attachment_images: Vec::new(),
                        attachments_can_reveal: false,
                        markdown: Some(view),
                        ctx: &ctx,
                        menu,
                        waku: cx.entity().downgrade(),
                        composer: self.composer.clone(),
                    },
                    window,
                    cx,
                )
            }
            None => return div().into_any_element(),
        };
        let padding = match state.rows[index] {
            DetailRow::Activity(_, _) => 0.0,
            DetailRow::Activities(_) => 4.0,
            DetailRow::Message(_) => 8.0,
        };
        div()
            .w_full()
            .min_w_0()
            // GPUI's virtual list lays out rows at its full width and applies
            // only vertical list padding. Inset the row itself so Markdown
            // measurement and activity controls share the header's margins.
            .px(px(16.0))
            .py(px(padding))
            .child(inner)
            .into_any_element()
    }
}

fn keyboard_scroll_offset(
    key: &str,
    shift: bool,
    current: Pixels,
    max_offset: Pixels,
    viewport_height: Pixels,
) -> Option<Pixels> {
    let page = viewport_height * 0.9;
    let next = match key {
        "up" => current + px(40.0),
        "down" => current - px(40.0),
        "pageup" => current + page,
        "pagedown" => current - page,
        "home" => Pixels::ZERO,
        "end" => -max_offset,
        "space" if shift => current + page,
        "space" => current - page,
        _ => return None,
    };
    Some(next.clamp(-max_offset.max(Pixels::ZERO), Pixels::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use waku_client::delegation::{DelegatedTaskStatus, DelegationGroupStatus, ModelReference};

    fn event(sequence: u64, kind: &str, delta: &str) -> DelegatedTaskEvent {
        DelegatedTaskEvent {
            sequence,
            created_at_ms: sequence.saturating_mul(1000),
            event: waku_client::WireDriverEvent {
                kind: kind.into(),
                payload: serde_json::Value::String(delta.into()),
            },
        }
    }

    fn page(events: Vec<DelegatedTaskEvent>, next_sequence: u64) -> DelegatedTaskEvents {
        DelegatedTaskEvents {
            events,
            next_sequence,
            has_more: false,
        }
    }

    fn payload_event(sequence: u64, kind: &str, payload: serde_json::Value) -> DelegatedTaskEvent {
        let mut event = event(sequence, kind, "");
        event.event.payload = payload;
        event
    }

    #[test]
    fn native_thinking_blocks_survive_background_tool_updates_and_replay_delay() {
        let mut output = DetailOutput::default();
        let first = "**Checking settings definition**\n\n**Checking default-path references**";
        let second = "**Implementing daemon monitoring**\n\n**Initializing daemon state**";
        let tool = |complete| {
            serde_json::to_value(ActivityItem::new(
                Some("earlier-tool".into()),
                ActivityKind::Command,
                "Run command",
                None,
                complete,
            ))
            .unwrap()
        };
        output.append_page(page(vec![
            payload_event(1, "richActivity", tool(false)),
            payload_event(2, "reasoningStarted", serde_json::json!({"id":"first","timestampMs":5000})),
            payload_event(3, "reasoningBlockDelta", serde_json::json!({"id":"first","delta":"**Checking settings definition**"})),
            payload_event(4, "richActivity", tool(true)),
            payload_event(5, "reasoningBlockDelta", serde_json::json!({"id":"first","delta":"\n\n**Checking default-path references**"})),
        ], 5));
        let activities = &output.session.transcript_blocks[0].activities;
        assert_eq!(
            activities.len(),
            2,
            "background progress cannot split a thought"
        );
        assert!(!activities[1].complete);
        assert_eq!(activities[1].reasoning.as_ref().unwrap().content, first);
        output.append_page(page(
            vec![
                payload_event(
                    6,
                    "reasoningCompleted",
                    serde_json::json!({"id":"first","timestampMs":47000,"content":first}),
                ),
                payload_event(
                    7,
                    "reasoningStarted",
                    serde_json::json!({"id":"second","timestampMs":48000}),
                ),
                payload_event(
                    8,
                    "reasoningBlockDelta",
                    serde_json::json!({"id":"second","delta":"**partial**"}),
                ),
                payload_event(
                    9,
                    "reasoningCompleted",
                    serde_json::json!({"id":"second","timestampMs":65000,"content":second}),
                ),
            ],
            9,
        ));
        let activities = &output.session.transcript_blocks[0].activities;
        assert_eq!(
            activities.len(),
            3,
            "independent provider blocks cannot merge"
        );
        assert!(activities.iter().all(|activity| activity.complete));
        let reasoning = activities[1].reasoning.as_ref().unwrap();
        assert_eq!(reasoning.finished_at_ms - reasoning.started_at_ms, 42000);
        assert_eq!(activities[2].reasoning.as_ref().unwrap().content, second);
    }

    #[test]
    fn earlier_tool_updates_preserve_legacy_reasoning_and_text_continuity() {
        let mut output = DetailOutput::default();
        let tool = || {
            serde_json::to_value(ActivityItem::new(
                Some("tool".into()),
                ActivityKind::Command,
                "Run command",
                None,
                false,
            ))
            .unwrap()
        };
        output.append_page(page(
            vec![
                payload_event(1, "richActivity", tool()),
                event(2, "reasoningDelta", "first"),
                payload_event(3, "richActivity", tool()),
                event(4, "reasoningDelta", " second"),
            ],
            4,
        ));
        assert_eq!(output.phase, Some(StreamPhase::Reasoning));
        let activities = &output.session.transcript_blocks[0].activities;
        assert_eq!(activities.len(), 2);
        assert!(!activities[1].complete);
        assert_eq!(
            activities[1].reasoning.as_ref().unwrap().content,
            "first second"
        );
        output.append_page(page(
            vec![
                event(5, "textDelta", "answer"),
                payload_event(6, "richActivity", tool()),
                event(7, "textDelta", " continues"),
            ],
            7,
        ));
        assert_eq!(output.phase, Some(StreamPhase::Text));
        assert_eq!(output.session.messages.len(), 1);
        assert_eq!(output.session.messages[0].content, "answer continues");
    }

    #[gpui::test]
    fn completed_provider_thought_is_not_running_while_the_task_continues(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut state = detail_state(cx);
            state.history_loaded = true;
            state.output.append_page(page(
                vec![
                    payload_event(
                        1,
                        "reasoningStarted",
                        serde_json::json!({"id":"thought","timestampMs":1000}),
                    ),
                    payload_event(
                        2,
                        "reasoningBlockDelta",
                        serde_json::json!({"id":"thought","delta":"checking"}),
                    ),
                ],
                2,
            ));
            let task = task_summary(DelegatedTaskStatus::Running);
            state.sync_document(&task);
            assert!(state.live_reasoning_id(0).is_some());
            state.output.append_page(page(
                vec![payload_event(
                    3,
                    "reasoningCompleted",
                    serde_json::json!({"id":"thought","timestampMs":16000,"content":"checked"}),
                )],
                3,
            ));
            state.document_dirty = true;
            state.sync_document(&task);
            assert!(state.rendered_live);
            assert_eq!(state.live_reasoning_id(0), None);
        });
    }

    fn detail_state(cx: &mut App) -> DelegationDetailState {
        DelegationDetailState::new(
            DetailIdentity {
                session_id: Uuid::new_v4(),
                group_id: Uuid::new_v4(),
                task_id: Uuid::new_v4(),
            },
            cx,
        )
    }

    fn task_summary(status: DelegatedTaskStatus) -> DelegatedTaskSummary {
        DelegatedTaskSummary {
            task_id: Uuid::new_v4(),
            attempt_id: None,
            task_key: "review".into(),
            target: ModelReference::new("target", ProviderKind::Codex, "model", "Model"),
            status,
            duration_ms: None,
            final_conclusion: None,
            error_message: None,
            latest_activity: None,
        }
    }

    #[gpui::test]
    fn historical_thinking_stays_folded_while_loading_and_rechecking_completion(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let mut state = detail_state(cx);
            let task = task_summary(DelegatedTaskStatus::Completed);
            let mut first = page(vec![event(1, "reasoningDelta", "old thought")], 1);
            first.has_more = true;
            let progress = state.apply_read(
                DetailEventRead::from_page(first, Some(DetailOutput::default())),
                Some(&task),
                true,
            );
            assert!(!progress.notify);
            assert!(progress.has_more);
            assert_eq!(state.output.cursor, 0, "no partial replay was published");
            state.sync_document(&task);
            assert!(
                state.rows.is_empty(),
                "partial history is not replayed on screen"
            );

            let progress = state.apply_read(
                DetailEventRead::Failed {
                    replay: progress.replay,
                },
                Some(&task),
                true,
            );
            assert_eq!(progress.replay.as_ref().unwrap().cursor, 1);
            assert!(!progress.notify);
            assert!(state.polling, "retry without losing the historical prefix");

            // Completion can arrive while the final page is in flight. Its
            // thoughts fold immediately, while a fresh read still verifies the
            // authoritative final cursor.
            let progress = state.apply_read(
                DetailEventRead::from_page(
                    page(vec![event(2, "reasoningDelta", " finishes")], 2),
                    progress.replay,
                ),
                Some(&task),
                false,
            );
            assert!(progress.notify);
            assert!(!progress.has_more);
            assert!(progress.replay.is_none());
            assert_eq!(state.output.cursor, 2);
            assert_eq!(state.rows, vec![DetailRow::Activities(0)]);
            assert!(!state.live_group(0));
            assert_eq!(state.live_reasoning_id(0), None);
            assert!(!state.rendered_terminal, "final cursor has not drained yet");

            state.apply_read(DetailEventRead::Page(page(vec![], 2)), Some(&task), true);
            assert!(state.drained);
            assert!(!state.polling);
            state.drained = false; // Reopening rechecks the final cursor.
            state.sync_document(&task);
            assert_eq!(state.rows, vec![DetailRow::Activities(0)]);
            assert!(
                !state.live_group(0),
                "rechecking cannot reopen finished thinking"
            );
        });
    }

    #[gpui::test]
    fn current_thinking_follows_main_disclosure_rules_and_respects_manual_expansion(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let mut state = detail_state(cx);
            let mut task = task_summary(DelegatedTaskStatus::Running);
            state.history_loaded = true;
            state
                .output
                .append_page(page(vec![event(1, "reasoningDelta", "live thought")], 1));
            state.sync_document(&task);
            assert_eq!(
                state.rows,
                vec![DetailRow::Activities(0), DetailRow::Activity(0, 0)]
            );
            assert!(state.live_reasoning_id(0).is_some());

            state
                .output
                .append_page(page(vec![event(2, "textDelta", "commentary")], 2));
            state.document_dirty = true;
            state.sync_document(&task);
            assert_eq!(
                state.rows,
                vec![DetailRow::Activities(0), DetailRow::Message(0)]
            );
            assert_eq!(state.live_reasoning_id(0), None);

            state
                .output
                .append_page(page(vec![event(3, "reasoningDelta", "next thought")], 3));
            state.document_dirty = true;
            state.sync_document(&task);
            let thought = state.live_reasoning_id(1).unwrap();
            assert!(state.rows.contains(&DetailRow::Activity(1, 0)));
            assert!(!state.rows.contains(&DetailRow::Activity(0, 0)));

            task.status = DelegatedTaskStatus::Completed;
            state.sync_document(&task);
            assert!(!state.rows.contains(&DetailRow::Activity(1, 0)));
            assert_eq!(state.live_reasoning_id(1), None);
            assert!(
                !state.drained,
                "fold on completion, not on the later final read"
            );

            state.groups_expanded.insert(1, true);
            state.items_expanded.insert(thought, true);
            state.sync_rows();
            state.drained = true;
            state.sync_document(&task);
            assert!(state.rows.contains(&DetailRow::Activity(1, 0)));
            assert!(state.items_expanded[&thought]);
        });
    }

    #[gpui::test]
    fn detail_updates_and_earlier_disclosures_preserve_the_reading_offset(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut state = detail_state(cx);
            let task = task_summary(DelegatedTaskStatus::Completed);
            state.history_loaded = true;
            state.drained = true;
            state.output.append_page(page(
                vec![
                    event(1, "reasoningDelta", "first thought"),
                    event(2, "textDelta", "commentary"),
                    event(3, "reasoningDelta", "second thought"),
                    event(4, "textDelta", "answer"),
                ],
                4,
            ));
            state.sync_document(&task);
            state.list.scroll_to(ListOffset {
                item_ix: 3,
                offset_in_item: px(11.0),
            });
            let assert_reading_position = |state: &DelegationDetailState| {
                let offset = state.list.logical_scroll_top();
                assert_eq!(state.rows[offset.item_ix], DetailRow::Message(1));
                assert_eq!(offset.offset_in_item, px(11.0));
                assert!(!state.list.is_following_tail());
            };
            state.document_dirty = true;
            state.sync_document(&task);
            assert_reading_position(&state);

            state.groups_expanded.insert(0, true);
            state.sync_rows();
            assert_reading_position(&state);
            state.groups_expanded.insert(0, false);
            state.sync_rows();
            assert_reading_position(&state);

            state
                .output
                .append_page(page(vec![event(5, "textDelta", "later output")], 5));
            state.document_dirty = true;
            state.sync_document(&task);
            assert_reading_position(&state);
        });
    }

    #[test]
    fn replayed_thought_duration_uses_recorded_receipt_times() {
        let mut output = DetailOutput::default();
        output.append_page(page(
            vec![
                event(10, "reasoningDelta", "thinking"),
                event(25, "reasoningDelta", " across pages"),
                event(26, "textDelta", "answer"),
            ],
            26,
        ));
        output.settle(Some("answer"));
        let reasoning = output.session.transcript_blocks[0].activities[0]
            .reasoning
            .as_ref()
            .unwrap();
        assert_eq!(reasoning.started_at_ms, 10_000);
        assert_eq!(reasoning.finished_at_ms, 25_000);
        assert_eq!(reasoning.content, "thinking across pages");
    }

    #[test]
    fn retry_replay_preserves_the_cursor_but_displays_only_the_current_attempt() {
        let old = Uuid::new_v4();
        let next = Uuid::new_v4();
        let marker = |sequence, id| DelegatedTaskEvent {
            sequence,
            created_at_ms: sequence * 1000,
            event: waku_client::WireDriverEvent::new(
                "delegationAttemptStarted",
                serde_json::json!({"attemptId": id}),
            ),
        };
        let mut output = DetailOutput {
            expected_attempt: Some(next),
            ..Default::default()
        };
        output.append_page(page(
            vec![marker(1, old), event(2, "textDelta", "old answer")],
            2,
        ));
        assert!(output.session.messages.is_empty());
        output.append_page(page(
            vec![marker(3, next), event(4, "textDelta", "new answer")],
            4,
        ));
        output.settle(Some("new answer"));
        assert_eq!(output.cursor, 4);
        assert_eq!(output.session.messages.len(), 1);
        assert_eq!(output.session.messages[0].content, "new answer");
    }

    #[test]
    fn provider_output_preserves_phase_order_and_coalesces_adjacent_deltas() {
        let mut output = DetailOutput::default();
        output.append_page(page(
            vec![
                event(1, "reasoningDelta", "first thought"),
                event(2, "reasoningDelta", " continues"),
                event(3, "textDelta", "commentary"),
                event(4, "reasoningDelta", "second thought"),
                event(5, "textDelta", "answer"),
            ],
            5,
        ));
        assert_eq!(
            output.row_plan(),
            vec![
                DetailRow::Activities(0),
                DetailRow::Message(0),
                DetailRow::Activities(1),
                DetailRow::Message(1),
            ]
        );
        assert_eq!(
            output.session.transcript_blocks[0].activities[0]
                .reasoning
                .as_ref()
                .unwrap()
                .content,
            "first thought continues"
        );
        assert!(output.session.transcript_blocks[0].activities[0].complete);
        output.settle(Some("commentaryanswer"));
        assert_eq!(output.session.messages.len(), 2);
        assert_eq!(output.session.messages[0].content, "commentary");
        assert_eq!(output.session.messages[1].content, "answer");
        assert!(
            output
                .session
                .messages
                .iter()
                .all(|message| !message.streaming)
        );
    }

    #[test]
    fn separator_only_messages_do_not_space_out_activities_and_can_gain_visible_text() {
        let mut output = DetailOutput::default();
        output.append_page(page(
            vec![
                event(1, "reasoningDelta", "first thought"),
                event(2, "textDelta", "\n\n"),
                event(3, "reasoningDelta", "second thought"),
                event(4, "textDelta", "\n\n"),
            ],
            4,
        ));
        assert_eq!(
            output.row_plan(),
            vec![DetailRow::Activities(0), DetailRow::Activities(1)]
        );
        assert_eq!(output.session.transcript_blocks[1].after_message, 1);
        assert_eq!(output.session.messages.len(), 2);

        output.append_page(page(vec![event(5, "textDelta", "Answer")], 5));
        assert_eq!(
            output.row_plan(),
            vec![
                DetailRow::Activities(0),
                DetailRow::Activities(1),
                DetailRow::Message(1),
            ]
        );
        assert_eq!(output.session.messages[1].content, "\n\nAnswer");
        output.settle(Some("Answer"));
        assert_eq!(output.session.messages.len(), 2);
    }

    #[test]
    fn final_conclusion_already_in_the_last_prose_is_not_repeated() {
        let mut output = DetailOutput::default();
        output.append_page(page(
            vec![event(1, "textDelta", "Commentary.\n\nActual final answer.")],
            1,
        ));
        output.settle(Some("Actual final answer."));
        assert_eq!(output.session.messages.len(), 1);
        assert_eq!(
            output.session.messages[0].content,
            "Commentary.\n\nActual final answer."
        );
    }

    #[test]
    fn event_cursor_advances_over_non_displayable_events_and_ignores_replays() {
        let mut output = DetailOutput::default();
        output.append_page(page(vec![event(10, "textDelta", "hello")], 10));
        assert!(!output.append_page(page(
            vec![
                event(10, "textDelta", "duplicate"),
                event(12, "toolEnd", "ignored"),
                event(13, "reasoningDelta", ""),
            ],
            14
        )));
        assert_eq!(output.cursor, 14);
        assert_eq!(output.session.messages[0].content, "hello");
        assert!(!output.append_page(page(vec![], 2)));
        assert_eq!(output.cursor, 14);
    }

    #[test]
    fn tool_snapshots_update_the_original_row_and_keep_input_and_failure() {
        let mut output = DetailOutput::default();
        let initial = ActivityItem::new(
            Some("call-1".into()),
            ActivityKind::Tool,
            "Inspect",
            None,
            false,
        );
        let mut started = initial.clone();
        started.arguments = Some("{\"path\":\"snapshot.rs\"}".into());
        let mut finished = initial;
        finished.complete = true;
        finished.failed = true;
        finished.output = Some("permission denied".into());
        let rich = |sequence, item| DelegatedTaskEvent {
            sequence,
            created_at_ms: sequence.saturating_mul(1000),
            event: waku_client::event_to_wire(DriverEvent::RichActivity(item)).unwrap(),
        };
        output.append_page(page(vec![rich(1, started)], 1));
        let id = output.session.transcript_blocks[0].activities[0].id;
        output.append_page(page(
            vec![event(2, "textDelta", "commentary"), rich(3, finished)],
            3,
        ));
        assert_eq!(output.session.transcript_blocks.len(), 1);
        let activity = &output.session.transcript_blocks[0].activities[0];
        assert_eq!(activity.id, id);
        assert!(activity.failed && activity.complete);
        assert_eq!(
            activity.arguments.as_deref(),
            Some("{\"path\":\"snapshot.rs\"}")
        );
        assert_eq!(activity.output.as_deref(), Some("permission denied"));
        output.settle(Some("authoritative answer"));
        assert_eq!(output.session.messages[0].content, "commentary");
        assert_eq!(output.session.messages[1].content, "authoritative answer");
        output.settle(Some("authoritative answer"));
        assert_eq!(output.session.messages.len(), 2);
    }

    #[test]
    fn legacy_activity_payloads_and_tasks_without_events_keep_actual_content() {
        let mut output = DetailOutput::default();
        output.append_page(page(
            vec![DelegatedTaskEvent {
                sequence: 1,
                created_at_ms: 1000,
                event: waku_client::event_to_wire(DriverEvent::Activity {
                    id: Some("legacy-call".into()),
                    kind: ActivityKind::Tool,
                    title: "Legacy tool".into(),
                    detail: Some("result".into()),
                    complete: true,
                })
                .unwrap(),
            }],
            1,
        ));
        assert_eq!(
            output.session.transcript_blocks[0].activities[0]
                .detail
                .as_deref(),
            Some("result")
        );
        let mut legacy = DetailOutput::default();
        legacy.settle(Some("Actual saved conclusion"));
        assert_eq!(legacy.session.messages.len(), 1);
        assert_eq!(
            legacy.session.messages[0].content,
            "Actual saved conclusion"
        );
    }

    #[test]
    fn task_lookup_is_scoped_to_session_and_group() {
        let session_id = Uuid::new_v4();
        let group_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        let identity = DetailIdentity {
            session_id,
            group_id,
            task_id,
        };
        let group = DelegationGroupInfo {
            id: group_id,
            parent_session_id: session_id,
            parent_turn_id: None,
            status: DelegationGroupStatus::Completed,
            review_source: None,
            patch_summary: None,
            tasks: vec![DelegatedTaskSummary {
                task_id,
                attempt_id: None,
                task_key: "review".into(),
                target: ModelReference::new("target", ProviderKind::Codex, "model", "Model"),
                status: DelegatedTaskStatus::Completed,
                duration_ms: None,
                final_conclusion: Some("# Conclusion".into()),
                error_message: None,
                latest_activity: None,
            }],
            created_at: 0,
            updated_at: 0,
        };
        let mut sessions = HashMap::from([(session_id, vec![group.clone()])]);
        assert_eq!(cached_task(&sessions, identity).unwrap().task_id, task_id);
        assert!(
            cached_task(
                &sessions,
                DetailIdentity {
                    session_id: Uuid::new_v4(),
                    ..identity
                }
            )
            .is_none()
        );
        assert!(
            cached_task(
                &sessions,
                DetailIdentity {
                    group_id: Uuid::new_v4(),
                    ..identity
                }
            )
            .is_none()
        );
        assert!(
            cached_task(
                &sessions,
                DetailIdentity {
                    task_id: Uuid::new_v4(),
                    ..identity
                }
            )
            .is_none()
        );

        let other_session = Uuid::new_v4();
        sessions.insert(other_session, vec![group]);
        assert!(
            cached_task(
                &sessions,
                DetailIdentity {
                    session_id: other_session,
                    ..identity
                }
            )
            .is_none()
        );
    }

    #[test]
    fn keyboard_scrolling_stays_inside_document_bounds() {
        let scroll = |key, shift, offset| {
            keyboard_scroll_offset(key, shift, px(offset), px(500.0), px(200.0))
        };
        assert_eq!(scroll("up", false, -10.0), Some(px(0.0)));
        assert_eq!(scroll("down", false, -490.0), Some(px(-500.0)));
        assert_eq!(scroll("space", false, -100.0), Some(px(-280.0)));
        assert_eq!(scroll("space", true, -100.0), Some(px(0.0)));
        assert_eq!(scroll("home", false, -100.0), Some(px(0.0)));
        assert_eq!(scroll("end", false, -100.0), Some(px(-500.0)));
        assert_eq!(scroll("enter", false, -100.0), None);
        assert_eq!(
            keyboard_scroll_offset("end", false, px(0.0), px(0.0), px(200.0)),
            Some(px(0.0))
        );
    }
}

// Temporary real-history integration harness; removed after integration smoke.
#[cfg(test)]
mod real_replay_smoke {
    use super::*;
    use gpui::{Render, TestAppContext, size};
    use serde::Deserialize;
    use waku_client::delegation::{DelegatedTaskStatus, DelegationGroupStatus};

    #[derive(Deserialize)]
    struct Fixture {
        tasks: Vec<FixtureTask>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureTask {
        parent_session_id: Uuid,
        group_id: Uuid,
        summary: DelegatedTaskSummary,
        events: Vec<DelegatedTaskEvent>,
    }

    struct Harness {
        waku: Entity<Waku>,
        group: Uuid,
        task: Uuid,
    }

    impl Render for Harness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.waku.update(cx, |waku, cx| {
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(waku.render_background_work_summary(cx))
                    .child(waku.render_delegation_detail(self.group, self.task, window, cx))
            })
        }
    }

    #[gpui::test]
    #[ignore]
    fn real_delegated_history_replays_and_renders(cx: &mut TestAppContext) {
        assert!(
            std::env::var_os("WAKU_SUBAGENT_SMOKE_DATA_DIR").is_some(),
            "the test daemon and client persistence MUST both use an isolated data directory"
        );
        let fixture: Fixture = serde_json::from_slice(
            &std::fs::read(
                std::env::var_os("WAKU_DELEGATION_SMOKE_PATH").expect("real event fixture path"),
            )
            .unwrap(),
        )
        .unwrap();
        let daemon = waku_client::DaemonSupervisor::spawn(
            &PathBuf::from(
                std::env::var_os("WAKU_DELEGATION_SMOKE_DAEMON")
                    .expect("isolated real-backend daemon executable"),
            ),
            false,
        )
        .unwrap();
        cx.update(|cx| {
            crate::theme::init(cx);
            crate::input::init(cx);
            crate::ui::menu::init(cx);
        });
        let (harness, cx) = cx.add_window_view(move |window, cx| Harness {
            waku: Waku::new(window, cx, daemon),
            group: Uuid::nil(),
            task: Uuid::nil(),
        });
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        let waku = cx.read_entity(&harness, |harness, _| harness.waku.clone());
        for task in fixture.tasks {
            let identity = DetailIdentity {
                session_id: task.parent_session_id,
                group_id: task.group_id,
                task_id: task.summary.task_id,
            };
            let mut summary = task.summary.clone();
            summary.status = DelegatedTaskStatus::Running;
            summary.final_conclusion = None;
            let (mut run_bytes, mut largest_bytes, mut reason_end) = (0, 0, 0);
            for (index, event) in task.events.iter().enumerate() {
                match event.event.kind.as_str() {
                    "reasoningDelta" => {
                        run_bytes += event.event.payload.as_str().map_or(0, str::len)
                    }
                    "textDelta" | "richActivity" | "activity" => {
                        if run_bytes > largest_bytes {
                            largest_bytes = run_bytes;
                            reason_end = index;
                        }
                        run_bytes = 0;
                    }
                    _ => {}
                }
            }
            if run_bytes > largest_bytes {
                reason_end = task.events.len();
            }
            assert!(reason_end > 0, "fixture has actual reasoning");
            cx.update(|_, cx| {
                waku.update(cx, |waku, cx| {
                    if !waku
                        .state
                        .sessions
                        .iter()
                        .any(|session| session.id == identity.session_id)
                    {
                        let mut session = AgentSession::new(Uuid::nil(), summary.target.provider);
                        session.id = identity.session_id;
                        waku.state.sessions.push(session);
                    }
                    waku.delegation_refresh_pending.insert(identity.session_id);
                    waku.state.selected_session = Some(identity.session_id);
                    let groups = waku
                        .session_delegations
                        .entry(identity.session_id)
                        .or_default();
                    if let Some(group) = groups
                        .iter_mut()
                        .find(|group| group.id == identity.group_id)
                    {
                        group.tasks.push(summary.clone());
                    } else {
                        groups.push(DelegationGroupInfo {
                            id: identity.group_id,
                            parent_session_id: identity.session_id,
                            parent_turn_id: None,
                            status: DelegationGroupStatus::Running,
                            review_source: None,
                            patch_summary: None,
                            tasks: vec![summary.clone()],
                            created_at: 0,
                            updated_at: 0,
                        });
                    }
                    let mut state = DelegationDetailState::new(identity, cx);
                    state.polling = true; // Replay owns delivery, not the polling timer.
                    state.output.append_page(DelegatedTaskEvents {
                        events: task.events[..reason_end].to_vec(),
                        next_sequence: task.events[reason_end - 1].sequence,
                        has_more: false,
                    });
                    state.history_loaded = true; // A caught-up live snapshot.
                    state.sync_document(&summary);
                    assert!(
                        state
                            .rows
                            .iter()
                            .any(|row| matches!(row, DetailRow::Activity(_, _))),
                        "live thoughts expand by default into virtualized activity rows"
                    );
                    state.list.scroll_to_reveal_item(
                        state
                            .rows
                            .iter()
                            .rposition(|row| matches!(row, DetailRow::Activity(_, _)))
                            .unwrap(),
                    );
                    *waku.delegation_detail.borrow_mut() = Some(state);
                })
            });
            cx.update_entity(&harness, |harness, cx| {
                harness.group = identity.group_id;
                harness.task = identity.task_id;
                cx.notify();
            });
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            cx.update(|_, cx| {
                waku.update(cx, |waku, _| {
                    let cache = waku.delegation_detail.borrow();
                    let state = cache.as_ref().unwrap();
                    eprintln!(
                        "LIVE VIEWPORT: phase={:?}, rows={}, bounds={:?}, offset={:?}",
                        state.output.phase,
                        state.rows.len(),
                        state.list.viewport_bounds(),
                        state.list.scroll_px_offset_for_scrollbar()
                    );
                    assert!(
                        !state.activity_markdown.borrow().is_empty(),
                        "the shared live-thinking renderer prepared the visible reasoning"
                    );
                    assert!(
                        state
                            .activity_markdown
                            .borrow()
                            .values()
                            .all(|view| view.source_len() <= LIVE_REASONING_WINDOW_MAX),
                        "live thinking is byte-window bounded in the actual renderer"
                    );
                })
            });
            let handle = cx.update(|_, cx| {
                waku.update(cx, |waku, cx| {
                    waku.menu_handle("background-work-summary", cx)
                })
            });
            cx.update(|window, cx| handle.open_context_menu(window, cx));
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            assert!(handle.is_open(), "existing info summary popover opened");
            cx.update(|window, cx| handle.close(window, cx));
            cx.update(|_, cx| {
                waku.update(cx, |waku, cx| {
                    let (generation, block, tool, before_main) = {
                        let mut cache = waku.delegation_detail.borrow_mut();
                        let state = cache.as_mut().unwrap();
                        state.output.append_page(DelegatedTaskEvents {
                            events: task.events.clone(),
                            next_sequence: task.events.last().unwrap().sequence,
                            has_more: false,
                        });
                        let aggregate = state
                            .output
                            .session
                            .messages
                            .iter()
                            .map(|message| message.content.as_str())
                            .collect::<String>();
                        let message_count = state.output.session.messages.len();
                        let sources = state
                            .output
                            .session
                            .transcript_blocks
                            .iter()
                            .flat_map(|block| &block.activities)
                            .filter_map(|activity| activity.source_id.as_ref())
                            .collect::<Vec<_>>();
                        assert_eq!(
                            sources.len(),
                            sources.iter().copied().collect::<HashSet<_>>().len(),
                            "tool snapshots with matching source IDs updated in place"
                        );
                        summary.status = DelegatedTaskStatus::Completed;
                        summary.final_conclusion = Some(aggregate);
                        state.drained = true;
                        state.document_dirty = true;
                        state.sync_document(&summary);
                        assert_eq!(
                            state.output.session.messages.len(),
                            message_count,
                            "authoritative aggregate did not duplicate streamed commentary/answer"
                        );
                        assert!(
                            !state
                                .rows
                                .iter()
                                .any(|row| matches!(row, DetailRow::Activity(_, _))),
                            "settled activity groups fold by default"
                        );
                        let block = state
                            .output
                            .session
                            .transcript_blocks
                            .iter()
                            .position(|block| {
                                block
                                    .activities
                                    .iter()
                                    .any(|activity| activity.reasoning.is_none())
                            })
                            .expect("real tool activity");
                        let tool = state.output.session.transcript_blocks[block]
                            .activities
                            .iter()
                            .find(|activity| activity.reasoning.is_none())
                            .unwrap()
                            .id;
                        (
                            state.generation,
                            block,
                            tool,
                            waku.activities_expanded.clone(),
                        )
                    };
                    if let Some(task) = waku
                        .session_delegations
                        .get_mut(&identity.session_id)
                        .and_then(|groups| {
                            groups
                                .iter_mut()
                                .find(|group| group.id == identity.group_id)
                        })
                        .and_then(|group| {
                            group
                                .tasks
                                .iter_mut()
                                .find(|task| task.task_id == identity.task_id)
                        })
                    {
                        *task = summary.clone();
                    }
                    waku.toggle_detail_group(generation, block, false, cx);
                    waku.toggle_detail_item(generation, tool, false, cx);
                    assert_eq!(
                        waku.activities_expanded, before_main,
                        "child disclosure did not modify main transcript groups"
                    );
                    let cache = waku.delegation_detail.borrow();
                    let state = cache.as_ref().unwrap();
                    assert!(state.items_expanded[&tool]);
                    eprintln!(
                        "REAL DETAIL: {} events={} groups={} messages={} rows={}",
                        task.summary.target.display_label,
                        task.events.len(),
                        state.output.session.transcript_blocks.len(),
                        state.output.session.messages.len(),
                        state.rows.len()
                    );
                })
            });
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            let reveal_rows =
                cx.update(|_, cx| {
                    waku.update(cx, |waku, cx| {
                        let (generation, reasoning_block) =
                            {
                                let cache = waku.delegation_detail.borrow();
                                let state = cache.as_ref().unwrap();
                                (
                                    state.generation,
                                    state.output.session.transcript_blocks.iter().position(
                                        |block| {
                                            block
                                                .activities
                                                .iter()
                                                .any(|activity| activity.reasoning.is_some())
                                        },
                                    ),
                                )
                            };
                        if let Some(block) = reasoning_block {
                            waku.toggle_detail_group(generation, block, false, cx);
                            let id = waku
                                .delegation_detail
                                .borrow()
                                .as_ref()
                                .unwrap()
                                .output
                                .session
                                .transcript_blocks[block]
                                .activities
                                .iter()
                                .find(|activity| activity.reasoning.is_some())
                                .unwrap()
                                .id;
                            waku.toggle_detail_item(generation, id, false, cx);
                        }
                        let cache = waku.delegation_detail.borrow();
                        let state = cache.as_ref().unwrap();
                        state
                            .rows
                            .iter()
                            .enumerate()
                            .filter_map(|(index, row)| match row {
                                DetailRow::Message(_) | DetailRow::Activity(_, _) => Some(index),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                    })
                });
            for row in reveal_rows {
                cx.update(|_, cx| {
                    waku.update(cx, |waku, _| {
                        waku.delegation_detail
                            .borrow()
                            .as_ref()
                            .unwrap()
                            .list
                            .scroll_to_reveal_item(row);
                    })
                });
                cx.update(|window, cx| {
                    window.refresh();
                    window.draw(cx).clear(cx);
                });
            }
        }
        cx.update(|_, cx| {
            waku.update(cx, |waku, _| {
                let session = waku.state.selected_session.unwrap();
                let groups = waku.session_delegations.get_mut(&session).unwrap();
                let template = groups[0].tasks[0].clone();
                groups.truncate(1);
                groups[0].tasks = (0..201)
                    .map(|index| {
                        let mut task = template.clone();
                        task.task_id = Uuid::new_v4();
                        task.task_key = format!("long-session-{index}");
                        task.status = if index == 200 {
                            DelegatedTaskStatus::Running
                        } else {
                            DelegatedTaskStatus::Completed
                        };
                        task
                    })
                    .collect();
            })
        });
        let handle = cx.update(|_, cx| {
            waku.update(cx, |waku, cx| {
                waku.menu_handle("background-work-summary", cx)
            })
        });
        cx.update(|window, cx| handle.open_context_menu(window, cx));
        let start = std::time::Instant::now();
        for _ in 0..5 {
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
        }
        eprintln!(
            "REAL INFO LAYOUT: 200 settled + 1 running, 5 production layout frames {:?}",
            start.elapsed()
        );
        let (session, copy_focus, last_row, last_stop, first_action, second_action) =
            cx.update(|_, cx| {
                waku.update(cx, |waku, cx| {
                    let session = waku.state.selected_session.unwrap();
                    let group = &waku.session_delegations[&session][0];
                    let target = format!(
                        "delegated-{}-{}",
                        group.id,
                        group.tasks.last().unwrap().task_id
                    );
                    (
                        session,
                        waku.transcript_control_focus("background-summary-copy-task-id", cx),
                        waku.transcript_control_focus(
                            format!("background-summary-row-{target}"),
                            cx,
                        ),
                        waku.transcript_control_focus(
                            format!("background-summary-stop-{target}"),
                            cx,
                        ),
                        waku.transcript_control_focus("environment-summary-commit", cx),
                        waku.transcript_control_focus("environment-summary-compare", cx),
                    )
                })
            });
        let frame = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..3 {
                cx.update(|window, cx| {
                    window.refresh();
                    window.draw(cx).clear(cx);
                    window.simulate_next_frame(cx);
                });
            }
        };
        frame(cx);
        cx.update(|window, _| {
            assert!(
                handle.focus_handle().is_focused(window),
                "opening frames focused the actual info popover before navigation"
            );
        });
        cx.simulate_keystrokes("end");
        frame(cx);
        cx.update(|window, _| {
            assert!(handle.is_open(), "offscreen focus did not dismiss the menu");
            assert!(copy_focus.is_focused(window), "End revealed task-ID copy");
        });
        cx.simulate_keystrokes("space");
        frame(cx);
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(session.to_string()),
            "keyboard copied the actual selected task ID"
        );
        cx.simulate_keystrokes("shift-tab");
        frame(cx);
        cx.update(|window, _| {
            assert!(
                last_stop.is_focused(window),
                "reverse Tab reached offscreen stop"
            );
        });
        cx.simulate_keystrokes("shift-tab");
        frame(cx);
        cx.update(|window, _| {
            assert!(last_row.is_focused(window), "reverse Tab reached task open");
        });
        cx.simulate_keystrokes("home");
        frame(cx);
        cx.update(|window, _| {
            assert!(
                first_action.is_focused(window),
                "Home revealed first action"
            );
        });
        cx.simulate_keystrokes("tab");
        frame(cx);
        cx.update(|window, _| {
            assert!(
                second_action.is_focused(window),
                "Tab reached next environment action"
            );
        });
        eprintln!("REAL INFO KEYBOARD: End/copy/Shift-Tab/Home/Tab passed across virtual rows");
        cx.update(|window, cx| handle.close(window, cx));
    }
}
