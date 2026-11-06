//! Read-only side chats: quick questions that branch off a task without
//! touching its conversation, the way a branch leaves trunk alone.
//!
//! A side chat is a hidden [`AgentSession`] whose `side_chat` origin names its
//! parent task and, when it was started from selected transcript text, the
//! quoted excerpt. It runs through the ordinary runtime pipeline with the
//! driver's read-only tool policy, lives in the right panel, and never appears
//! in task lists. See `docs/side-chat.md`.

use gpui::FollowMode;

use super::*;
use crate::md::render::{InlineAnchor, InlineAnchors};
use crate::model::{SideChatAnchor, SideChatAnchorSpan, SideChatOrigin};

pub(super) const SIDE_CHAT_MENU_ID: &str = "side-chats";
/// Private link scheme the transcript's quote anchors route through.
const SIDE_CHAT_LINK_PREFIX: &str = "raku-side-chat:";
/// Bound on the parent transcript a first side-chat prompt carries.
const SNAPSHOT_LIMIT: usize = 32 * 1024;
/// Bound on a quoted excerpt.
const QUOTE_LIMIT: usize = 8 * 1024;
const TITLE_CHARS: usize = 40;
/// Bound on the quote a header chip's tooltip shows.
const QUOTE_TOOLTIP_LIMIT: usize = 1200;

/// Everything the desktop keeps for side chats beyond their sessions.
pub(super) struct SideChatState {
    /// One composer shared by every side chat; drafts swap in and out.
    pub(super) composer: Entity<ComposerInput>,
    pub(super) selection: TranscriptSelection,
    /// The side chat whose draft the composer currently holds.
    composer_target: Option<Uuid>,
    drafts: HashMap<Uuid, String>,
    lists: RefCell<HashMap<Uuid, SideChatList>>,
    scrollbar: Rc<ScrollbarState>,
    /// Parent and anchor lookups, rebuilt only when side chats change.
    index: RefCell<Option<Rc<SideChatIndex>>>,
}

impl SideChatState {
    pub(super) fn new(composer: Entity<ComposerInput>) -> Self {
        Self {
            composer,
            selection: TranscriptSelection::default(),
            composer_target: None,
            drafts: HashMap::new(),
            lists: RefCell::new(HashMap::new()),
            scrollbar: ScrollbarState::new(),
            index: RefCell::new(None),
        }
    }
}

/// One side chat's virtualized transcript.
struct SideChatList {
    state: ListState,
    rows: Rc<[SideChatRow]>,
    /// Byte length of the newest row's text, so a growing reply remeasures
    /// only its own row.
    tail_len: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SideChatRow {
    Message(usize),
    Working,
}

#[derive(Debug, Default)]
pub(super) struct SideChatIndex {
    /// Parent task → its side chats, newest first.
    by_parent: HashMap<Uuid, Vec<Uuid>>,
    /// Parent message → the dashed anchors drawn over its text.
    anchors: HashMap<Uuid, InlineAnchors>,
}

pub(super) fn build_side_chat_index(sessions: &[AgentSession]) -> SideChatIndex {
    let mut chats = sessions
        .iter()
        .filter_map(|session| Some((session, session.side_chat.as_ref()?)))
        .collect::<Vec<_>>();
    chats.sort_by_key(|(session, _)| std::cmp::Reverse(session.created_at));
    let mut index = SideChatIndex::default();
    let mut anchors: HashMap<Uuid, Vec<InlineAnchor>> = HashMap::new();
    for (session, origin) in chats {
        index
            .by_parent
            .entry(origin.parent_session_id)
            .or_default()
            .push(session.id);
        let Some(anchor) = &origin.anchor else {
            continue;
        };
        let target: Rc<str> = Rc::from(side_chat_link(session.id));
        anchors.entry(anchor.message_id).or_default().extend(
            anchor
                .spans
                .iter()
                .filter(|span| span.start < span.end)
                .map(|span| InlineAnchor {
                    index: span.index,
                    range: span.start..span.end,
                    target: target.clone(),
                }),
        );
    }
    index.anchors = anchors
        .into_iter()
        .filter(|(_, anchors)| !anchors.is_empty())
        .map(|(message, anchors)| (message, Rc::from(anchors)))
        .collect();
    index
}

pub(super) fn side_chat_link(id: Uuid) -> String {
    format!("{SIDE_CHAT_LINK_PREFIX}{id}")
}

pub(super) fn side_chat_link_target(target: &str) -> Option<Uuid> {
    Uuid::parse_str(target.strip_prefix(SIDE_CHAT_LINK_PREFIX)?).ok()
}

/// Where side chats run. An explicit, still-available setting wins; otherwise
/// a DeepSeek model found through Pi, then Pi's own default, then the parent
/// task's provider and model.
pub(super) fn resolve_side_chat_target(
    probes: &[ProviderProbe],
    disabled: &[ProviderKind],
    setting_provider: Option<ProviderKind>,
    setting_model: Option<&str>,
    fallback: Option<(ProviderKind, Option<String>)>,
) -> Option<(ProviderKind, Option<String>)> {
    let usable = |provider: ProviderKind| {
        (!disabled.contains(&provider))
            .then(|| {
                probes
                    .iter()
                    .find(|probe| probe.provider == provider && probe.path.is_some())
            })
            .flatten()
    };
    if let Some(provider) = setting_provider
        && let Some(probe) = usable(provider)
    {
        match setting_model {
            Some(model) if probe.models.is_empty() || probe.model(model).is_some() => {
                return Some((provider, Some(model.to_owned())));
            }
            Some(_) => {}
            None => {
                return Some((
                    provider,
                    probe.preferred_model().map(|model| model.id.clone()),
                ));
            }
        }
    }
    if let Some(probe) = usable(ProviderKind::Pi) {
        let is_deepseek = |model: &&ProviderModel| model.id.to_lowercase().contains("deepseek");
        let model = probe
            .preferred_model()
            .filter(is_deepseek)
            .or_else(|| probe.models.iter().find(is_deepseek))
            .or_else(|| probe.preferred_model())
            .map(|model| model.id.clone());
        return Some((ProviderKind::Pi, model));
    }
    fallback.filter(|(provider, _)| usable(*provider).is_some())
}

/// The least privileged interactive mode the provider accepts. The driver's
/// read-only tool policy is what actually keeps a side chat from writing.
fn side_chat_runtime_mode(provider: ProviderKind) -> RuntimeMode {
    [RuntimeMode::Ask, RuntimeMode::FullAccess]
        .into_iter()
        .find(|mode| provider.validate_access_mode(*mode).is_ok())
        .unwrap_or_default()
}

/// What a side chat is called: its quote, a generated or prompt-derived
/// title, or "Side chat" before the first prompt.
fn side_chat_display_title(session: &AgentSession) -> String {
    let title = session.display_title();
    if title == AgentSession::DEFAULT_TITLE {
        tr!("side_chat.title")
    } else {
        title.to_owned()
    }
}

fn side_chat_title(quote: &str) -> String {
    let line = quote.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title = line.chars().take(TITLE_CHARS).collect::<String>();
    if line.chars().count() > TITLE_CHARS {
        title.push('…');
    }
    title
}

/// The longest suffix of `text` within `limit` bytes, on a char boundary.
fn tail_within(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn head_within(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Hidden context for a side chat's first prompt: the parent conversation so
/// far (newest messages kept within [`SNAPSHOT_LIMIT`]) and the quoted
/// excerpt. The user's bubble shows only what they typed.
pub(super) fn side_chat_preamble(parent: &AgentSession, quote: Option<&str>) -> String {
    let mut entries = Vec::new();
    let mut used = 0;
    let mut truncated = false;
    for message in parent.messages.iter().rev() {
        let role = match message.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Agent",
            MessageRole::System => continue,
        };
        let text = message.visible_content().trim();
        if text.is_empty() {
            continue;
        }
        let remaining = SNAPSHOT_LIMIT.saturating_sub(used);
        if remaining < 64 {
            truncated = true;
            break;
        }
        let text = if text.len() + role.len() + 4 > remaining {
            truncated = true;
            tail_within(text, remaining - role.len() - 4)
        } else {
            text
        };
        let entry = format!("{role}: {text}\n\n");
        used += entry.len();
        entries.push(entry);
    }
    entries.reverse();
    let mut out = String::from(crate::model::SIDE_CHAT_CONTEXT_PREFIX);
    out.push_str(
        "\nThis is the conversation the user is having with a separate coding \
         agent, up to now. Their question below is about it: answer from this shared context \
         first. You are not that agent and cannot act for it.\n\n",
    );
    if !entries.is_empty() {
        out.push_str("<conversation>\n");
        if truncated {
            out.push_str("[earlier messages omitted]\n\n");
        }
        for entry in entries {
            out.push_str(&entry);
        }
        out.push_str("</conversation>\n\n");
    }
    if let Some(quote) = quote.map(str::trim).filter(|quote| !quote.is_empty()) {
        out.push_str("The user is asking about this excerpt:\n<excerpt>\n");
        out.push_str(head_within(quote, QUOTE_LIMIT));
        out.push_str("\n</excerpt>\n\n");
    }
    // `SIDE_CHAT_QUESTION_MARKER`, minus the newline the excerpt or
    // conversation block already ends with.
    out.push_str(&crate::model::SIDE_CHAT_QUESTION_MARKER[1..]);
    out
}

/// The quote a side chat is about, as a pasted-text style chip: its first
/// line, the whole quote on hover. Not interactive — the quote stays readable
/// in the transcript, where its dashed link lives.
fn side_chat_quote_chip(theme: &Theme, key: &str, quote: &str) -> Stateful<Div> {
    let title = SharedString::from(single_line_label(quote));
    let trimmed = quote.trim();
    let shown = head_within(trimmed, QUOTE_TOOLTIP_LIMIT);
    let full = if shown.len() < trimmed.len() {
        format!("{shown}…")
    } else {
        shown.to_owned()
    };
    pasted_text_chip(theme, &title)
        .id(SharedString::from(format!("side-chat-quote-{key}")))
        .tooltip(Tooltip::text(full))
}

impl Raku {
    pub(super) fn invalidate_side_chat_index(&self) {
        self.side_chat.index.borrow_mut().take();
    }

    fn side_chat_index(&self) -> Rc<SideChatIndex> {
        self.side_chat
            .index
            .borrow_mut()
            .get_or_insert_with(|| Rc::new(build_side_chat_index(&self.state.sessions)))
            .clone()
    }

    /// Side chats branched off `parent`, newest first.
    pub(super) fn side_chat_ids_for(&self, parent: Uuid) -> Vec<Uuid> {
        self.side_chat_index()
            .by_parent
            .get(&parent)
            .cloned()
            .unwrap_or_default()
    }

    /// The dashed side-chat anchors drawn over a parent message, if any.
    pub(super) fn side_chat_anchors_for(&self, message_id: Uuid) -> Option<InlineAnchors> {
        self.side_chat_index().anchors.get(&message_id).cloned()
    }

    /// The live title for a side chat's tab.
    pub(super) fn side_chat_title(&self, id: Uuid) -> Option<String> {
        self.side_chat_session(id).map(side_chat_display_title)
    }

    fn side_chat_session(&self, id: Uuid) -> Option<&AgentSession> {
        self.state
            .sessions
            .iter()
            .find(|session| session.id == id && session.is_side_chat())
    }

    fn standard_side_chat_id(&self, parent: Uuid) -> Option<Uuid> {
        self.side_chat_ids_for(parent).into_iter().find(|id| {
            self.side_chat_session(*id)
                .and_then(|session| session.side_chat.as_ref())
                .is_some_and(|origin| origin.anchor.is_none())
        })
    }

    /// The task side chats branch from: the selected task, never a side chat.
    fn side_chat_parent_session(&self) -> Option<&AgentSession> {
        self.selected_session()
            .filter(|session| !session.is_side_chat())
    }

    pub(super) fn side_chat_target(
        &self,
        parent: Option<&AgentSession>,
    ) -> Option<(ProviderKind, Option<String>)> {
        resolve_side_chat_target(
            &self.probes,
            &self.state.disabled_providers,
            self.state.side_chat_provider,
            self.state.side_chat_model.as_deref(),
            parent.map(|parent| {
                (
                    parent.provider,
                    self.model_for_session(parent).map(str::to_owned),
                )
            }),
        )
    }

    /// Create a side chat off `parent_id`. Without an anchor this returns the
    /// task's standard side chat, creating it on first use.
    pub(super) fn create_side_chat(
        &mut self,
        parent_id: Uuid,
        anchor: Option<SideChatAnchor>,
        cx: &mut Context<Self>,
    ) -> Option<Uuid> {
        if anchor.is_none()
            && let Some(id) = self.standard_side_chat_id(parent_id)
        {
            return Some(id);
        }
        let parent = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == parent_id && !session.is_side_chat())?;
        let Some((provider, model)) = self.side_chat_target(Some(parent)) else {
            self.show_toast(tr!("side_chat.no_provider"));
            cx.notify();
            return None;
        };
        let reasoning_effort = model
            .as_deref()
            .and_then(|model| self.provider_probe(provider)?.model(model))
            .filter(|model| {
                model
                    .reasoning_efforts
                    .iter()
                    .any(|option| option.id == "low")
            })
            // A side question wants a quick answer.
            .map(|_| "low".to_owned());
        let mut session = AgentSession::new(parent.project_id, provider);
        session.workspace = match &parent.workspace {
            // A side chat must never create a worktree of its own.
            SessionWorkspace::NewWorktree { .. } => SessionWorkspace::Local,
            workspace => workspace.clone(),
        };
        session.runtime_mode = side_chat_runtime_mode(provider);
        session.model = model;
        session.reasoning_effort = reasoning_effort;
        // A quoted side chat is named by its quote. A standard one keeps the
        // default title, so its first prompt and then the provider name it.
        if let Some(title) = anchor
            .as_ref()
            .map(|anchor| side_chat_title(&anchor.quote))
            .filter(|title| !title.is_empty())
        {
            session.set_title(title);
        }
        session.side_chat = Some(SideChatOrigin {
            parent_session_id: parent_id,
            anchor,
        });
        let id = session.id;
        self.state.push_session(session);
        self.invalidate_side_chat_index();
        cx.notify();
        Some(id)
    }

    /// Show `id` in the right panel and focus its composer.
    pub(super) fn open_side_chat(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.side_chat_session(id) else {
            return;
        };
        let title = SharedString::from(side_chat_display_title(session));
        if let Some(parent) = session.side_chat_parent()
            && self.state.selected_session != Some(parent)
        {
            self.select_session(parent, cx);
        }
        self.ensure_session_loaded(id, cx);
        self.switch_side_chat_composer(Some(id), cx);
        self.open_right_panel_surface(RightPanelSurface::SideChat { id, title }, cx);
        let focus = self.side_chat.composer.read(cx).focus();
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Open the selected task's standard side chat.
    pub(super) fn open_standard_side_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(parent) = self.side_chat_parent_session().map(|session| session.id) else {
            return;
        };
        if let Some(id) = self.create_side_chat(parent, None, cx) {
            self.open_side_chat(id, window, cx);
        }
    }

    /// Toggle the standard side chat: close it when it is the panel's active
    /// tab, open it otherwise.
    pub(super) fn toggle_side_chat_action(
        &mut self,
        _: &crate::ToggleSideChat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let standard = self
            .side_chat_parent_session()
            .and_then(|parent| self.standard_side_chat_id(parent.id));
        if self.right_panel_visible
            && let Some(standard) = standard
            && let Some(index) = self.right_panel_active_surface
            && matches!(
                self.right_panel_surfaces.get(index),
                Some(RightPanelSurface::SideChat { id, .. }) if *id == standard
            )
        {
            self.close_right_panel_surface(index, cx);
            let focus = self.composer_focus(cx);
            window.focus(&focus, cx);
            return;
        }
        self.open_standard_side_chat(window, cx);
    }

    /// Start a side chat about the transcript selection inside `message_id`.
    pub(super) fn start_side_chat_from_selection(
        &mut self,
        message_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(parent) = self.side_chat_parent_session().map(|session| session.id) else {
            return;
        };
        let anchor = {
            let selection = self.transcript_selection.selection.borrow();
            let Some(quote) = selection.selected_text() else {
                return;
            };
            let row = format!("message-{message_id}");
            let spans = selection
                .spans()
                .iter()
                .filter(|span| span.key.row.as_ref() == row.as_str() && !span.range.is_empty())
                .map(|span| SideChatAnchorSpan {
                    index: span.key.index,
                    start: span.range.start,
                    end: span.range.end,
                })
                .collect();
            SideChatAnchor {
                message_id,
                quote: head_within(quote.trim(), QUOTE_LIMIT).to_owned(),
                spans,
            }
        };
        self.transcript_selection.selection.borrow_mut().clear();
        if let Some(id) = self.create_side_chat(parent, Some(anchor), cx) {
            self.open_side_chat(id, window, cx);
        }
    }

    /// Delete a side chat, its runtime, and any panel tab showing it.
    pub(super) fn remove_side_chat(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(index) = self
            .state
            .sessions
            .iter()
            .position(|session| session.id == id && session.is_side_chat())
        else {
            return;
        };
        self.submission_preparations.remove(&id);
        self.reset_session_runtime(id);
        self.state.sessions.remove(index);
        if let Err(error) = self.store.remove_session(id) {
            self.show_toast(tr!("errors.save_local_state", error = error));
        }
        self.side_chat.drafts.remove(&id);
        self.side_chat.lists.borrow_mut().remove(&id);
        if self.side_chat.composer_target == Some(id) {
            self.side_chat.composer_target = None;
            self.side_chat
                .composer
                .update(cx, |composer, cx| composer.clear(cx));
        }
        let is_tab = |surface: &RightPanelSurface| matches!(surface, RightPanelSurface::SideChat { id: tab, .. } if *tab == id);
        if let Some(position) = self.right_panel_surfaces.iter().position(is_tab) {
            self.close_right_panel_surface(position, cx);
        }
        for state in self.right_panel_session_states.values_mut() {
            while let Some(position) = state.surfaces.iter().position(is_tab) {
                state.surfaces.remove(position);
                state.active_surface = match state.active_surface {
                    _ if state.surfaces.is_empty() => None,
                    Some(active) if active > position => Some(active - 1),
                    Some(active) => Some(active.min(state.surfaces.len() - 1)),
                    None => None,
                };
            }
        }
        self.invalidate_side_chat_index();
        self.save();
        cx.notify();
    }

    /// Park the composer's text as the current side chat's draft and load
    /// `target`'s draft in its place.
    fn switch_side_chat_composer(&mut self, target: Option<Uuid>, cx: &mut Context<Self>) {
        if self.side_chat.composer_target == target {
            return;
        }
        let current = self.side_chat.composer.read(cx).content(cx).to_owned();
        if let Some(previous) = self.side_chat.composer_target {
            if current.trim().is_empty() {
                self.side_chat.drafts.remove(&previous);
            } else {
                self.side_chat.drafts.insert(previous, current);
            }
        }
        let next = target
            .and_then(|target| self.side_chat.drafts.get(&target).cloned())
            .unwrap_or_default();
        self.side_chat.composer_target = target;
        self.side_chat
            .composer
            .update(cx, |composer, cx| composer.set_content(next, cx));
    }

    /// Send the side-chat composer's prompt to its side chat.
    pub(super) fn submit_side_chat(&mut self, prompt: String, cx: &mut Context<Self>) {
        let prompt = prompt.trim().to_owned();
        let Some(id) = self.side_chat.composer_target else {
            return;
        };
        let Some(session) = self.side_chat_session(id) else {
            return;
        };
        if prompt.is_empty() || !session.detail_loaded {
            return;
        }
        let busy = session.is_busy() || self.submission_preparations.contains(&id);
        let first = session.messages.is_empty() && session.turns.is_empty();
        if busy {
            self.side_chat.drafts.remove(&id);
            self.enqueue_follow_up_submission(id, ComposerSubmission::plain(prompt), cx);
            return;
        }
        let submission = if first {
            let quote = session
                .side_chat
                .as_ref()
                .and_then(|origin| origin.anchor.as_ref())
                .map(|anchor| anchor.quote.clone());
            let preamble = session
                .side_chat_parent()
                .and_then(|parent| {
                    self.state
                        .sessions
                        .iter()
                        .find(|session| session.id == parent)
                })
                .map(|parent| side_chat_preamble(parent, quote.as_deref()))
                .unwrap_or_default();
            ComposerSubmission {
                prompt: format!("{preamble}{prompt}"),
                display_content: Some(prompt),
                attachments: Vec::new(),
            }
        } else {
            ComposerSubmission::plain(prompt)
        };
        self.side_chat.drafts.remove(&id);
        self.submit_submission_for_session(id, submission, cx);
        cx.notify();
    }

    fn submit_side_chat_composer(&mut self, cx: &mut Context<Self>) {
        let prompt = self.side_chat.composer.read(cx).content(cx).to_owned();
        if prompt.trim().is_empty() {
            return;
        }
        self.side_chat
            .composer
            .update(cx, |composer, cx| composer.clear(cx));
        self.submit_side_chat(prompt, cx);
    }

    /// Disclose or hide a failed reply's provider detail, in the transcript
    /// or a side chat, and re-lay out the row it changes.
    pub(super) fn toggle_message_error(&mut self, message_id: Uuid, cx: &mut Context<Self>) {
        if !self.expanded_error_details.remove(&message_id) {
            self.expanded_error_details.insert(message_id);
        }
        if let Some(index) = self.selected_session().and_then(|session| {
            session
                .messages
                .iter()
                .position(|message| message.id == message_id)
        }) {
            self.remeasure_transcript_message(index);
        }
        for (chat_id, list) in self.side_chat.lists.borrow().iter() {
            let Some(index) = self.side_chat_session(*chat_id).and_then(|session| {
                session
                    .messages
                    .iter()
                    .position(|message| message.id == message_id)
            }) else {
                continue;
            };
            if let Some(row) = list
                .rows
                .iter()
                .position(|row| *row == SideChatRow::Message(index))
            {
                list.state.remeasure_items(row..row + 1);
            }
        }
        cx.notify();
    }

    // ── Panel surface ──────────────────────────────────────────────────────

    fn side_chat_rows(session: &AgentSession, busy: bool) -> (Vec<SideChatRow>, usize) {
        let mut rows = session
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| !message.visible_content().trim().is_empty())
            .map(|(index, _)| SideChatRow::Message(index))
            .collect::<Vec<_>>();
        let replying = session
            .messages
            .last()
            .is_some_and(|message| message.role == MessageRole::Assistant && message.streaming);
        if busy && !replying {
            rows.push(SideChatRow::Working);
        }
        // The tail row re-lays out when its text grows or when the turn
        // settles and its reply gains a footer.
        let tail_len = match rows.last() {
            Some(SideChatRow::Message(index)) => {
                session.messages[*index].visible_content().len() * 2 + usize::from(busy)
            }
            _ => 0,
        };
        (rows, tail_len)
    }

    /// Bring a side chat's list state in line with its rows. Rows only ever
    /// append or change at the tail, so the splice touches the tail alone.
    fn sync_side_chat_list(&self, id: Uuid, rows: Vec<SideChatRow>, tail_len: usize) -> ListState {
        let mut lists = self.side_chat.lists.borrow_mut();
        let list = lists.entry(id).or_insert_with(|| {
            let state = ListState::new(0, ListAlignment::Top, px(400.0));
            state.set_follow_mode(FollowMode::Tail);
            SideChatList {
                state,
                rows: Rc::from([]),
                tail_len: 0,
            }
        });
        if *list.rows != *rows {
            let old = list.rows.len();
            let prefix = list
                .rows
                .iter()
                .zip(&rows)
                .take_while(|(old, new)| old == new)
                .count()
                // The previous tail may have changed shape (a working row
                // becoming a reply), so it is always re-laid out.
                .min(old.saturating_sub(1));
            list.state.splice(prefix..old, rows.len() - prefix);
            list.rows = Rc::from(rows);
        } else if list.tail_len != tail_len && !list.rows.is_empty() {
            let last = list.rows.len() - 1;
            list.state.remeasure_items(last..last + 1);
        }
        list.tail_len = tail_len;
        list.state.clone()
    }

    pub(super) fn render_side_chat_surface(
        &mut self,
        id: Uuid,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(session) = self.side_chat_session(id) else {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .px(px(28.0))
                .text_center()
                .child(
                    div()
                        .text_size(sp(13.5))
                        .text_color(theme.text_secondary)
                        .child(tr!("side_chat.missing")),
                )
                .child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(theme.text_tertiary)
                        .child(tr!("side_chat.missing_description")),
                )
                .into_any_element();
        };
        let loaded = session.detail_loaded;
        let busy = session.is_busy() || self.submission_preparations.contains(&id);
        // Until the first prompt is sent, the quote rides in the composer
        // like a staged pasted-text attachment; afterwards it sits above that
        // prompt in the transcript.
        let pending_quote = session
            .side_chat
            .as_ref()
            .and_then(|origin| origin.anchor.as_ref())
            .filter(|_| session.messages.is_empty() && session.turns.is_empty())
            .map(|anchor| anchor.quote.clone());
        let queued = session.queued_messages.len();
        let (rows, tail_len) = Self::side_chat_rows(session, busy);
        let empty = rows.is_empty();
        if !loaded {
            self.ensure_session_loaded(id, cx);
        }
        if self.side_chat.composer_target != Some(id) {
            self.switch_side_chat_composer(Some(id), cx);
        }
        let list_state = self.sync_side_chat_list(id, rows, tail_len);
        let weak = cx.entity().downgrade();

        let body = if empty {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .px(px(28.0))
                .text_center()
                .child(icon("icons/ant/messages.svg", 22.0, theme.text_tertiary))
                .child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(theme.text_tertiary)
                        .child(if loaded {
                            tr!("side_chat.empty_description")
                        } else {
                            tr!("side_chat.loading")
                        }),
                )
                .into_any_element()
        } else {
            let list_weak = weak.clone();
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .relative()
                .child(md::render::frame_reset(self.side_chat.selection.clone()))
                .child(
                    list(list_state.clone(), move |index, window, cx| {
                        list_weak
                            .upgrade()
                            .map(|entity| {
                                entity.update(cx, |this, cx| {
                                    this.side_chat_row(id, index, window, cx)
                                })
                            })
                            .unwrap_or_else(|| div().into_any_element())
                    })
                    .size_full(),
                )
                .child(scrollbar::vertical(&list_state, &self.side_chat.scrollbar))
                .child({
                    let selection = self.side_chat.selection.clone();
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, _| {
                            md::render::install_selection_input(window, &selection)
                        },
                    )
                    .absolute()
                    .w(px(0.0))
                    .h(px(0.0))
                })
                .into_any_element()
        };

        let action = if busy {
            div()
                .id(SharedString::from(format!("side-chat-stop-{id}")))
                .track_focus(&self.transcript_control_focus(format!("side-chat-stop-{id}"), cx))
                .tab_index(0)
                .size(px(28.0))
                .flex_none()
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.inverse)
                .cursor_pointer()
                .focus_visible(|style| style.border_2().border_color(theme.accent))
                .tooltip(Tooltip::text(tr!("side_chat.stop")))
                .child(icon("icons/stop-filled.svg", 11.0, theme.on_inverse))
                .on_click(cx.listener(move |this, _, _, cx| this.cancel_session_turn(id, cx)))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        this.cancel_session_turn(id, cx);
                        cx.stop_propagation();
                    }
                }))
        } else {
            div()
                .id(SharedString::from(format!("side-chat-send-{id}")))
                .track_focus(&self.transcript_control_focus(format!("side-chat-send-{id}"), cx))
                .tab_index(0)
                .size(px(28.0))
                .flex_none()
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.inverse)
                .cursor_pointer()
                .focus_visible(|style| style.border_2().border_color(theme.accent))
                .tooltip(Tooltip::text(tr!("side_chat.send")))
                .child(icon("icons/arrow-up.svg", 14.0, theme.on_inverse))
                .on_click(cx.listener(|this, _, _, cx| this.submit_side_chat_composer(cx)))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        this.submit_side_chat_composer(cx);
                        cx.stop_propagation();
                    }
                }))
        };

        let composer = div().flex_none().w_full().p(px(10.0)).child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.composer)
                .pt(px(6.0))
                .pb(px(6.0))
                .pr(px(6.0))
                .when_some(pending_quote, |card, quote| {
                    card.child(div().px(px(10.0)).pt(px(4.0)).pb(px(2.0)).flex().child(
                        side_chat_quote_chip(&theme, &format!("composer-{id}"), &quote),
                    ))
                })
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_end()
                        .gap(px(6.0))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .max_h(px(180.0))
                                .child(self.side_chat.composer.clone()),
                        )
                        .child(action),
                )
                .when(queued > 0, |composer| {
                    composer.child(
                        div()
                            .px(px(12.0))
                            .text_size(sp(11.5))
                            .text_color(theme.text_tertiary)
                            .child(tr!("side_chat.queued", count = queued)),
                    )
                }),
        );

        div()
            .id(SharedString::from(format!("side-chat-surface-{id}")))
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .flex_col()
            // Escape from the composer is routed by `cancel_turn_action`.
            .child(body)
            .child(composer)
            .into_any_element()
    }

    fn side_chat_row(
        &mut self,
        id: Uuid,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let row = self
            .side_chat
            .lists
            .borrow()
            .get(&id)
            .and_then(|list| list.rows.get(index).copied());
        let Some(session) = self.side_chat_session(id) else {
            return div().into_any_element();
        };
        let element = match row {
            None => div().into_any_element(),
            Some(SideChatRow::Working) => {
                let activity = session
                    .transcript_blocks
                    .last()
                    .and_then(|block| block.activities.iter().rev().find(|item| !item.complete))
                    .map(|item| item.title.clone())
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| tr!("side_chat.thinking"));
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(sp(12.5))
                    .text_color(theme.text_tertiary)
                    .child(pulse_dot(6.0, theme.accent))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(single_line_label(&activity)),
                    )
                    .into_any_element()
            }
            Some(SideChatRow::Message(message_index)) => {
                let Some(message) = session.messages.get(message_index).cloned() else {
                    return div().into_any_element();
                };
                let palette = MarkdownPalette::from_theme(&theme);
                let metrics =
                    self.transcript_markdown_metrics(if message.role == MessageRole::User {
                        MarkdownMetrics::USER_MESSAGE
                    } else {
                        MarkdownMetrics::BODY
                    });
                let ctx = MarkdownCtx::new(
                    format!("side-chat-{id}-message-{}", message.id),
                    &palette,
                    metrics,
                    self.side_chat.selection.clone(),
                )
                .with_math_enabled(self.state.render_math)
                .with_link_handler(self.markdown_link_handler.clone())
                .with_file_links(self.markdown_file_links.clone())
                // Settled text only; a dissolve would hold the pulse clock for
                // a panel the user is mostly reading, not watching.
                .with_streaming_animation(false);
                let body = if let Some(error) = &message.error {
                    let focus = self.transcript_control_focus(
                        format!("message-error-details-{}", message.id),
                        cx,
                    );
                    render_error_reply(
                        message.id,
                        message.visible_content(),
                        error,
                        self.expanded_error_details.contains(&message.id),
                        focus,
                        cx.entity().downgrade(),
                        &theme,
                        &ctx,
                    )
                } else {
                    let mut markdown = self.message_markdown.borrow_mut();
                    let view = markdown.entry(message.id).or_default();
                    view.set_text(message.visible_content(), message.streaming);
                    md::render::markdown(view, &ctx).unwrap_or_else(|| {
                        md::render::plain_text(
                            message.visible_content().to_owned(),
                            md::render::SANS_FAMILY,
                            FontWeight::NORMAL,
                            theme.text,
                            &ctx,
                        )
                    })
                };
                let quote = (message.role == MessageRole::User)
                    .then(|| {
                        let first_user = session
                            .messages
                            .iter()
                            .position(|candidate| candidate.role == MessageRole::User);
                        session
                            .side_chat
                            .as_ref()
                            .and_then(|origin| origin.anchor.as_ref())
                            .filter(|_| first_user == Some(message_index))
                            .map(|anchor| anchor.quote.clone())
                    })
                    .flatten();
                match message.role {
                    MessageRole::User => div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .items_end()
                        .gap(px(6.0))
                        .when_some(quote, |column, quote| {
                            column.child(side_chat_quote_chip(
                                &theme,
                                &format!("message-{}", message.id),
                                &quote,
                            ))
                        })
                        .child(
                            div()
                                .max_w(gpui::relative(0.88))
                                .min_w_0()
                                .px(px(12.0))
                                .py(px(8.0))
                                .rounded(px(12.0))
                                .bg(theme.raised)
                                .child(body),
                        )
                        .into_any_element(),
                    MessageRole::Assistant => {
                        let footer = self.side_chat_reply_footer(session, message_index, cx);
                        div()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(body)
                            .children(footer)
                            .into_any_element()
                    }
                    MessageRole::System => div()
                        .w_full()
                        .text_size(sp(12.0))
                        .text_color(theme.text_tertiary)
                        .child(body)
                        .into_any_element(),
                }
            }
        };
        let _ = window;
        div()
            .id(("side-chat-row", index))
            .w_full()
            .px(px(14.0))
            .py(px(6.0))
            .when(index == 0, |row| row.pt(px(12.0)))
            .child(element)
            .into_any_element()
    }

    /// Copy and time under the last reply of a settled side-chat turn,
    /// copying every reply message of that turn.
    fn side_chat_reply_footer(
        &self,
        session: &AgentSession,
        message_index: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let message = session.messages.get(message_index)?;
        let ends_turn = session
            .messages
            .get(message_index + 1)
            .is_none_or(|next| next.role == MessageRole::User);
        let settled = !message.streaming
            && !(session.is_busy() && message_index + 1 == session.messages.len());
        if !ends_turn || !settled {
            return None;
        }
        let start = session.messages[..message_index]
            .iter()
            .rposition(|candidate| candidate.role == MessageRole::User)
            .map_or(0, |index| index + 1);
        let copy_content = session.messages[start..=message_index]
            .iter()
            .filter(|candidate| candidate.role == MessageRole::Assistant)
            .map(|candidate| candidate.visible_content().trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let theme = Theme::current(cx);
        let message_id = message.id;
        let copied = self.copied_message_feedback.contains_key(&message_id);
        let focus = self.transcript_control_focus(format!("side-chat-copy-{message_id}"), cx);
        let color = theme.text_tertiary;
        let click_content = SharedString::from(copy_content);
        let key_content = click_content.clone();
        let click_raku = cx.entity().downgrade();
        let key_raku = click_raku.clone();
        let copy = div()
            .id(SharedString::from(format!("side-chat-copy-{message_id}")))
            .track_focus(&focus)
            .tab_index(0)
            .size(px(27.0))
            .flex_none()
            .rounded(px(8.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .hover(|style| style.bg(theme.overlay_strong))
            .focus_visible(|style| style.bg(theme.overlay_strong))
            .tooltip(Tooltip::text(if copied {
                tr!("common.copied")
            } else {
                tr!("common.copy_message")
            }))
            .child(icon(
                if copied {
                    "icons/check.svg"
                } else {
                    "icons/copy.svg"
                },
                14.0,
                color,
            ))
            .on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(click_content.to_string()));
                let _ = click_raku.update(cx, |this, cx| this.show_message_copied(message_id, cx));
            })
            .on_key_down(move |event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(key_content.to_string()));
                    let _ =
                        key_raku.update(cx, |this, cx| this.show_message_copied(message_id, cx));
                    cx.stop_propagation();
                }
            });
        let model_label = self.turn_model_label(session, message_index);
        // Like a main response footer: copy, then model · effort and time,
        // all on the left.
        Some(
            div()
                .h(px(27.0))
                .ml(-px(7.0))
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(2.0))
                .child(copy)
                .when_some(model_label, |footer, label| {
                    footer.child(
                        div()
                            .min_w_0()
                            .px(px(4.0))
                            .text_size(sp(12.5))
                            .text_color(color)
                            .truncate()
                            .child(label),
                    )
                })
                .child(
                    div()
                        .flex_none()
                        .px(px(4.0))
                        .text_size(sp(12.5))
                        .text_color(color)
                        .child(format_message_time(message.created_at)),
                )
                .into_any_element(),
        )
    }

    // ── Header popover ────────────────────────────────────────────────────

    pub(super) fn render_side_chat_trigger(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let parent = self.side_chat_parent_session()?;
        let parent_id = parent.id;
        let theme = Theme::current(cx);
        let replying = self.side_chat_ids_for(parent_id).into_iter().any(|id| {
            self.side_chat_session(id)
                .is_some_and(|session| session.is_busy())
                || self.submission_preparations.contains(&id)
        });
        let handle = self.menu_handle(SIDE_CHAT_MENU_ID, cx);
        let open = handle.is_open();
        let trigger = div()
            .id("side-chats-trigger")
            .size(px(28.0))
            .relative()
            .rounded(px(7.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .when(!open, |style| style.hover(|style| style.bg(theme.overlay)))
            .when(open, |style| style.bg(theme.overlay_strong))
            .tooltip(Tooltip::text(if replying {
                tr!("side_chat.popover_tooltip_replying")
            } else {
                tr!("side_chat.popover_tooltip")
            }))
            .child(icon("icons/ant/messages.svg", 19.0, theme.text))
            .when(replying, |trigger| {
                trigger.child(
                    div()
                        .absolute()
                        .top(px(4.0))
                        .right(px(4.0))
                        .child(pulse_dot(5.0, theme.accent)),
                )
            });
        let weak = cx.entity().downgrade();
        Some(popover(
            trigger,
            &handle,
            MenuAlign::BelowRight,
            move |handle, window, cx| {
                weak.update(cx, |this, cx| {
                    this.render_side_chat_card(handle, window, cx)
                })
                .unwrap_or_else(|_| div().into_any_element())
            },
        ))
    }

    fn render_side_chat_card(
        &mut self,
        handle: &ContextMenuHandle,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(parent_id) = self.side_chat_parent_session().map(|session| session.id) else {
            return div().into_any_element();
        };
        let standard = self.standard_side_chat_id(parent_id);
        let anchored = self
            .side_chat_ids_for(parent_id)
            .into_iter()
            .filter(|id| Some(*id) != standard)
            .filter_map(|id| self.side_chat_session(id))
            .map(|session| SideChatCardEntry::from_session(session, self))
            .collect::<Vec<_>>();
        let standard_entry = standard
            .and_then(|id| self.side_chat_session(id))
            .map(|session| SideChatCardEntry::from_session(session, self))
            .unwrap_or_else(|| SideChatCardEntry {
                id: None,
                title: tr!("side_chat.title"),
                replying: false,
            });
        let mut entries = vec![standard_entry];
        let has_anchored = !anchored.is_empty();
        entries.extend(anchored);
        let focuses = entries
            .iter()
            .map(|entry| {
                let key = entry
                    .id
                    .map_or_else(|| "standard".to_owned(), |id| id.to_string());
                self.transcript_control_focus(format!("side-chat-card-row-{key}"), cx)
            })
            .collect::<Vec<_>>();
        let weak = cx.entity().downgrade();
        let mut rows = Vec::new();
        for (index, entry) in entries.into_iter().enumerate() {
            if index == 1 {
                rows.push(
                    div()
                        .h(px(26.0))
                        .px(px(8.0))
                        .flex()
                        .items_end()
                        .pb(px(4.0))
                        .text_size(sp(12.0))
                        .text_color(theme.text_tertiary)
                        .child(tr!("side_chat.from_selections"))
                        .into_any_element(),
                );
            }
            rows.push(render_side_chat_card_row(
                entry,
                index == 0,
                focuses[index].clone(),
                handle.clone(),
                weak.clone(),
                &theme,
            ));
        }
        if !has_anchored {
            rows.push(
                div()
                    .px(px(8.0))
                    .pt(px(6.0))
                    .pb(px(2.0))
                    .text_size(sp(12.0))
                    .line_height(sp(16.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!("side_chat.selection_hint"))
                    .into_any_element(),
            );
        }
        div()
            .id("side-chat-card")
            .track_focus(handle.focus_handle())
            .w(px(320.0))
            .max_h(px(420.0))
            .p(px(6.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border_strong)
            .overflow_y_scroll()
            .bg(theme.menu)
            .shadow_lg()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(rows)
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                let current = focuses.iter().position(|focus| focus.is_focused(window));
                let last = focuses.len().saturating_sub(1);
                let next = match event.keystroke.key.as_str() {
                    "down" => current.map_or(0, |index| (index + 1).min(last)),
                    "up" => current.map_or(last, |index| index.saturating_sub(1)),
                    "home" => 0,
                    "end" => last,
                    _ => return,
                };
                if let Some(focus) = focuses.get(next) {
                    window.focus(focus, cx);
                }
                cx.stop_propagation();
            })
            .into_any_element()
    }
}

impl Raku {
    // ── Settings ──────────────────────────────────────────────────────────

    pub(super) fn render_side_chat_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let explicit = self.state.side_chat_provider;
        let resolved = self.side_chat_target(None);
        let resolved_label = resolved
            .as_ref()
            .map(|(provider, model)| {
                format!(
                    "{} ({})",
                    self.model_display_name(*provider, model.as_deref()),
                    provider.display_name()
                )
            })
            .unwrap_or_else(|| tr!("side_chat.no_provider"));
        let providers = ProviderKind::ALL
            .into_iter()
            .filter(|provider| !self.state.disabled_providers.contains(provider))
            .filter(|provider| {
                self.provider_probe(*provider)
                    .is_some_and(|probe| probe.path.is_some())
            })
            .collect::<Vec<_>>();

        let weak = cx.entity().downgrade();
        let provider_handle = self.menu_handle("side-chat-provider-selector", cx);
        let provider_selector = dropdown_menu(
            MenuChip::new("side-chat-provider-selector")
                .label(explicit.map_or_else(
                    || tr!("side_chat.automatic"),
                    |provider| provider.display_name().to_owned(),
                ))
                .outlined()
                .selected(provider_handle.is_open())
                .w(px(180.0))
                .justify_between(),
            "side-chat-provider-selector-menu",
            &provider_handle,
            MenuAlign::BelowRight,
            move |_| {
                let mut items = vec![{
                    let weak = weak.clone();
                    MenuItem::new(tr!("side_chat.automatic"), move |_, cx| {
                        let _ =
                            weak.update(cx, |this, cx| this.set_side_chat_target(None, None, cx));
                    })
                    .selected(explicit.is_none())
                }];
                items.extend(providers.iter().map(|provider| {
                    let provider = *provider;
                    let weak = weak.clone();
                    MenuItem::new(provider.display_name(), move |_, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.set_side_chat_target(Some(provider), None, cx);
                        });
                    })
                    .selected(explicit == Some(provider))
                }));
                items
            },
        );

        let model_row = explicit.map(|provider| {
            let selected_model = resolved
                .as_ref()
                .filter(|(resolved, _)| *resolved == provider)
                .and_then(|(_, model)| model.clone());
            let models = self
                .provider_probe(provider)
                .map(|probe| {
                    probe
                        .models
                        .iter()
                        .map(|model| {
                            let label = match &model.sub_provider {
                                Some(sub) => format!("{sub} · {}", model.name),
                                None => model.name.clone(),
                            };
                            (model.id.clone(), label)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let weak = cx.entity().downgrade();
            let model_handle = self.menu_handle("side-chat-model-selector", cx);
            let label = self.model_display_name(provider, selected_model.as_deref());
            let selector = dropdown_menu(
                MenuChip::new("side-chat-model-selector")
                    .label(label)
                    .outlined()
                    .selected(model_handle.is_open())
                    .w(px(220.0))
                    .justify_between(),
                "side-chat-model-selector-menu",
                &model_handle,
                MenuAlign::BelowRight,
                move |_| {
                    models
                        .iter()
                        .map(|(id, label)| {
                            let weak = weak.clone();
                            let model = id.clone();
                            MenuItem::new(label.clone(), move |_, cx| {
                                let model = model.clone();
                                let _ = weak.update(cx, |this, cx| {
                                    this.set_side_chat_target(Some(provider), Some(model), cx);
                                });
                            })
                            .selected(selected_model.as_deref() == Some(id.as_str()))
                        })
                        .collect()
                },
            );
            (selector, tr!("side_chat.model_description"))
        });

        let row = |title: String, description: String, control: AnyElement| {
            div()
                .w_full()
                .min_h(px(60.0))
                .px(px(20.0))
                .py(px(12.0))
                .flex()
                .items_center()
                .gap(px(24.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(sp(13.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(title),
                        )
                        .child(
                            div()
                                .mt(px(5.0))
                                .text_size(sp(12.5))
                                .line_height(sp(18.0))
                                .text_color(theme.text_secondary)
                                .child(description),
                        ),
                )
                .child(control)
        };

        div()
            .mt(px(15.0))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .rounded(px(13.0))
                    .overflow_hidden()
                    .bg(theme.raised)
                    .child(row(
                        tr!("side_chat.provider"),
                        tr!("side_chat.provider_description", model = resolved_label),
                        provider_selector.into_any_element(),
                    ))
                    .when_some(model_row, |card, (selector, description)| {
                        card.child(div().mx(px(20.0)).h(px(1.0)).bg(theme.border))
                            .child(row(
                                tr!("side_chat.model"),
                                description,
                                selector.into_any_element(),
                            ))
                    }),
            )
            .child(
                div()
                    .px(px(4.0))
                    .flex()
                    .items_start()
                    .gap(px(8.0))
                    .text_size(sp(12.5))
                    .line_height(sp(18.0))
                    .text_color(theme.text_secondary)
                    .child(div().pt(px(2.0)).child(icon(
                        "icons/eye.svg",
                        13.0,
                        theme.text_secondary,
                    )))
                    .child(div().flex_1().child(tr!("side_chat.read_only_description"))),
            )
            .into_any_element()
    }

    fn set_side_chat_target(
        &mut self,
        provider: Option<ProviderKind>,
        model: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.state.side_chat_provider = provider;
        self.state.side_chat_model = model;
        self.save();
        cx.notify();
    }
}

struct SideChatCardEntry {
    /// `None` for a standard side chat that has not been created yet.
    id: Option<Uuid>,
    title: String,
    replying: bool,
}

impl SideChatCardEntry {
    fn from_session(session: &AgentSession, raku: &Raku) -> Self {
        Self {
            id: Some(session.id),
            title: side_chat_display_title(session),
            replying: session.is_busy() || raku.submission_preparations.contains(&session.id),
        }
    }
}

fn render_side_chat_card_row(
    entry: SideChatCardEntry,
    standard: bool,
    focus: FocusHandle,
    handle: ContextMenuHandle,
    weak: WeakEntity<Raku>,
    theme: &Theme,
) -> AnyElement {
    let key = entry
        .id
        .map_or_else(|| "standard".to_owned(), |id| id.to_string());
    let group = SharedString::from(format!("side-chat-card-group-{key}"));
    let id = entry.id;
    let open = {
        let weak = weak.clone();
        let handle = handle.clone();
        move |window: &mut Window, cx: &mut App| {
            handle.close(window, cx);
            let _ = weak.update(cx, |this, cx| match id {
                Some(id) => this.open_side_chat(id, window, cx),
                None => this.open_standard_side_chat(window, cx),
            });
        }
    };
    let open = Rc::new(open);
    let delete = entry.id.map(|chat_id| {
        let weak = weak.clone();
        div()
            .id(SharedString::from(format!("side-chat-card-delete-{key}")))
            .size(px(24.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .opacity(0.0)
            .group_hover(group.clone(), |style| style.opacity(1.0))
            .hover(|style| style.bg(theme.overlay_strong))
            .tooltip(Tooltip::text(tr!("side_chat.delete")))
            .child(icon("icons/x.svg", 12.0, theme.text_tertiary))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                let _ = weak.update(cx, |this, cx| this.remove_side_chat(chat_id, cx));
            })
    });
    let click_open = open.clone();
    let key_open = open;
    let key_weak = weak;
    div()
        .id(SharedString::from(format!("side-chat-card-row-{key}")))
        .group(group)
        .track_focus(&focus)
        .tab_index(0)
        .w_full()
        .h(px(32.0))
        .px(px(8.0))
        .rounded(px(8.0))
        .flex()
        .items_center()
        .gap(px(9.0))
        .cursor_default()
        .focus_visible(|style| style.border_1().border_color(theme.accent))
        .hover(|style| style.bg(theme.overlay_strong))
        .child(icon(
            if standard {
                "icons/ant/messages.svg"
            } else {
                "icons/ant/blockquote.svg"
            },
            15.0,
            theme.text_secondary,
        ))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_size(sp(13.0))
                .text_color(theme.text)
                .child(single_line_label(&entry.title)),
        )
        // Replying: a pulse dot with the status spelled out on hover, so the
        // row stays one line without leaning on motion alone.
        .when(entry.replying, |row| {
            row.child(
                div()
                    .id(SharedString::from(format!("side-chat-card-replying-{key}")))
                    .flex_none()
                    .px(px(4.0))
                    .tooltip(Tooltip::text(tr!("side_chat.replying")))
                    .child(pulse_dot(5.0, theme.accent)),
            )
        })
        .children(delete)
        .on_click(move |_, window, cx| click_open(window, cx))
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            if event.keystroke.modifiers.modified() {
                return;
            }
            match event.keystroke.key.as_str() {
                "enter" | "space" => {
                    key_open(window, cx);
                    cx.stop_propagation();
                }
                "delete" | "backspace" => {
                    if let Some(chat_id) = id {
                        let _ = key_weak.update(cx, |this, cx| this.remove_side_chat(chat_id, cx));
                        cx.stop_propagation();
                    }
                }
                _ => {}
            }
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(provider: ProviderKind, models: &[(&str, bool)]) -> ProviderProbe {
        ProviderProbe {
            provider,
            installed: true,
            path: Some(PathBuf::from("/usr/bin/true")),
            models: models
                .iter()
                .map(|(id, is_default)| {
                    let mut model = ProviderModel::new(*id, *id);
                    model.is_default = *is_default;
                    model
                })
                .collect(),
            agent_presets: Vec::new(),
        }
    }

    #[test]
    fn side_chats_default_to_deepseek_through_pi() {
        let probes = [
            probe(ProviderKind::Claude, &[("claude-opus-5", true)]),
            probe(
                ProviderKind::Pi,
                &[
                    ("opencode-go/kimi-k3", true),
                    ("opencode-go/deepseek-v4.1-flash", false),
                ],
            ),
        ];
        assert_eq!(
            resolve_side_chat_target(&probes, &[], None, None, None),
            Some((
                ProviderKind::Pi,
                Some("opencode-go/deepseek-v4.1-flash".into())
            ))
        );
    }

    #[test]
    fn an_explicit_side_chat_model_wins_while_available() {
        let probes = [
            probe(ProviderKind::Claude, &[("claude-haiku-4-5", false)]),
            probe(
                ProviderKind::Pi,
                &[("opencode-go/deepseek-v4.1-flash", true)],
            ),
        ];
        assert_eq!(
            resolve_side_chat_target(
                &probes,
                &[],
                Some(ProviderKind::Claude),
                Some("claude-haiku-4-5"),
                None
            ),
            Some((ProviderKind::Claude, Some("claude-haiku-4-5".into())))
        );
        // A disabled provider falls back to the Pi default.
        assert_eq!(
            resolve_side_chat_target(
                &probes,
                &[ProviderKind::Claude],
                Some(ProviderKind::Claude),
                Some("claude-haiku-4-5"),
                None
            ),
            Some((
                ProviderKind::Pi,
                Some("opencode-go/deepseek-v4.1-flash".into())
            ))
        );
    }

    #[test]
    fn without_pi_side_chats_follow_the_parent_task() {
        let probes = [probe(ProviderKind::Codex, &[("gpt-5.6", true)])];
        assert_eq!(
            resolve_side_chat_target(
                &probes,
                &[],
                None,
                None,
                Some((ProviderKind::Codex, Some("gpt-5.6".into())))
            ),
            Some((ProviderKind::Codex, Some("gpt-5.6".into())))
        );
    }

    #[test]
    fn the_snapshot_keeps_the_newest_messages_within_its_limit() {
        let mut parent = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        parent.push_message(MessageRole::User, "first question");
        for index in 0..400 {
            parent.push_message(
                MessageRole::Assistant,
                format!("reply {index} {}", "x".repeat(200)),
            );
        }
        parent.push_message(MessageRole::User, "latest question");
        let preamble = side_chat_preamble(&parent, Some("quoted text"));
        assert!(preamble.len() < SNAPSHOT_LIMIT + 1024);
        assert!(preamble.contains("latest question"));
        assert!(!preamble.contains("first question"));
        assert!(preamble.contains("[earlier messages omitted]"));
        assert!(preamble.contains("<excerpt>\nquoted text\n</excerpt>"));
    }

    #[test]
    fn anchors_index_by_parent_message_and_link_back() {
        let parent = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        let message_id = Uuid::new_v4();
        let mut chat = AgentSession::new(parent.project_id, ProviderKind::Pi);
        chat.side_chat = Some(SideChatOrigin {
            parent_session_id: parent.id,
            anchor: Some(SideChatAnchor {
                message_id,
                quote: "quoted".into(),
                spans: vec![SideChatAnchorSpan {
                    index: 3,
                    start: 2,
                    end: 8,
                }],
            }),
        });
        let mut standard = AgentSession::new(parent.project_id, ProviderKind::Pi);
        standard.side_chat = Some(SideChatOrigin {
            parent_session_id: parent.id,
            anchor: None,
        });
        let index = build_side_chat_index(&[parent.clone(), chat.clone(), standard.clone()]);
        let ids = &index.by_parent[&parent.id];
        assert_eq!(ids.len(), 2);
        let anchors = &index.anchors[&message_id];
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].index, 3);
        assert_eq!(anchors[0].range, 2..8);
        assert_eq!(side_chat_link_target(&anchors[0].target), Some(chat.id));
        assert!(!chat.is_listed_task());
        assert!(!chat.is_task_draft());
    }
}
