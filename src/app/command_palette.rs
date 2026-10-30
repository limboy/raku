//! Session search opened with the platform's primary shortcut + K.
//!
//! The search field keeps real focus while the list cursor is drawn, matching
//! native pickers. Session title results rebuild when the query changes;
//! persisted transcript matches join from a debounced background
//! SQLite scan. Caret blinks therefore repaint one in-memory snapshot instead
//! of re-fuzzy-matching history or touching storage every frame.

use gpui::{KeyBinding, StyledText, TextRun, actions};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};

use super::*;

actions!(
    waku_command_palette,
    [
        SelectNext,
        SelectPrevious,
        SelectFirst,
        SelectLast,
        SelectPageDown,
        SelectPageUp,
        Confirm,
        Dismiss
    ]
);

const SEARCH_CONTEXT: &str = "CommandPalette > TextInput";
const MAX_TASK_RESULTS: usize = 12;
const MAX_RESUME_RESULTS: usize = 30;
const PROVIDER_SESSION_CATALOG_LIMIT: usize = 250;
const MESSAGE_SEARCH_LIMIT: usize = 50;
const MESSAGE_SEARCH_CACHE_CAPACITY: usize = 24;
const PAGE_STEP: isize = 7;
const MESSAGE_SEARCH_DEBOUNCE: Duration = Duration::from_millis(90);
const SEARCH_ROW_HEIGHT: f32 = 60.0;
const SECTION_HEADER_HEIGHT: f32 = 30.0;
const SECTION_TOP_MARGIN: f32 = 8.0;
const RESULT_ROW_HEIGHT: f32 = 44.0;
const CONTENT_RESULT_ROW_HEIGHT: f32 = 60.0;
const EMPTY_RESULTS_HEIGHT: f32 = 180.0;
const RESULTS_BOTTOM_PADDING: f32 = 8.0;
const MAX_CARD_HEIGHT: f32 = 480.0;

/// Bind list navigation beneath the focused one-line input. This is registered
/// after the input's bindings, although the more-specific key context would
/// win either way.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, Some(SEARCH_CONTEXT)),
        KeyBinding::new("up", SelectPrevious, Some(SEARCH_CONTEXT)),
        KeyBinding::new("ctrl-n", SelectNext, Some(SEARCH_CONTEXT)),
        KeyBinding::new("ctrl-p", SelectPrevious, Some(SEARCH_CONTEXT)),
        KeyBinding::new("tab", SelectNext, Some(SEARCH_CONTEXT)),
        KeyBinding::new("shift-tab", SelectPrevious, Some(SEARCH_CONTEXT)),
        KeyBinding::new("home", SelectFirst, Some(SEARCH_CONTEXT)),
        KeyBinding::new("end", SelectLast, Some(SEARCH_CONTEXT)),
        KeyBinding::new("pagedown", SelectPageDown, Some(SEARCH_CONTEXT)),
        KeyBinding::new("pageup", SelectPageUp, Some(SEARCH_CONTEXT)),
        KeyBinding::new("enter", Confirm, Some(SEARCH_CONTEXT)),
        // Bound at the palette, not the field: the query field's own
        // clear-on-escape outranks this (deeper context) while it has text,
        // and an empty field propagates the keystroke down to it.
        KeyBinding::new("escape", Dismiss, Some("CommandPalette")),
    ]);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaletteSection {
    Tasks,
    Sessions,
    Providers,
}

impl PaletteSection {
    fn label(self) -> String {
        tr!(match self {
            Self::Tasks => "command_palette.tasks",
            Self::Sessions => "command_palette.sessions",
            Self::Providers => "command_palette.providers",
        })
    }

    fn shows_header(self) -> bool {
        self == Self::Sessions
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PaletteAction {
    ChooseResumeProvider,
    SelectResumeProvider(ProviderKind),
    ResumeProviderSession(ProviderSessionSummary),
    SelectTask(Uuid),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CommandPaletteView {
    #[default]
    Search,
    Resume,
    ResumeProviders,
}

#[derive(Clone, Debug)]
pub(super) struct CommandPaletteItem {
    section: PaletteSection,
    label: String,
    detail: Option<String>,
    provider: Option<ProviderKind>,
    action: PaletteAction,
    content_match: Option<crate::persistence::SessionMessageMatch>,
    search_text: String,
    order: usize,
    recency: u64,
}

/// Build only session results. Project/provider metadata is presentation, not
/// searchable text; conversation matches arrive from the background scan.
fn session_search_candidates(
    sessions: &[AgentSession],
    projects: &[Project],
    message_matches: &HashMap<Uuid, crate::persistence::SessionMessageMatch>,
) -> Vec<CommandPaletteItem> {
    let projects = projects
        .iter()
        .map(|project| (project.id, project.display_name()))
        .collect::<HashMap<_, _>>();
    sessions
        .iter()
        .filter(|session| session.has_started())
        .enumerate()
        .map(|(order, session)| {
            let label = session.display_title().to_owned();
            CommandPaletteItem {
                section: PaletteSection::Tasks,
                search_text: label.clone(),
                label,
                detail: Some(
                    projects
                        .get(&session.project_id)
                        .cloned()
                        .unwrap_or_else(|| tr!("project.no_project_name")),
                ),
                provider: None,
                action: PaletteAction::SelectTask(session.id),
                content_match: message_matches.get(&session.id).cloned(),
                order,
                recency: session.last_reply_at.unwrap_or(session.created_at),
            }
        })
        .collect()
}

fn session_search_results(
    candidates: Vec<CommandPaletteItem>,
    query: &str,
    message_matches_are_current: bool,
    matcher: &mut Matcher,
) -> Vec<CommandPaletteItem> {
    let query = query.trim();
    if query.is_empty() {
        let mut recent = candidates;
        recent.sort_by(|left, right| {
            right
                .recency
                .cmp(&left.recency)
                .then(left.order.cmp(&right.order))
        });
        recent.truncate(MAX_TASK_RESULTS);
        for item in &mut recent {
            item.content_match = None;
        }
        return recent;
    }

    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut utf32 = Vec::new();
    let mut scored = candidates
        .into_iter()
        .filter_map(|mut item| {
            let title_score = pattern.score(Utf32Str::new(&item.search_text, &mut utf32), matcher);
            let content_score = item.content_match.as_ref().and_then(|matched| {
                pattern.score(Utf32Str::new(&matched.snippet, &mut utf32), matcher)
            });
            let current_content_match = message_matches_are_current && item.content_match.is_some();
            if content_score.is_none() && !current_content_match {
                item.content_match = None;
            }
            title_score
                .into_iter()
                .chain(content_score)
                .max()
                .or_else(|| current_content_match.then_some(0))
                .map(|score| ScoredPaletteItem { score, item })
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then(right.item.recency.cmp(&left.item.recency))
            .then(left.item.order.cmp(&right.item.order))
    });
    scored.truncate(MAX_TASK_RESULTS);
    scored.into_iter().map(|scored| scored.item).collect()
}

struct ScoredPaletteItem {
    score: u32,
    item: CommandPaletteItem,
}

fn next_selection_index(selected: usize, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let selected = selected.min(len - 1);
    Some(if delta == isize::MIN {
        0
    } else if delta == isize::MAX {
        len - 1
    } else if delta.unsigned_abs() > 1 {
        (selected as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize
    } else {
        (selected as isize + delta).rem_euclid(len as isize) as usize
    })
}

fn palette_content_match_text(
    matched: &crate::persistence::SessionMessageMatch,
    query: &str,
    window: &Window,
    theme: Theme,
) -> StyledText {
    let (source_label, source_color) = match matched.source {
        MessageRole::User => (tr!("command_palette.you"), theme.gauge),
        MessageRole::Assistant | MessageRole::System => {
            (tr!("command_palette.agent"), theme.success)
        }
    };
    let label = format!("{source_label}: ");
    let mut text = String::with_capacity(label.len() + matched.snippet.len());
    text.push_str(&label);
    text.push_str(&matched.snippet);

    let mut normal_font = window.text_style().font();
    normal_font.weight = FontWeight::NORMAL;
    let mut emphasized_font = normal_font.clone();
    emphasized_font.weight = FontWeight::SEMIBOLD;
    let mut runs = vec![TextRun {
        len: label.len(),
        font: emphasized_font.clone(),
        color: source_color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }];

    let query = query.trim();
    let match_range = (!query.is_empty())
        .then(|| {
            matched
                .snippet
                .to_ascii_lowercase()
                .find(&query.to_ascii_lowercase())
                .map(|start| start..start + query.len())
        })
        .flatten();
    let mut push = |len: usize, font, color| {
        if len > 0 {
            runs.push(TextRun {
                len,
                font,
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            });
        }
    };
    if let Some(range) = match_range {
        push(range.start, normal_font.clone(), theme.text_tertiary);
        push(range.len(), emphasized_font, theme.text_secondary);
        push(
            matched.snippet.len().saturating_sub(range.end),
            normal_font,
            theme.text_tertiary,
        );
    } else {
        push(matched.snippet.len(), normal_font, theme.text_tertiary);
    }
    StyledText::new(text).with_runs(runs)
}

fn command_palette_row_height(item: &CommandPaletteItem) -> f32 {
    if item.content_match.is_some() {
        CONTENT_RESULT_ROW_HEIGHT
    } else {
        RESULT_ROW_HEIGHT
    }
}

fn should_show_command_palette_empty_state(result_count: usize, search_pending: bool) -> bool {
    result_count == 0 && !search_pending
}

fn should_keep_previous_command_palette_results(
    next_result_count: usize,
    search_pending: bool,
    previous_result_count: usize,
) -> bool {
    next_result_count == 0 && search_pending && previous_result_count > 0
}

fn same_provider_session(left: &ProviderResumeCursor, right: &ProviderResumeCursor) -> bool {
    left.provider() == right.provider() && left.native_id() == right.native_id()
}

fn palette_scroll_index(results: &[CommandPaletteItem], selected: usize) -> usize {
    let mut headers = 0;
    let mut previous = None;
    for item in results.iter().take(selected + 1) {
        if previous != Some(item.section) {
            headers += usize::from(item.section.shows_header());
            previous = Some(item.section);
        }
    }
    selected + headers
}

fn command_palette_results_height(results: &[CommandPaletteItem], show_empty_state: bool) -> f32 {
    if show_empty_state {
        EMPTY_RESULTS_HEIGHT
    } else {
        let mut previous_section = None;
        results
            .iter()
            .map(|item| {
                let section_leading_height = if previous_section != Some(item.section) {
                    if item.section.shows_header() {
                        SECTION_HEADER_HEIGHT
                    } else {
                        SECTION_TOP_MARGIN
                    }
                } else {
                    0.0
                };
                if previous_section != Some(item.section) {
                    previous_section = Some(item.section);
                }
                section_leading_height + command_palette_row_height(item)
            })
            .sum()
    }
}

pub(super) struct CommandPaletteUi {
    search: Entity<TextInput>,
    open: bool,
    focus_generation: u64,
    previous_focus: Option<FocusHandle>,
    view: CommandPaletteView,
    results: Vec<CommandPaletteItem>,
    message_searches: QueryCache<String, Vec<crate::persistence::SessionMessageMatch>>,
    active_message_query: Option<String>,
    message_matches_query: Option<String>,
    message_matches: HashMap<Uuid, crate::persistence::SessionMessageMatch>,
    message_search_pending: bool,
    provider_sessions: Vec<ProviderSessionSummary>,
    resume_provider: ProviderKind,
    provider_sessions_pending: bool,
    provider_session_import: Option<ProviderResumeCursor>,
    provider_session_error: Option<String>,
    provider_session_generation: u64,
    selected: usize,
    scroll: ScrollHandle,
    matcher: Matcher,
}

impl CommandPaletteUi {
    pub(super) fn new(search: Entity<TextInput>) -> Self {
        Self {
            search,
            open: false,
            focus_generation: 0,
            previous_focus: None,
            view: CommandPaletteView::Search,
            results: Vec::new(),
            message_searches: QueryCache::new(MESSAGE_SEARCH_CACHE_CAPACITY),
            active_message_query: None,
            message_matches_query: None,
            message_matches: HashMap::new(),
            message_search_pending: false,
            provider_sessions: Vec::new(),
            resume_provider: ProviderKind::default(),
            provider_sessions_pending: false,
            provider_session_import: None,
            provider_session_error: None,
            provider_session_generation: 0,
            selected: 0,
            scroll: ScrollHandle::new(),
            matcher: crate::composer_complete::matcher(),
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }
}

impl Waku {
    pub(super) fn open_resume_picker_action(
        &mut self,
        _: &OpenResumePicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.command_palette.open {
            self.open_command_palette(window, cx);
        }
        self.open_command_palette_resume_view(None, cx);
    }

    pub(super) fn toggle_command_palette_action(
        &mut self,
        _: &ToggleCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.command_palette.open {
            self.close_command_palette(window, cx);
        } else {
            self.open_command_palette(window, cx);
        }
    }

    fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open_menus = self
            .menus
            .borrow()
            .values()
            .filter(|menu| menu.is_open())
            .cloned()
            .collect::<Vec<_>>();
        // A focused picker input disappears when its menu closes. Restore to a
        // durable surface instead of remembering that soon-detached handle.
        self.command_palette.previous_focus = if open_menus.is_empty() {
            window.focused(cx)
        } else if self.settings_page.is_some() {
            Some(self.settings_focus.clone())
        } else {
            Some(self.composer_focus(cx))
        };

        self.command_palette.open = true;
        self.command_palette.view = CommandPaletteView::Search;
        self.command_palette.focus_generation =
            self.command_palette.focus_generation.wrapping_add(1);
        self.command_palette.message_searches.clear();
        self.command_palette.active_message_query = None;
        self.command_palette.message_matches_query = None;
        self.command_palette.message_matches.clear();
        self.command_palette.message_search_pending = false;
        self.command_palette.provider_sessions.clear();
        self.command_palette.provider_sessions_pending = false;
        self.command_palette.provider_session_import = None;
        self.command_palette.provider_session_error = None;
        self.command_palette.provider_session_generation = self
            .command_palette
            .provider_session_generation
            .wrapping_add(1);
        let focus_generation = self.command_palette.focus_generation;
        self.command_palette.search.update(cx, |input, cx| {
            input.set_placeholder(tr!("session_search.placeholder"), cx);
            input.clear(cx);
        });
        self.refresh_command_palette_results("", false, cx);

        // Closing an open GPUI menu can call its toggle observers back into
        // this entity, so release this action listener's mutable borrow first.
        if !open_menus.is_empty() {
            window.defer(cx, move |window, cx| {
                for menu in open_menus {
                    menu.close(window, cx);
                }
            });
        }

        // The palette is deferred onto GPUI's overlay plane. Wait for that
        // subtree to join the dispatch tree before handing focus to its input.
        let focus = self.command_palette.search.read(cx).focus();
        let weak = cx.entity().downgrade();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| {
                let mut should_focus = false;
                let _ = weak.update(cx, |this, _| {
                    should_focus = this.command_palette.open
                        && this.command_palette.focus_generation == focus_generation;
                });
                if should_focus {
                    window.focus(&focus, cx);
                }
            });
        });
        cx.notify();
    }

    fn close_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.command_palette.open {
            return;
        }
        self.command_palette.open = false;
        self.command_palette.focus_generation =
            self.command_palette.focus_generation.wrapping_add(1);
        self.command_palette.active_message_query = None;
        self.command_palette.message_search_pending = false;
        self.command_palette.provider_sessions_pending = false;
        self.command_palette.provider_session_import = None;
        self.command_palette.provider_session_generation = self
            .command_palette
            .provider_session_generation
            .wrapping_add(1);
        if let Some(previous_focus) = self.command_palette.previous_focus.take() {
            window.focus(&previous_focus, cx);
        }
        cx.notify();
    }

    fn leave_command_palette_resume_provider_view(&mut self, cx: &mut Context<Self>) {
        self.command_palette.view = CommandPaletteView::Resume;
        self.command_palette.search.update(cx, |input, cx| {
            input.set_placeholder(tr!("command_palette.resume_placeholder"), cx);
            input.clear(cx);
        });
        self.refresh_command_palette_results("", false, cx);
        cx.notify();
    }

    fn dismiss_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.command_palette.view {
            CommandPaletteView::Search | CommandPaletteView::Resume => {
                self.close_command_palette(window, cx)
            }
            CommandPaletteView::ResumeProviders => {
                self.leave_command_palette_resume_provider_view(cx)
            }
        }
    }

    pub(super) fn refresh_command_palette_localized_text(&mut self, cx: &mut Context<Self>) {
        let placeholder = match self.command_palette.view {
            CommandPaletteView::Search => tr!("session_search.placeholder"),
            CommandPaletteView::Resume => tr!("command_palette.resume_placeholder"),
            CommandPaletteView::ResumeProviders => {
                tr!("command_palette.resume_provider_placeholder")
            }
        };
        self.command_palette
            .search
            .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        if self.command_palette.open {
            let query = self.command_palette.search.read(cx).content().to_owned();
            self.refresh_command_palette_results(&query, true, cx);
        }
    }

    pub(super) fn command_palette_query_edited(&mut self, query: &str, cx: &mut Context<Self>) {
        if !self.command_palette.open {
            return;
        }
        if matches!(
            self.command_palette.view,
            CommandPaletteView::Resume | CommandPaletteView::ResumeProviders
        ) {
            self.refresh_command_palette_results(query, false, cx);
            cx.notify();
            return;
        }
        let query = query.trim().to_owned();
        self.command_palette.active_message_query = (!query.is_empty()).then(|| query.clone());
        let fetch = if query.is_empty() {
            self.command_palette.message_matches_query = None;
            self.command_palette.message_matches.clear();
            self.command_palette.message_search_pending = false;
            None
        } else {
            match self.command_palette.message_searches.read(&query) {
                Query::Ready(matches) => {
                    self.command_palette.message_matches_query = Some(query.clone());
                    self.command_palette.message_matches = matches
                        .iter()
                        .cloned()
                        .map(|matched| (matched.session_id, matched))
                        .collect();
                    self.command_palette.message_search_pending = false;
                    None
                }
                Query::Pending => {
                    self.command_palette.message_search_pending = true;
                    None
                }
                Query::Missing(token) => {
                    self.command_palette.message_search_pending = true;
                    Some(token)
                }
            }
        };
        self.refresh_command_palette_results(&query, false, cx);
        cx.notify();

        let Some(token) = fetch else {
            return;
        };
        let search = self
            .store
            .session_message_search(query.clone(), MESSAGE_SEARCH_LIMIT);
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(MESSAGE_SEARCH_DEBOUNCE)
                .await;
            let current = this
                .update(cx, |this, _| {
                    this.command_palette.open
                        && this.command_palette.view == CommandPaletteView::Search
                        && this.command_palette.active_message_query.as_deref()
                            == Some(query.as_str())
                })
                .unwrap_or(false);
            if !current {
                let _ = this.update(cx, |this, _| {
                    this.command_palette.message_searches.abandon(token)
                });
                return;
            }

            let matches = cx
                .background_executor()
                .spawn(async move { search().unwrap_or_default() })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this
                    .command_palette
                    .message_searches
                    .fulfill(token, matches)
                {
                    return;
                }
                if !this.command_palette.open
                    || this.command_palette.view != CommandPaletteView::Search
                    || this.command_palette.active_message_query.as_deref() != Some(query.as_str())
                {
                    return;
                }
                let Query::Ready(matches) = this.command_palette.message_searches.read(&query)
                else {
                    return;
                };
                this.command_palette.message_matches_query = Some(query.clone());
                this.command_palette.message_matches = matches
                    .iter()
                    .cloned()
                    .map(|matched| (matched.session_id, matched))
                    .collect();
                this.command_palette.message_search_pending = false;
                this.refresh_command_palette_results(&query, false, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn command_palette_resume_candidates(&self) -> Vec<CommandPaletteItem> {
        let now = unix_time();
        self.command_palette
            .provider_sessions
            .iter()
            .filter(|native| {
                !self.state.sessions.iter().any(|session| {
                    session.provider_cursor.as_ref().is_some_and(|cursor| {
                        cursor.provider() == native.provider()
                            && cursor.native_id() == native.cursor.native_id()
                    })
                })
            })
            .enumerate()
            .map(|(order, native)| {
                let provider = native.provider();
                let age = super::sidebar::format_time_ago(now.saturating_sub(native.updated_at));
                let path = native.cwd.to_string_lossy();
                CommandPaletteItem {
                    section: PaletteSection::Sessions,
                    label: native.title.clone(),
                    detail: Some(format!("{} · {} · {age}", provider.short_name(), path)),
                    provider: Some(provider),
                    action: PaletteAction::ResumeProviderSession(native.clone()),
                    content_match: None,
                    search_text: format!(
                        "{} {} {} {} {} resume continue terminal cli session conversation",
                        native.title,
                        path,
                        provider.short_name(),
                        provider.display_name(),
                        native.cursor.native_id(),
                    ),
                    order,
                    recency: native.updated_at,
                }
            })
            .collect()
    }

    fn command_palette_resume_provider_selector(&self) -> CommandPaletteItem {
        let provider = self.command_palette.resume_provider;
        CommandPaletteItem {
            section: PaletteSection::Providers,
            label: provider.display_name().to_owned(),
            detail: Some(tr!("command_palette.change_provider")),
            provider: Some(provider),
            action: PaletteAction::ChooseResumeProvider,
            content_match: None,
            search_text: format!(
                "{} {} change select provider agent cli",
                provider.short_name(),
                provider.display_name()
            ),
            order: 0,
            recency: u64::MAX,
        }
    }

    fn command_palette_resume_provider_candidates(&self) -> Vec<CommandPaletteItem> {
        ProviderKind::ALL
            .into_iter()
            .filter(|provider| {
                *provider == self.command_palette.resume_provider
                    || !self.state.disabled_providers.contains(provider)
            })
            .enumerate()
            .map(|(order, provider)| CommandPaletteItem {
                section: PaletteSection::Providers,
                label: provider.display_name().to_owned(),
                detail: (provider == self.command_palette.resume_provider)
                    .then(|| tr!("command_palette.current_provider")),
                provider: Some(provider),
                action: PaletteAction::SelectResumeProvider(provider),
                content_match: None,
                search_text: format!(
                    "{} {} provider agent cli terminal session",
                    provider.short_name(),
                    provider.display_name()
                ),
                order,
                recency: 0,
            })
            .collect()
    }

    fn refresh_command_palette_resume_results(&mut self, query: &str, preserve_selection: bool) {
        let query = query.trim();
        let selected_action = preserve_selection.then(|| {
            self.command_palette
                .results
                .get(self.command_palette.selected)
                .map(|item| item.action.clone())
        });
        let mut candidates = self.command_palette_resume_candidates();
        if query.is_empty() {
            candidates.sort_by(|a, b| b.recency.cmp(&a.recency).then(a.order.cmp(&b.order)));
            candidates.truncate(MAX_RESUME_RESULTS);
            self.command_palette.results = candidates;
        } else {
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
            let mut utf32 = Vec::new();
            let mut scored = candidates
                .into_iter()
                .filter_map(|item| {
                    pattern
                        .score(
                            Utf32Str::new(&item.search_text, &mut utf32),
                            &mut self.command_palette.matcher,
                        )
                        .map(|score| ScoredPaletteItem { score, item })
                })
                .collect::<Vec<_>>();
            scored.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then(b.item.recency.cmp(&a.item.recency))
                    .then(a.item.order.cmp(&b.item.order))
            });
            scored.truncate(MAX_RESUME_RESULTS);
            self.command_palette.results = scored.into_iter().map(|scored| scored.item).collect();
        }
        let provider_selector = self.command_palette_resume_provider_selector();
        self.command_palette.results.insert(0, provider_selector);
        self.command_palette.selected = selected_action
            .flatten()
            .and_then(|action| {
                self.command_palette
                    .results
                    .iter()
                    .position(|item| item.action == action)
            })
            .unwrap_or_else(|| usize::from(self.command_palette.results.len() > 1));
        let scroll_index = self.command_palette_scroll_index(self.command_palette.selected);
        self.command_palette.scroll.scroll_to_item(scroll_index);
    }

    fn refresh_command_palette_resume_provider_results(
        &mut self,
        query: &str,
        preserve_selection: bool,
    ) {
        let selected_action = preserve_selection.then(|| {
            self.command_palette
                .results
                .get(self.command_palette.selected)
                .map(|item| item.action.clone())
        });
        let query = query.trim();
        let mut candidates = self.command_palette_resume_provider_candidates();
        if !query.is_empty() {
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
            let mut utf32 = Vec::new();
            let mut scored = candidates
                .into_iter()
                .filter_map(|item| {
                    pattern
                        .score(
                            Utf32Str::new(&item.search_text, &mut utf32),
                            &mut self.command_palette.matcher,
                        )
                        .map(|score| ScoredPaletteItem { score, item })
                })
                .collect::<Vec<_>>();
            scored.sort_by(|left, right| {
                right
                    .score
                    .cmp(&left.score)
                    .then(left.item.order.cmp(&right.item.order))
            });
            candidates = scored.into_iter().map(|scored| scored.item).collect();
        }
        self.command_palette.results = candidates;
        self.command_palette.selected = selected_action
            .flatten()
            .and_then(|action| {
                self.command_palette
                    .results
                    .iter()
                    .position(|item| item.action == action)
            })
            .or_else(|| {
                self.command_palette.results.iter().position(|item| {
                    item.action
                        == PaletteAction::SelectResumeProvider(self.command_palette.resume_provider)
                })
            })
            .unwrap_or(0);
        self.command_palette
            .scroll
            .scroll_to_item(self.command_palette_scroll_index(self.command_palette.selected));
    }

    fn refresh_command_palette_results(
        &mut self,
        query: &str,
        preserve_selection: bool,
        _cx: &App,
    ) {
        match self.command_palette.view {
            CommandPaletteView::Resume => {
                self.refresh_command_palette_resume_results(query, preserve_selection);
                return;
            }
            CommandPaletteView::ResumeProviders => {
                self.refresh_command_palette_resume_provider_results(query, preserve_selection);
                return;
            }
            CommandPaletteView::Search => {}
        }
        let query = query.trim();
        let selected_action = preserve_selection.then(|| {
            self.command_palette
                .results
                .get(self.command_palette.selected)
                .map(|item| item.action.clone())
        });
        let candidates = session_search_candidates(
            &self.state.sessions,
            &self.state.projects,
            &self.command_palette.message_matches,
        );
        let next_results = session_search_results(
            candidates,
            query,
            self.command_palette.message_matches_query.as_deref() == Some(query),
            &mut self.command_palette.matcher,
        );
        // This is the palette equivalent of TanStack Query's
        // `keepPreviousData`: never replace useful rows with a transient blank
        // frame while the transcript query is still in flight.
        if should_keep_previous_command_palette_results(
            next_results.len(),
            self.command_palette.message_search_pending,
            self.command_palette.results.len(),
        ) {
            self.command_palette.selected = 0;
            self.command_palette.scroll.scroll_to_item(0);
            return;
        }
        self.command_palette.results = next_results;
        self.command_palette.selected = selected_action
            .flatten()
            .and_then(|action| {
                self.command_palette
                    .results
                    .iter()
                    .position(|item| item.action == action)
            })
            .unwrap_or(0);
        let scroll_index = self.command_palette_scroll_index(self.command_palette.selected);
        self.command_palette.scroll.scroll_to_item(scroll_index);
    }

    fn command_palette_scroll_index(&self, selected: usize) -> usize {
        palette_scroll_index(&self.command_palette.results, selected)
    }

    fn move_command_palette_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.command_palette.results.len();
        let Some(next) = next_selection_index(self.command_palette.selected, len, delta) else {
            return;
        };
        self.command_palette.selected = next;
        self.command_palette
            .scroll
            .scroll_to_item(self.command_palette_scroll_index(next));
        cx.notify();
    }

    fn set_command_palette_selection(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.command_palette.results.len() && self.command_palette.selected != index {
            self.command_palette.selected = index;
            cx.notify();
        }
    }

    fn default_resume_provider(&self) -> ProviderKind {
        self.selected_session()
            .map(|session| session.provider)
            .or_else(|| {
                ProviderKind::ALL
                    .into_iter()
                    .find(|provider| !self.state.disabled_providers.contains(provider))
            })
            .unwrap_or_default()
    }

    fn open_command_palette_resume_provider_view(&mut self, cx: &mut Context<Self>) {
        self.command_palette.view = CommandPaletteView::ResumeProviders;
        self.command_palette.search.update(cx, |input, cx| {
            input.set_placeholder(tr!("command_palette.resume_provider_placeholder"), cx);
            input.clear(cx);
        });
        self.refresh_command_palette_results("", false, cx);
        cx.notify();
    }

    fn open_command_palette_resume_view(
        &mut self,
        provider: Option<ProviderKind>,
        cx: &mut Context<Self>,
    ) {
        let provider = provider.unwrap_or_else(|| self.default_resume_provider());
        self.command_palette.view = CommandPaletteView::Resume;
        self.command_palette.active_message_query = None;
        self.command_palette.message_search_pending = false;
        self.command_palette.resume_provider = provider;
        self.command_palette.provider_sessions.clear();
        self.command_palette.provider_sessions_pending = true;
        self.command_palette.provider_session_import = None;
        self.command_palette.provider_session_error = None;
        self.command_palette.provider_session_generation = self
            .command_palette
            .provider_session_generation
            .wrapping_add(1);
        let generation = self.command_palette.provider_session_generation;
        self.command_palette.search.update(cx, |input, cx| {
            input.set_placeholder(tr!("command_palette.resume_placeholder"), cx);
            input.clear(cx);
        });
        self.refresh_command_palette_results("", false, cx);
        cx.notify();

        let fetch = self
            .store
            .provider_sessions(provider, PROVIDER_SESSION_CATALOG_LIMIT);
        cx.spawn(async move |waku, cx| {
            let result = cx.background_executor().spawn(async move { fetch() }).await;
            let _ = waku.update(cx, |waku, cx| {
                if !waku.command_palette.open
                    || waku.command_palette.provider_session_generation != generation
                    || waku.command_palette.resume_provider != provider
                {
                    return;
                }
                waku.command_palette.provider_sessions_pending = false;
                match result {
                    Ok(sessions) => {
                        waku.command_palette.provider_sessions = sessions;
                        waku.command_palette.provider_session_error = None;
                    }
                    Err(error) => {
                        waku.command_palette.provider_sessions.clear();
                        waku.command_palette.provider_session_error = Some(error.to_string());
                    }
                }
                if waku.command_palette.view == CommandPaletteView::Resume {
                    let query = waku.command_palette.search.read(cx).content().to_owned();
                    waku.refresh_command_palette_results(&query, false, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn import_provider_session(
        &mut self,
        summary: ProviderSessionSummary,
        history: ProviderSessionHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session_id) = self
            .state
            .sessions
            .iter()
            .find(|session| {
                session
                    .provider_cursor
                    .as_ref()
                    .is_some_and(|cursor| same_provider_session(cursor, &summary.cursor))
            })
            .map(|session| session.id)
        {
            self.close_command_palette(window, cx);
            self.settings_page = None;
            self.select_session(session_id, cx);
            let focus = self.composer_focus(cx);
            window.focus(&focus, cx);
            return;
        }

        let project_id = if let Some(project) = self
            .state
            .projects
            .iter()
            .find(|project| project.path == summary.cwd)
        {
            project.id
        } else {
            let project = Project::from_path(summary.cwd.clone());
            let project_id = project.id;
            self.state.projects.push(project);
            self.analytics.track(crate::analytics::Event::ProjectAdded);
            project_id
        };
        let provider = summary.provider();
        let runtime_mode = self
            .selected_session()
            .map(|session| session.runtime_mode)
            .unwrap_or(self.state.last_runtime_mode);
        let now = unix_time();
        let created_at = if summary.created_at == 0 {
            now
        } else {
            summary.created_at
        };
        let updated_at = summary.updated_at.max(created_at);
        let has_history = !history.messages.is_empty() || !history.turns.is_empty();
        let mut session = AgentSession::new(project_id, provider);
        session.runtime_mode = runtime_mode;
        session.auto_title = Some(summary.title);
        session.provider_cursor = Some(summary.cursor);
        session.created_at = created_at;
        session.updated_at = updated_at;
        session.last_reply_at = has_history.then_some(updated_at);
        session.messages = history.messages;
        session.turns = history.turns;
        let session_id = session.id;
        self.state.push_session(session);

        self.close_command_palette(window, cx);
        self.settings_page = None;
        self.select_session(session_id, cx);
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
    }

    fn load_command_palette_provider_session(
        &mut self,
        summary: ProviderSessionSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.command_palette.provider_session_import.is_some() {
            return;
        }
        if let Some(session_id) = self
            .state
            .sessions
            .iter()
            .find(|session| {
                session
                    .provider_cursor
                    .as_ref()
                    .is_some_and(|cursor| same_provider_session(cursor, &summary.cursor))
            })
            .map(|session| session.id)
        {
            self.close_command_palette(window, cx);
            self.settings_page = None;
            self.select_session(session_id, cx);
            let focus = self.composer_focus(cx);
            window.focus(&focus, cx);
            return;
        }

        self.command_palette.provider_session_import = Some(summary.cursor.clone());
        self.command_palette.provider_session_error = None;
        self.command_palette.provider_session_generation = self
            .command_palette
            .provider_session_generation
            .wrapping_add(1);
        let generation = self.command_palette.provider_session_generation;
        let cursor = summary.cursor.clone();
        let fetch = self
            .store
            .provider_session_history(cursor.clone(), summary.cwd.clone());
        let window_handle = window.window_handle();
        cx.notify();

        cx.spawn(async move |waku, cx| {
            let result = cx.background_executor().spawn(async move { fetch() }).await;
            let update = waku.update(cx, |waku, cx| {
                if !waku.command_palette.open
                    || waku.command_palette.view != CommandPaletteView::Resume
                    || waku.command_palette.provider_session_generation != generation
                    || waku
                        .command_palette
                        .provider_session_import
                        .as_ref()
                        .is_none_or(|pending| !same_provider_session(pending, &cursor))
                {
                    return None;
                }
                match result {
                    Ok(history) => Some((summary, history)),
                    Err(error) => {
                        let error = error.to_string();
                        waku.command_palette.provider_session_import = None;
                        waku.command_palette.provider_session_error = Some(error.clone());
                        waku.show_toast(tr!("command_palette.resume_failed", error = error));
                        cx.notify();
                        None
                    }
                }
            });
            let Ok(Some((summary, history))) = update else {
                return;
            };
            let _ = window_handle.update(cx, |_, window, cx| {
                let _ = waku.update(cx, |waku, cx| {
                    if waku.command_palette.open
                        && waku.command_palette.view == CommandPaletteView::Resume
                        && waku.command_palette.provider_session_generation == generation
                    {
                        waku.import_provider_session(summary, history, window, cx);
                    }
                });
            });
        })
        .detach();
    }

    fn execute_command_palette_selection(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = index.unwrap_or(self.command_palette.selected);
        let Some(action) = self
            .command_palette
            .results
            .get(index)
            .map(|item| item.action.clone())
        else {
            return;
        };

        match action {
            PaletteAction::ChooseResumeProvider => {
                self.open_command_palette_resume_provider_view(cx)
            }
            PaletteAction::SelectResumeProvider(provider) => {
                self.open_command_palette_resume_view(Some(provider), cx);
            }
            PaletteAction::ResumeProviderSession(summary) => {
                self.load_command_palette_provider_session(summary, window, cx);
            }
            PaletteAction::SelectTask(session_id) => {
                self.close_command_palette(window, cx);
                self.settings_page = None;
                self.select_session(session_id, cx);
                let focus = self.composer_focus(cx);
                window.focus(&focus, cx);
            }
        }
    }

    pub(super) fn render_command_palette(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.command_palette.open {
            return None;
        }
        let theme = Theme::current(cx);
        let viewport_height = f32::from(window.viewport_size().height);
        let top = (viewport_height * 0.09).clamp(48.0, 72.0);
        let card_max_height = (viewport_height - top - 36.0)
            .max(SEARCH_ROW_HEIGHT)
            .min(MAX_CARD_HEIGHT);
        let selected = self
            .command_palette
            .selected
            .min(self.command_palette.results.len().saturating_sub(1));
        let search_query = self.command_palette.search.read(cx).content().to_owned();
        let search_view = self.command_palette.view == CommandPaletteView::Search;
        let resume_view = self.command_palette.view == CommandPaletteView::Resume;
        let resume_session_count = self
            .command_palette
            .results
            .iter()
            .filter(|item| matches!(&item.action, PaletteAction::ResumeProviderSession(_)))
            .count();
        let results_pending = match self.command_palette.view {
            CommandPaletteView::Resume => self.command_palette.provider_sessions_pending,
            CommandPaletteView::Search => self.command_palette.message_search_pending,
            CommandPaletteView::ResumeProviders => false,
        };
        let show_empty_state = should_show_command_palette_empty_state(
            if resume_view {
                resume_session_count
            } else {
                self.command_palette.results.len()
            },
            results_pending,
        );
        let show_loading_state = results_pending
            && if resume_view {
                resume_session_count == 0
            } else {
                self.command_palette.results.is_empty()
            };
        let show_placeholder_state = show_empty_state || show_loading_state;
        let results_height =
            command_palette_results_height(&self.command_palette.results, show_placeholder_state)
                .min((card_max_height - SEARCH_ROW_HEIGHT - RESULTS_BOTTOM_PADDING).max(0.0));
        let card_height = SEARCH_ROW_HEIGHT + results_height + RESULTS_BOTTOM_PADDING;

        let mut results = div()
            .id("command-palette-results")
            .h(px(results_height))
            .flex_none()
            .overflow_y_scroll()
            .track_scroll(&self.command_palette.scroll)
            .px(px(8.0));

        if show_placeholder_state {
            let error = resume_view
                .then(|| self.command_palette.provider_session_error.clone())
                .flatten();
            let (icon_path, title, hint, spinning) = if show_loading_state {
                (
                    "icons/loader-circle.svg",
                    if resume_view {
                        tr!("command_palette.loading_sessions")
                    } else {
                        tr!("session_search.searching")
                    },
                    None,
                    true,
                )
            } else if let Some(error) = error {
                (
                    "icons/alert.svg",
                    tr!("command_palette.could_not_load_sessions"),
                    Some(error),
                    false,
                )
            } else if resume_view {
                (
                    "icons/search.svg",
                    tr!("command_palette.no_resume_sessions"),
                    Some(tr!("command_palette.no_resume_sessions_hint")),
                    false,
                )
            } else if search_view {
                let empty_query = search_query.trim().is_empty();
                (
                    "icons/search.svg",
                    tr!(if empty_query {
                        "session_search.no_sessions"
                    } else {
                        "session_search.no_results"
                    }),
                    Some(tr!(if empty_query {
                        "session_search.no_sessions_hint"
                    } else {
                        "session_search.no_results_hint"
                    })),
                    false,
                )
            } else {
                (
                    "icons/search.svg",
                    tr!("command_palette.no_matching_providers"),
                    None,
                    false,
                )
            };
            let empty_icon = icon(icon_path, 18.0, theme.text_ghost);
            results = results.child(
                div()
                    .h(px(EMPTY_RESULTS_HEIGHT))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .child(if spinning {
                        motion::spin(empty_icon)
                    } else {
                        empty_icon.into_any_element()
                    })
                    .child(
                        div()
                            .mt(px(12.0))
                            .text_size(sp(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text_secondary)
                            .child(title),
                    )
                    .when_some(hint, |empty, hint| {
                        empty.child(
                            div()
                                .max_w(px(540.0))
                                .mt(px(5.0))
                                .text_size(sp(12.5))
                                .text_color(theme.text_tertiary)
                                .child(hint),
                        )
                    })
                    .when(resume_view && show_loading_state, |empty| {
                        let provider = self.command_palette.resume_provider;
                        empty.child(
                            div()
                                .mt(px(12.0))
                                .flex()
                                .items_center()
                                .gap(px(7.0))
                                .child(provider_mark(
                                    provider,
                                    13.0,
                                    provider_color(&theme, provider),
                                ))
                                .child(
                                    div()
                                        .text_size(sp(12.5))
                                        .text_color(theme.text_secondary)
                                        .child(provider.display_name().to_owned()),
                                ),
                        )
                    })
                    .when(resume_view && !show_loading_state, |empty| {
                        let provider = self.command_palette.resume_provider;
                        empty.child(
                            div()
                                .id("command-palette-resume-provider")
                                .mt(px(12.0))
                                .h(px(28.0))
                                .px(px(9.0))
                                .rounded(px(8.0))
                                .border_1()
                                .border_color(theme.border)
                                .flex()
                                .items_center()
                                .gap(px(7.0))
                                .cursor_default()
                                .hover(|button| button.bg(theme.overlay))
                                .active(|button| button.opacity(0.82))
                                .child(provider_mark(
                                    provider,
                                    13.0,
                                    provider_color(&theme, provider),
                                ))
                                .child(
                                    div()
                                        .text_size(sp(12.5))
                                        .text_color(theme.text_secondary)
                                        .child(provider.display_name().to_owned()),
                                )
                                .child(icon("icons/chevron-down.svg", 11.0, theme.text_tertiary))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.open_command_palette_resume_provider_view(cx);
                                    cx.stop_propagation();
                                })),
                        )
                    }),
            );
        } else {
            let mut previous_section = None;
            for (index, item) in self.command_palette.results.iter().enumerate() {
                let starts_section = previous_section != Some(item.section);
                if starts_section {
                    if item.section.shows_header() {
                        results = results.child(
                            div()
                                .h(px(SECTION_HEADER_HEIGHT))
                                .px(px(9.0))
                                .pt(px(10.0))
                                .flex()
                                .items_center()
                                .text_size(sp(12.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text_tertiary)
                                .child(item.section.label()),
                        );
                    }
                    previous_section = Some(item.section);
                }

                let highlighted = index == selected;
                let importing = match &item.action {
                    PaletteAction::ResumeProviderSession(summary) => self
                        .command_palette
                        .provider_session_import
                        .as_ref()
                        .is_some_and(|cursor| same_provider_session(cursor, &summary.cursor)),
                    _ => false,
                };
                let detail = if importing {
                    Some(tr!("command_palette.loading_selected_session"))
                } else {
                    item.detail.clone()
                };
                let content_match = item.content_match.clone();
                results = results.child(
                    div()
                        .id(SharedString::from(format!("command-palette-row-{index}")))
                        .when(starts_section && !item.section.shows_header(), |row| {
                            row.mt(px(SECTION_TOP_MARGIN))
                        })
                        .h(px(command_palette_row_height(item)))
                        .px(px(11.0))
                        .rounded(px(9.0))
                        .border_1()
                        .border_color(if highlighted && !search_view {
                            theme.border_strong
                        } else {
                            gpui::transparent_black()
                        })
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .cursor_default()
                        .when(highlighted, |row| row.bg(theme.overlay_strong))
                        .hover(|row| row.bg(theme.overlay))
                        .active(|row| row.opacity(0.82))
                        .on_hover(cx.listener(move |this, hovering: &bool, _, cx| {
                            if *hovering {
                                this.set_command_palette_selection(index, cx);
                            }
                        }))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.execute_command_palette_selection(Some(index), window, cx);
                            cx.stop_propagation();
                        }))
                        .when_some(item.provider, |row, provider| {
                            let color = provider_color(&theme, provider);
                            row.child(
                                div()
                                    .size(px(20.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(if importing {
                                        motion::spin(icon("icons/loader-circle.svg", 16.0, color))
                                    } else {
                                        provider_mark(provider, 16.0, color).into_any_element()
                                    }),
                            )
                        })
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .justify_center()
                                .gap(px(2.0))
                                .child(
                                    div()
                                        .min_w_0()
                                        .w_full()
                                        .flex()
                                        .items_baseline()
                                        .gap(px(7.0))
                                        .child(
                                            div()
                                                .min_w_0()
                                                .when(search_view, |title| title.flex_1())
                                                .truncate()
                                                .text_size(sp(14.0))
                                                .font_weight(if highlighted {
                                                    FontWeight::MEDIUM
                                                } else {
                                                    FontWeight::NORMAL
                                                })
                                                .text_color(if highlighted {
                                                    theme.text
                                                } else {
                                                    theme.text_secondary
                                                })
                                                .child(item.label.clone()),
                                        )
                                        .when_some(detail, |row, detail| {
                                            row.child(
                                                div()
                                                    .min_w_0()
                                                    .when(search_view, |detail| {
                                                        detail.flex_none().max_w(px(160.0))
                                                    })
                                                    .truncate()
                                                    .text_size(sp(12.5))
                                                    .text_color(theme.text_tertiary)
                                                    .child(detail),
                                            )
                                        }),
                                )
                                .when_some(content_match, |column, matched| {
                                    column.child(
                                        div()
                                            .min_w_0()
                                            .w_full()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_size(sp(12.5))
                                            .child(palette_content_match_text(
                                                &matched,
                                                &search_query,
                                                window,
                                                theme,
                                            )),
                                    )
                                }),
                        ),
                );
            }
        }

        let card =
            div()
                .id("command-palette-card")
                .key_context("CommandPalette")
                .on_action(cx.listener(Self::toggle_command_palette_action))
                .on_action(cx.listener(|this, _: &SelectNext, _, cx| {
                    this.move_command_palette_selection(1, cx)
                }))
                .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| {
                    this.move_command_palette_selection(-1, cx)
                }))
                .on_action(cx.listener(|this, _: &SelectFirst, _, cx| {
                    this.move_command_palette_selection(isize::MIN, cx)
                }))
                .on_action(cx.listener(|this, _: &SelectLast, _, cx| {
                    this.move_command_palette_selection(isize::MAX, cx)
                }))
                .on_action(cx.listener(|this, _: &SelectPageDown, _, cx| {
                    this.move_command_palette_selection(PAGE_STEP, cx)
                }))
                .on_action(cx.listener(|this, _: &SelectPageUp, _, cx| {
                    this.move_command_palette_selection(-PAGE_STEP, cx)
                }))
                .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                    this.execute_command_palette_selection(None, window, cx)
                }))
                .on_action(cx.listener(|this, _: &Dismiss, window, cx| {
                    this.dismiss_command_palette(window, cx)
                }))
                .w_full()
                .max_w(px(680.0))
                .h(px(card_height))
                .overflow_hidden()
                .rounded(px(15.0))
                .bg(theme.surface)
                .shadow_xl()
                .relative()
                .flex()
                .flex_col()
                // Keep the gutter outside the scroll viewport, including
                // when the results fill the popup's maximum height.
                .pb(px(RESULTS_BOTTOM_PADDING))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .h(px(SEARCH_ROW_HEIGHT))
                        .px(px(19.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .border_b_1()
                        .border_color(theme.border)
                        .text_size(sp(15.5))
                        .text_color(theme.text)
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .child(self.command_palette.search.clone()),
                        ),
                )
                .child(results);

        let scrim = if theme.is_dark {
            gpui::hsla(0.0, 0.0, 0.0, 0.26)
        } else {
            gpui::hsla(0.0, 0.0, 0.0, 0.14)
        };
        let layer = div()
            .id("command-palette-layer")
            .absolute()
            .inset_0()
            .occlude()
            .bg(scrim)
            .px(px(24.0))
            .pt(px(top))
            .flex()
            .items_start()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_command_palette(window, cx)),
            )
            .child(card);
        Some(gpui::deferred(layer).with_priority(3).into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::SessionMessageMatch;

    fn started_session(project_id: Uuid, title: &str, timestamp: u64) -> AgentSession {
        let mut session = AgentSession::new(project_id, ProviderKind::Codex);
        session.title = title.into();
        session.push_message(MessageRole::User, "A conversation");
        session.created_at = timestamp;
        session.last_reply_at = None;
        session
    }

    fn search(
        sessions: &[AgentSession],
        projects: &[Project],
        matches: &HashMap<Uuid, SessionMessageMatch>,
        query: &str,
    ) -> Vec<CommandPaletteItem> {
        session_search_results(
            session_search_candidates(sessions, projects, matches),
            query,
            true,
            &mut crate::composer_complete::matcher(),
        )
    }

    fn item(section: PaletteSection, order: usize) -> CommandPaletteItem {
        CommandPaletteItem {
            section,
            label: format!("Item {order}"),
            detail: None,
            provider: None,
            action: PaletteAction::SelectTask(Uuid::from_u128(order as u128)),
            content_match: None,
            search_text: String::new(),
            order,
            recency: 0,
        }
    }

    #[test]
    fn session_search_finds_message_content_when_the_title_does_not_match() {
        let session = started_session(Uuid::nil(), "Native sidebar", 1);
        let matches = HashMap::from([(
            session.id,
            SessionMessageMatch {
                session_id: session.id,
                source: MessageRole::Assistant,
                snippet: "SQLite supports 中文检索.".into(),
            },
        )]);
        for query in ["SQLite", "中文检索"] {
            let results = search(std::slice::from_ref(&session), &[], &matches, query);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].action, PaletteAction::SelectTask(session.id));
            assert_eq!(
                results[0].content_match.as_ref().unwrap().snippet,
                "SQLite supports 中文检索."
            );
        }
    }

    #[test]
    fn session_search_matches_titles_without_searching_project_or_provider_metadata() {
        let project = Project::from_path(PathBuf::from("/work/settings"));
        let mut session = started_session(project.id, "Fix native sidebar", 1);
        session.model = Some("unique-model-name".into());
        session.workspace = SessionWorkspace::NewWorktree {
            base_branch: Some("unique-branch-name".into()),
        };
        let projects = [project];
        let sessions = [session];
        let matches = HashMap::new();
        let results = search(&sessions, &projects, &matches, "native sidebar");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].detail.as_deref(), Some("settings"));
        for query in [
            "settings",
            "codex",
            "unique-model-name",
            "unique-branch-name",
            "open project",
            "new task",
        ] {
            assert!(
                search(&sessions, &projects, &matches, query).is_empty(),
                "metadata or command matched {query}"
            );
        }
    }

    #[test]
    fn empty_search_lists_recent_started_sessions_without_stale_excerpts() {
        let project = Uuid::nil();
        let mut older = started_session(project, "First conversation", 10);
        older.last_reply_at = Some(30);
        let mut newer = started_session(project, "Second conversation", 20);
        newer.updated_at = 1_000;
        let draft = AgentSession::new(project, ProviderKind::Codex);
        let matches = HashMap::from([(
            older.id,
            SessionMessageMatch {
                session_id: older.id,
                source: MessageRole::User,
                snippet: "An old query".into(),
            },
        )]);
        let expected = [older.id, newer.id];
        let results = search(&[newer, older, draft], &[], &matches, "  ");
        assert_eq!(
            results
                .iter()
                .map(|item| match item.action {
                    PaletteAction::SelectTask(id) => id,
                    _ => panic!("session search returned a command"),
                })
                .collect::<Vec<_>>(),
            expected
        );
        assert!(results.iter().all(|item| item.content_match.is_none()));
    }

    #[test]
    fn obsolete_content_matches_do_not_keep_unrelated_sessions_in_results() {
        let session = started_session(Uuid::nil(), "Native sidebar", 1);
        let matches = HashMap::from([(
            session.id,
            SessionMessageMatch {
                session_id: session.id,
                source: MessageRole::User,
                snippet: "An obsolete excerpt".into(),
            },
        )]);
        let results = session_search_results(
            session_search_candidates(&[session], &[], &matches),
            "SQLite",
            false,
            &mut crate::composer_complete::matcher(),
        );
        assert!(results.is_empty());
    }

    #[test]
    fn provider_session_identity_uses_provider_and_native_id() {
        let listed = ProviderResumeCursor::Claude {
            session_id: "11111111-1111-4111-8111-111111111111".into(),
            resume_at: None,
        };
        let imported = ProviderResumeCursor::Claude {
            session_id: "11111111-1111-4111-8111-111111111111".into(),
            resume_at: Some("native-message".into()),
        };
        let other = ProviderResumeCursor::Codex {
            thread_id: "11111111-1111-4111-8111-111111111111".into(),
        };
        assert!(same_provider_session(&listed, &imported));
        assert!(!same_provider_session(&listed, &other));
    }

    #[test]
    fn flat_search_scroll_indexes_have_no_section_headers() {
        let tasks = [
            item(PaletteSection::Tasks, 0),
            item(PaletteSection::Tasks, 1),
        ];
        assert_eq!(palette_scroll_index(&tasks, 0), 0);
        assert_eq!(palette_scroll_index(&tasks, 1), 1);
        let resume = [
            item(PaletteSection::Providers, 0),
            item(PaletteSection::Sessions, 1),
        ];
        assert_eq!(palette_scroll_index(&resume, 0), 0);
        assert_eq!(palette_scroll_index(&resume, 1), 2);
    }

    #[test]
    fn arrows_wrap_while_page_and_boundary_keys_clamp() {
        assert_eq!(next_selection_index(0, 5, -1), Some(4));
        assert_eq!(next_selection_index(4, 5, 1), Some(0));
        assert_eq!(next_selection_index(3, 5, PAGE_STEP), Some(4));
        assert_eq!(next_selection_index(1, 5, -PAGE_STEP), Some(0));
        assert_eq!(next_selection_index(2, 5, isize::MIN), Some(0));
        assert_eq!(next_selection_index(2, 5, isize::MAX), Some(4));
        assert_eq!(next_selection_index(0, 0, 1), None);
    }

    #[test]
    fn no_results_appears_only_after_background_search_settles() {
        assert!(!should_show_command_palette_empty_state(0, true));
        assert!(should_show_command_palette_empty_state(0, false));
        assert!(!should_show_command_palette_empty_state(1, false));
        assert!(should_keep_previous_command_palette_results(0, true, 1));
        assert!(!should_keep_previous_command_palette_results(0, false, 1));
        assert!(!should_keep_previous_command_palette_results(1, true, 1));
    }

    #[test]
    fn result_height_hugs_flat_session_rows_and_message_excerpts() {
        let mut excerpt = item(PaletteSection::Tasks, 1);
        excerpt.content_match = Some(SessionMessageMatch {
            session_id: Uuid::nil(),
            source: MessageRole::User,
            snippet: "Matched phrase".into(),
        });
        let items = [item(PaletteSection::Tasks, 0), excerpt];
        assert_eq!(
            command_palette_results_height(&items, false),
            SECTION_TOP_MARGIN + RESULT_ROW_HEIGHT + CONTENT_RESULT_ROW_HEIGHT
        );
        assert_eq!(
            command_palette_results_height(&[], true),
            EMPTY_RESULTS_HEIGHT
        );
        let resume = [
            item(PaletteSection::Providers, 0),
            item(PaletteSection::Sessions, 1),
        ];
        assert_eq!(
            command_palette_results_height(&resume, false),
            SECTION_TOP_MARGIN + SECTION_HEADER_HEIGHT + RESULT_ROW_HEIGHT * 2.0
        );
    }

    #[test]
    fn render_reads_only_the_cached_result_snapshot() {
        let source = include_str!("./command_palette.rs");
        let start = source
            .find("\n    pub(super) fn render_command_palette(")
            .expect("render function must exist");
        let end = source[start..]
            .find("\n#[cfg(test)]")
            .map(|offset| start + offset)
            .expect("test module marker must exist");
        let render = &source[start..end];
        for forbidden in [
            "refresh_command_palette_results(",
            "session_search_candidates(",
            "session_search_results(",
            "session_message_search(",
            "background_executor(",
            "std::fs",
            "Command::new",
            "read_dir",
        ] {
            assert!(
                !render.contains(forbidden),
                "search render must not call `{forbidden}`"
            );
        }
    }
}
