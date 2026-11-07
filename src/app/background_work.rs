use super::*;

const MAX_BACKGROUND_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_SETTLED_BACKGROUND_ITEMS: usize = 24;
const OUTPUT_CACHE_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
/// How long a parked session keeps waiting for its provider's wake after the
/// last detached work settled. Claude re-enters the model a few seconds after
/// the settle; a first token that slow is a wake in progress, not a no-show.
const BACKGROUND_RESUME_GRACE: Duration = Duration::from_secs(30);
const BACKGROUND_SUMMARY_MENU_ID: &str = "background-work-summary";

#[derive(Default)]
pub(super) struct BackgroundWorkRegistry {
    items: HashMap<BackgroundWorkKey, BackgroundWorkItem>,
    order: Vec<BackgroundWorkKey>,
    /// GPUI's shared text makes repainting a long log O(1). It is rebuilt only
    /// when provider output changes, never from the panel's render path.
    rendered_output: HashMap<BackgroundWorkKey, SharedString>,
    dirty_output: HashSet<BackgroundWorkKey>,
    last_output_cache_refresh: Option<Instant>,
    output_viewports: HashMap<BackgroundWorkKey, BackgroundOutputViewport>,
    selection: TranscriptSelection,
}

#[derive(Clone)]
struct BackgroundOutputViewport {
    scroll_handle: ScrollHandle,
    scrollbar: Rc<ScrollbarState>,
}

impl Default for BackgroundOutputViewport {
    fn default() -> Self {
        Self {
            scroll_handle: ScrollHandle::new(),
            scrollbar: ScrollbarState::new(),
        }
    }
}

#[derive(Clone)]
struct BackgroundSummaryEntry {
    target: BackgroundSummaryTarget,
    kind: BackgroundWorkKind,
    title: String,
    tooltip: String,
    status: BackgroundSummaryStatus,
    can_stop: bool,
    row_focus: FocusHandle,
    stop_focus: FocusHandle,
}

struct BackgroundSummaryListState {
    list: ListState,
    scrollbar: Rc<ScrollbarState>,
    row_keys: Vec<String>,
    environment_focus: FocusHandle,
    navigation_focus: Option<FocusHandle>,
    navigation_generation: u64,
}

enum BackgroundSummaryRow {
    Environment(EnvironmentSummary),
    Separator(&'static str),
    Heading(&'static str, String),
    Work(BackgroundSummaryEntry),
}

impl BackgroundSummaryRow {
    fn key(&self) -> String {
        match self {
            Self::Environment(environment) => if environment.git_available == Some(true) {
                "environment-actions"
            } else {
                "environment-status"
            }
            .into(),
            Self::Separator(key) | Self::Heading(key, _) => (*key).into(),
            Self::Work(entry) => entry.target.control_id(),
        }
    }

    fn height(&self) -> f32 {
        match self {
            Self::Environment(environment) => {
                if environment.git_available == Some(true) {
                    94.0
                } else {
                    62.0
                }
            }
            Self::Separator(_) => 17.0,
            Self::Heading(_, _) => 26.0,
            Self::Work(_) => 34.0,
        }
    }

    fn list_focus(&self, environment_focus: &FocusHandle) -> Option<FocusHandle> {
        match self {
            Self::Environment(_) => Some(environment_focus.clone()),
            Self::Work(entry) => Some(entry.row_focus.clone()),
            Self::Separator(_) | Self::Heading(_, _) => None,
        }
    }

    fn focus_targets(&self, row: usize, targets: &mut Vec<(usize, FocusHandle)>) {
        match self {
            Self::Environment(environment) => {
                if environment.git_available == Some(true) {
                    if environment.commit_status.is_none() {
                        targets.push((row, environment.commit_focus.clone()));
                    }
                    targets.push((row, environment.compare_focus.clone()));
                }
            }
            Self::Work(entry) => {
                targets.push((row, entry.row_focus.clone()));
                if entry.can_stop && entry.status.is_stoppable() {
                    targets.push((row, entry.stop_focus.clone()));
                }
            }
            Self::Separator(_) | Self::Heading(_, _) => {}
        }
    }
}

#[derive(Clone)]
struct BackgroundSummaryTarget(BackgroundWorkKey);

impl BackgroundSummaryTarget {
    fn control_id(&self) -> String {
        format!("native-{}-{}", self.0.provider_id, self.0.kind as u8)
    }

    fn open(
        &self,
        session_id: Uuid,
        raku: &mut Raku,
        _window: &mut Window,
        cx: &mut Context<Raku>,
    ) {
        if raku.state.selected_session == Some(session_id) {
            raku.open_background_work_surface(session_id, self.0.clone(), cx);
        }
    }

    fn stop(&self, session_id: Uuid, raku: &mut Raku, cx: &mut Context<Raku>) {
        if raku.state.selected_session == Some(session_id) {
            raku.stop_background_work(session_id, self.0.clone(), cx);
        }
    }
}

#[derive(Clone, Copy)]
struct BackgroundSummaryStatus(BackgroundWorkStatus);

impl BackgroundSummaryStatus {
    fn label(self) -> String {
        work_status_label(self.0)
    }

    fn is_stoppable(self) -> bool {
        self.0.is_stoppable()
    }

    fn color(self, theme: Theme) -> Hsla {
        work_status_color(self.0, theme)
    }

    fn render_icon(self, kind: BackgroundWorkKind) -> bool {
        background_summary_process_status_icon(kind, self.0).is_some()
    }

    fn icon(self, theme: Theme) -> AnyElement {
        rendered_work_status_icon(self.0, 12.0, self.color(theme))
    }
}
#[derive(Clone)]
struct EnvironmentSummary {
    git_available: Option<bool>,
    /// Branch review line counts from the cached snapshot; `None` until known.
    review_counts: Option<(u64, u64)>,
    commit_status: Option<String>,
    commit_focus: FocusHandle,
    compare_focus: FocusHandle,
}

impl BackgroundWorkRegistry {
    fn apply(&mut self, event: BackgroundWorkEvent) {
        match event {
            BackgroundWorkEvent::Upsert(item) => self.upsert(item),
            BackgroundWorkEvent::OutputDelta { key, delta } => self.append_output(&key, &delta),
            BackgroundWorkEvent::ReconcileProcesses { items } => self.reconcile_processes(items),
            BackgroundWorkEvent::ReconcileLive { items } => self.reconcile_live(items),
            BackgroundWorkEvent::StopRequested(key) => {
                if let Some(item) = self.items.get_mut(&key) {
                    item.status = BackgroundWorkStatus::Stopping;
                    item.updated_at_ms = unix_time_millis();
                }
            }
            BackgroundWorkEvent::StopFailed { key, message } => {
                if let Some(item) = self
                    .items
                    .get_mut(&key)
                    .filter(|item| item.status.is_live())
                {
                    item.status = match item.key.kind {
                        BackgroundWorkKind::Monitor => BackgroundWorkStatus::Monitoring,
                        BackgroundWorkKind::Process | BackgroundWorkKind::Subagent => {
                            BackgroundWorkStatus::Running
                        }
                    };
                    item.detail = Some(message);
                    item.updated_at_ms = unix_time_millis();
                }
            }
        }
        self.trim_settled();
    }

    fn upsert(&mut self, mut incoming: BackgroundWorkItem) {
        bound_output(&mut incoming);
        incoming.settle_from_output();
        incoming.populate_exit_code_from_output();
        let key = incoming.key.clone();
        // Short foreground commands belong in the transcript. Show them while
        // running, then remove them instead of turning this surface into a
        // duplicate command history.
        let already_background = self
            .items
            .get(&incoming.key)
            .is_some_and(|item| item.background);
        if !incoming.background
            && !already_background
            && !incoming.status.is_live()
            && matches!(
                incoming.key.kind,
                BackgroundWorkKind::Process | BackgroundWorkKind::Monitor
            )
        {
            self.remove(&incoming.key);
            return;
        }

        self.output_viewports.entry(key.clone()).or_default();
        let output_changed;
        if let Some(current) = self.items.get_mut(&incoming.key) {
            let preserve_stopping =
                current.status == BackgroundWorkStatus::Stopping && incoming.status.is_stoppable();
            if incoming.title.is_empty() {
                incoming.title.clone_from(&current.title);
            }
            current.title = incoming.title;
            merge_option(&mut current.detail, incoming.detail);
            merge_option(&mut current.command, incoming.command);
            merge_option(&mut current.cwd, incoming.cwd);
            if let Some(output) = incoming.output {
                output_changed = current.output.as_ref() != Some(&output)
                    || current.output_truncated != incoming.output_truncated;
                current.output = Some(output);
                current.output_truncated = incoming.output_truncated;
            } else {
                output_changed = false;
            }
            merge_option(&mut current.duration_ms, incoming.duration_ms);
            merge_option(&mut current.exit_code, incoming.exit_code);
            merge_option(&mut current.control_id, incoming.control_id);
            merge_option(&mut current.origin_activity_id, incoming.origin_activity_id);
            merge_option(&mut current.role, incoming.role);
            merge_option(&mut current.model, incoming.model);
            merge_option(&mut current.parent_id, incoming.parent_id);
            current.started_at_ms = current.started_at_ms.min(incoming.started_at_ms);
            current.updated_at_ms = current.updated_at_ms.max(incoming.updated_at_ms);
            current.background |= incoming.background;
            current.can_stop = if incoming.status.is_live() {
                current.can_stop || incoming.can_stop
            } else {
                false
            };
            if !preserve_stopping {
                current.status = incoming.status;
            }
        } else {
            output_changed = incoming.output.is_some();
            self.order.push(incoming.key.clone());
            self.items.insert(incoming.key.clone(), incoming);
        }
        if output_changed {
            self.dirty_output.insert(key);
        }
    }

    fn append_output(&mut self, key: &BackgroundWorkKey, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let Some(item) = self.items.get_mut(key) else {
            return;
        };
        self.output_viewports.entry(key.clone()).or_default();
        item.output.get_or_insert_with(String::new).push_str(delta);
        item.updated_at_ms = unix_time_millis();
        bound_output(item);
        self.dirty_output.insert(key.clone());
        if item.settle_from_output()
            && !item.background
            && matches!(
                item.key.kind,
                BackgroundWorkKind::Process | BackgroundWorkKind::Monitor
            )
        {
            self.remove(key);
        }
    }

    fn reconcile_processes(&mut self, items: Vec<BackgroundWorkItem>) {
        let present = items
            .iter()
            .map(|item| item.key.clone())
            .collect::<HashSet<_>>();
        let now = unix_time_millis();
        for item in self.items.values_mut() {
            if matches!(
                item.key.kind,
                BackgroundWorkKind::Process | BackgroundWorkKind::Monitor
            ) && item.background
                && item.status.is_live()
                && !present.contains(&item.key)
            {
                item.status = BackgroundWorkStatus::Lost;
                item.can_stop = false;
                item.updated_at_ms = now;
            }
        }
        for item in items {
            self.upsert(item);
        }
    }

    fn reconcile_live(&mut self, items: Vec<BackgroundWorkItem>) {
        let present = items
            .iter()
            .map(|item| item.key.clone())
            .collect::<HashSet<_>>();
        let now = unix_time_millis();
        for item in self.items.values_mut() {
            if item.background && item.status.is_live() && !present.contains(&item.key) {
                item.status = BackgroundWorkStatus::Lost;
                item.can_stop = false;
                item.updated_at_ms = now;
            }
        }
        for item in items {
            self.upsert(item);
        }
    }

    fn remove(&mut self, key: &BackgroundWorkKey) {
        self.items.remove(key);
        self.rendered_output.remove(key);
        self.dirty_output.remove(key);
        self.output_viewports.remove(key);
        self.order.retain(|entry| entry != key);
    }

    fn trim_settled(&mut self) {
        let mut settled = self
            .order
            .iter()
            .filter_map(|key| self.items.get(key).map(|item| (key, item)))
            .filter(|(_, item)| !item.status.is_live())
            .count();
        if settled <= MAX_SETTLED_BACKGROUND_ITEMS {
            return;
        }
        let stale = self.order.clone();
        for key in stale {
            if settled <= MAX_SETTLED_BACKGROUND_ITEMS {
                break;
            }
            if self
                .items
                .get(&key)
                .is_some_and(|item| !item.status.is_live())
            {
                self.remove(&key);
                settled -= 1;
            }
        }
    }

    fn mark_live_lost(&mut self) {
        let now = unix_time_millis();
        let unbackgrounded = self
            .items
            .values()
            .filter(|item| {
                !item.background
                    && matches!(
                        item.key.kind,
                        BackgroundWorkKind::Process | BackgroundWorkKind::Monitor
                    )
            })
            .map(|item| item.key.clone())
            .collect::<Vec<_>>();
        for key in unbackgrounded {
            self.remove(&key);
        }
        for item in self.items.values_mut().filter(|item| item.status.is_live()) {
            item.status = BackgroundWorkStatus::Lost;
            item.can_stop = false;
            item.updated_at_ms = now;
        }
    }

    fn settle_foreground(&mut self, status: BackgroundWorkStatus) {
        let keys = self
            .items
            .values()
            .filter(|item| !item.background)
            .map(|item| item.key.clone())
            .collect::<Vec<_>>();
        let now = unix_time_millis();
        for key in keys {
            if key.kind == BackgroundWorkKind::Subagent {
                if let Some(item) = self.items.get_mut(&key) {
                    if item.status.is_live() {
                        item.status = status;
                        item.can_stop = false;
                        item.updated_at_ms = now;
                    }
                }
            } else {
                self.remove(&key);
            }
        }
    }

    fn has_live(&self) -> bool {
        self.items.values().any(|item| item.status.is_live())
    }

    fn has_live_detached(&self) -> bool {
        self.items
            .values()
            .any(|item| item.background && item.status.is_live())
    }

    fn counts(&self) -> (usize, usize) {
        self.items
            .values()
            .filter(|item| item.status.is_live())
            .fold((0, 0), |(processes, agents), item| match item.key.kind {
                BackgroundWorkKind::Subagent => (processes, agents + 1),
                BackgroundWorkKind::Process | BackgroundWorkKind::Monitor => {
                    (processes + 1, agents)
                }
            })
    }

    fn ordered_items(&self) -> Vec<&BackgroundWorkItem> {
        self.order
            .iter()
            .rev()
            .filter_map(|key| self.items.get(key))
            .collect()
    }

    pub(super) fn selected_text(&self) -> Option<String> {
        self.selection.selection.borrow().selected_text()
    }

    fn refresh_output_cache(&mut self) -> bool {
        if self.dirty_output.is_empty()
            || self
                .last_output_cache_refresh
                .is_some_and(|last| last.elapsed() < OUTPUT_CACHE_REFRESH_INTERVAL)
        {
            return false;
        }
        let dirty = std::mem::take(&mut self.dirty_output);
        for key in dirty {
            if let Some(output) = self.items.get(&key).and_then(|item| item.output.as_deref()) {
                self.rendered_output
                    .insert(key.clone(), SharedString::from(strip_ansi(output)));
                if let Some(viewport) = self.output_viewports.get(&key) {
                    viewport.scroll_handle.scroll_to_bottom();
                }
            }
        }
        self.last_output_cache_refresh = Some(Instant::now());
        true
    }

    fn output_refresh_delay(&self) -> Option<Duration> {
        (!self.dirty_output.is_empty()).then(|| {
            self.last_output_cache_refresh
                .map(|last| OUTPUT_CACHE_REFRESH_INTERVAL.saturating_sub(last.elapsed()))
                .unwrap_or_default()
        })
    }
}

fn merge_option<T>(target: &mut Option<T>, incoming: Option<T>) {
    if incoming.is_some() {
        *target = incoming;
    }
}

fn bound_output(item: &mut BackgroundWorkItem) {
    let Some(output) = item.output.as_mut() else {
        return;
    };
    if output.len() <= MAX_BACKGROUND_OUTPUT_BYTES {
        return;
    }
    let mut cut = output.len() - MAX_BACKGROUND_OUTPUT_BYTES;
    while !output.is_char_boundary(cut) {
        cut += 1;
    }
    output.drain(..cut);
    item.output_truncated = true;
}

pub(super) fn work_status_label(status: BackgroundWorkStatus) -> String {
    match status {
        BackgroundWorkStatus::Starting => tr!("background.status.starting"),
        BackgroundWorkStatus::Running => tr!("background.status.running"),
        BackgroundWorkStatus::Monitoring => tr!("background.status.monitoring"),
        BackgroundWorkStatus::Stopping => tr!("background.status.stopping"),
        BackgroundWorkStatus::Completed => tr!("background.status.completed"),
        BackgroundWorkStatus::Failed => tr!("background.status.failed"),
        BackgroundWorkStatus::Stopped => tr!("background.status.stopped"),
        BackgroundWorkStatus::Lost => tr!("background.status.lost"),
    }
}

fn work_status_icon(status: BackgroundWorkStatus) -> &'static str {
    match status {
        BackgroundWorkStatus::Starting
        | BackgroundWorkStatus::Running
        | BackgroundWorkStatus::Monitoring => "icons/loader-circle.svg",
        BackgroundWorkStatus::Stopping | BackgroundWorkStatus::Stopped => "icons/stop.svg",
        BackgroundWorkStatus::Completed => "icons/check.svg",
        BackgroundWorkStatus::Failed => "icons/x.svg",
        BackgroundWorkStatus::Lost => "icons/alert.svg",
    }
}

fn background_summary_process_status_icon(
    kind: BackgroundWorkKind,
    status: BackgroundWorkStatus,
) -> Option<&'static str> {
    if !matches!(
        kind,
        BackgroundWorkKind::Process | BackgroundWorkKind::Monitor
    ) {
        return None;
    }

    match status {
        BackgroundWorkStatus::Starting
        | BackgroundWorkStatus::Running
        | BackgroundWorkStatus::Monitoring
        | BackgroundWorkStatus::Completed
        | BackgroundWorkStatus::Failed => Some(work_status_icon(status)),
        _ => None,
    }
}

fn rendered_work_status_icon(status: BackgroundWorkStatus, size: f32, color: Hsla) -> AnyElement {
    let icon = icon(work_status_icon(status), size, color);
    if matches!(
        status,
        BackgroundWorkStatus::Starting
            | BackgroundWorkStatus::Running
            | BackgroundWorkStatus::Monitoring
    ) {
        // Background work runs for minutes; don't price its pane at full rate.
        motion::spin_slow(icon)
    } else {
        icon.into_any_element()
    }
}

pub(super) fn work_status_color(status: BackgroundWorkStatus, theme: Theme) -> Hsla {
    match status {
        BackgroundWorkStatus::Starting
        | BackgroundWorkStatus::Running
        | BackgroundWorkStatus::Monitoring => theme.accent,
        BackgroundWorkStatus::Completed => theme.success,
        BackgroundWorkStatus::Failed | BackgroundWorkStatus::Lost => theme.danger,
        BackgroundWorkStatus::Stopping | BackgroundWorkStatus::Stopped => theme.text_tertiary,
    }
}

pub(super) fn work_kind_icon(kind: BackgroundWorkKind) -> &'static str {
    match kind {
        BackgroundWorkKind::Subagent => "icons/bot.svg",
        BackgroundWorkKind::Process | BackgroundWorkKind::Monitor => "icons/terminal-square.svg",
    }
}

fn work_elapsed(item: &BackgroundWorkItem) -> String {
    let duration_ms = item.duration_ms.unwrap_or_else(|| {
        let end = if item.status.is_live() {
            unix_time_millis()
        } else {
            item.updated_at_ms
        };
        end.saturating_sub(item.started_at_ms)
    });
    let seconds = duration_ms / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3_600, (seconds % 3_600) / 60)
    }
}

impl Raku {
    pub(super) fn background_output_refresh_delay(&self) -> Option<Duration> {
        self.background_work
            .values()
            .filter_map(BackgroundWorkRegistry::output_refresh_delay)
            .min()
    }

    pub(super) fn observe_foreground_command_activity(
        &mut self,
        session_id: Uuid,
        activity: &ActivityItem,
    ) {
        if activity.kind != crate::model::ActivityKind::Command
            || self
                .state
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .is_some_and(|session| {
                    matches!(session.provider, ProviderKind::Codex | ProviderKind::Claude)
                })
        {
            return;
        }
        let provider_id = activity
            .source_id
            .clone()
            .unwrap_or_else(|| activity.id.to_string());
        let status = if !activity.complete {
            BackgroundWorkStatus::Running
        } else if activity.failed {
            BackgroundWorkStatus::Failed
        } else {
            BackgroundWorkStatus::Completed
        };
        let mut item = BackgroundWorkItem::new(
            BackgroundWorkKind::Process,
            provider_id.clone(),
            activity.title.clone(),
            status,
        );
        item.command = activity.display_target.clone();
        item.detail = activity.detail.clone();
        item.output = activity.output.clone();
        item.origin_activity_id = Some(provider_id);
        self.handle_background_work_event(session_id, BackgroundWorkEvent::Upsert(item));
    }

    pub(super) fn handle_background_work_event(
        &mut self,
        session_id: Uuid,
        event: BackgroundWorkEvent,
    ) {
        self.background_work
            .entry(session_id)
            .or_default()
            .apply(event);
    }

    pub(super) fn mark_background_work_lost(&mut self, session_id: Uuid) {
        if let Some(registry) = self.background_work.get_mut(&session_id) {
            registry.mark_live_lost();
        }
    }

    pub(super) fn settle_foreground_work(
        &mut self,
        session_id: Uuid,
        status: BackgroundWorkStatus,
    ) {
        if let Some(registry) = self.background_work.get_mut(&session_id) {
            registry.settle_foreground(status);
        }
    }

    pub(super) fn session_has_live_background_work(&self, session_id: Uuid) -> bool {
        self.background_work
            .get(&session_id)
            .is_some_and(BackgroundWorkRegistry::has_live)
    }

    pub(super) fn session_has_live_detached_work(&self, session_id: Uuid) -> bool {
        self.background_work
            .get(&session_id)
            .is_some_and(BackgroundWorkRegistry::has_live_detached)
    }

    pub(super) fn background_work_counts(&self, session_id: Uuid) -> (usize, usize) {
        self.background_work
            .get(&session_id)
            .map(BackgroundWorkRegistry::counts)
            .unwrap_or_default()
    }

    pub(super) fn background_work_for_activity(
        &self,
        session_id: Uuid,
        activity_id: &str,
    ) -> Option<&BackgroundWorkItem> {
        self.background_work
            .get(&session_id)?
            .items
            .values()
            .find(|item| item.origin_activity_id.as_deref() == Some(activity_id))
    }

    pub(super) fn maybe_refresh_background_work(&mut self, cx: &mut Context<Self>) {
        let mut output_changed = false;
        for registry in self.background_work.values_mut() {
            output_changed |= registry.refresh_output_cache();
        }
        if output_changed {
            cx.notify();
        }
        let selected = self.state.selected_session;
        for (session_id, runtime) in &mut self.runtimes {
            let should_refresh = selected == Some(*session_id)
                || self
                    .background_work
                    .get(session_id)
                    .is_some_and(BackgroundWorkRegistry::has_live);
            if should_refresh
                && runtime.last_background_refresh_at.elapsed() >= BACKGROUND_WORK_REFRESH_INTERVAL
            {
                runtime.last_background_refresh_at = Instant::now();
                runtime.driver.refresh_background_work();
            }
        }

        if self.last_background_work_tick.elapsed() >= BACKGROUND_WORK_TICK_INTERVAL
            && selected.is_some_and(|session_id| self.session_has_live_background_work(session_id))
        {
            self.last_background_work_tick = Instant::now();
            cx.notify();
        }

        // A parked turn waits for its provider to wake it, which Claude does
        // within seconds of a settle. Past that grace with no detached work
        // left there is nothing to wait for, so the held turn settles the way
        // its result would have — through the ordinary finish path.
        let unparked = self
            .runtimes
            .iter()
            .filter(|(_, runtime)| runtime.last_active_at.elapsed() >= BACKGROUND_RESUME_GRACE)
            .map(|(session_id, _)| *session_id)
            .filter(|session_id| {
                !self.session_has_live_detached_work(*session_id)
                    && self
                        .state
                        .sessions
                        .iter()
                        .find(|session| session.id == *session_id)
                        .is_some_and(|session| session.status == SessionStatus::Background)
            })
            .collect::<Vec<_>>();
        for session_id in unparked {
            let Some(mut runtime) = self.runtimes.remove(&session_id) else {
                continue;
            };
            let keep_runtime = self.handle_driver_event(
                session_id,
                &mut runtime,
                DriverEvent::TurnFinished {
                    success: true,
                    summary: None,
                },
                true,
                cx,
            );
            if keep_runtime {
                self.runtimes.insert(session_id, runtime);
            }
            self.state.mark_session_dirty(session_id);
            cx.notify();
        }
    }

    pub(super) fn stop_background_work(
        &mut self,
        session_id: Uuid,
        key: BackgroundWorkKey,
        cx: &mut Context<Self>,
    ) {
        let control_id = self
            .background_work
            .get(&session_id)
            .and_then(|registry| registry.items.get(&key))
            .filter(|item| item.status.is_stoppable() && item.can_stop)
            .and_then(|item| item.control_id.clone());
        let Some(control_id) = control_id else {
            return;
        };
        let Some(driver) = self
            .runtimes
            .get(&session_id)
            .map(|runtime| runtime.driver.clone())
        else {
            self.mark_background_work_lost(session_id);
            cx.notify();
            return;
        };
        self.handle_background_work_event(
            session_id,
            BackgroundWorkEvent::StopRequested(key.clone()),
        );
        driver.stop_background_work(key, control_id);
        cx.notify();
    }

    pub(super) fn open_background_work_surface(
        &mut self,
        session_id: Uuid,
        key: BackgroundWorkKey,
        cx: &mut Context<Self>,
    ) {
        let Some(title) = self
            .background_work
            .get(&session_id)
            .and_then(|registry| registry.items.get(&key))
            .map(|item| item.title.clone())
        else {
            return;
        };
        if self.state.selected_session != Some(session_id) {
            self.select_session(session_id, cx);
        }
        self.open_right_panel_surface(RightPanelSurface::BackgroundWork { key, title }, cx);
    }

    fn render_current_background_summary_card(
        &mut self,
        handle: &ContextMenuHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = self.selected_session();
        let session_id = session.map(|session| session.id).unwrap_or_else(Uuid::nil);
        let mut entries = Vec::new();
        if let Some(registry) = self.background_work.get(&session_id) {
            for item in registry.ordered_items() {
                if !item.background
                    && matches!(
                        item.key.kind,
                        BackgroundWorkKind::Process | BackgroundWorkKind::Monitor
                    )
                    && !item.status.is_live()
                {
                    continue;
                }
                let target = BackgroundSummaryTarget(item.key.clone());
                let id = target.control_id();
                let status = BackgroundSummaryStatus(item.status);
                let tooltip = match item.model.as_deref() {
                    Some(model) => format!("{}\n{model}\n{}", item.title, status.label()),
                    None => format!("{}\n{}", item.title, status.label()),
                };
                entries.push(BackgroundSummaryEntry {
                    target,
                    kind: item.key.kind,
                    title: item.title.clone(),
                    tooltip,
                    status,
                    can_stop: item.can_stop && item.status.is_stoppable(),
                    row_focus: self
                        .transcript_control_focus(format!("background-summary-row-{id}"), cx),
                    stop_focus: self
                        .transcript_control_focus(format!("background-summary-stop-{id}"), cx),
                });
            }
        }
        let workspace_path = self
            .selected_workspace_path()
            .or_else(|| {
                self.selected_project()
                    .map(|project| project.path.as_path())
            })
            .map(std::path::Path::to_path_buf);
        let mut review_counts = None;
        let git_available = workspace_path.map_or(Some(false), |path| {
            let snapshot = self.branch_snapshot_for_workspace(&path, cx).or_else(|| {
                self.visible_branch_snapshot
                    .as_ref()
                    .filter(|(snapshot_path, _)| snapshot_path == &path)
                    .map(|(_, snapshot)| snapshot.clone())
            });
            if let Some(snapshot) = snapshot {
                review_counts = Some((snapshot.branch_additions, snapshot.branch_deletions));
                Some(true)
            } else {
                match self.branch_snapshots.read(&path) {
                    Query::Ready(_) => Some(false),
                    Query::Pending | Query::Missing(_) => None,
                }
            }
        });
        let environment = Some(EnvironmentSummary {
            git_available,
            review_counts,
            commit_status: self.commit_operation_status_label(),
            commit_focus: self.transcript_control_focus("environment-summary-commit", cx),
            compare_focus: self.transcript_control_focus("environment-summary-compare", cx),
        });
        render_background_summary_card(
            handle,
            session_id,
            environment,
            entries,
            cx.entity().downgrade(),
            window,
            cx,
        )
    }

    pub(super) fn render_background_work_summary(&self, cx: &mut Context<Self>) -> AnyElement {
        let session = self.selected_session();
        let session_id = session.map(|session| session.id);
        let diff_selected = self.right_panel_visible
            && self.active_right_panel_surface() == Some(&RightPanelSurface::Diff);
        let (processes, agents) = session_id
            .map(|session_id| self.background_work_counts(session_id))
            .unwrap_or_default();
        let has_live_work = processes > 0 || agents > 0;
        let theme = Theme::current(cx);
        let refresh_weak = cx.entity().downgrade();
        let handle = self.menu_handle_with(BACKGROUND_SUMMARY_MENU_ID, cx, move |open, _, cx| {
            if open {
                let _ = refresh_weak.update(cx, |this, cx| {
                    this.refresh_selected_branch_snapshot(cx);
                });
            }
        });
        let info_selected = handle.is_open();
        let trigger = div()
            .id("environment-summary-trigger")
            .size(px(28.0))
            .relative()
            .rounded(px(7.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .when(!info_selected, |style| {
                style.hover(|style| style.bg(theme.overlay))
            })
            .when(info_selected, |style| style.bg(theme.overlay_strong))
            .child(icon("icons/ant/card-list.svg", 20.0, theme.text))
            .when(has_live_work, |trigger| {
                trigger.child(
                    div()
                        .absolute()
                        .top(px(4.0))
                        .right(px(4.0))
                        .child(pulse_dot(5.0, theme.accent)),
                )
            });
        let diff = {
            let focus = self.transcript_control_focus("header-git-status", cx);
            div()
                .id("header-git-status")
                .track_focus(&focus)
                .tab_index(0)
                .tab_stop(true)
                .size(px(28.0))
                .rounded(px(7.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .cursor_default()
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .when(diff_selected, |style| style.bg(theme.overlay_strong))
                .when(!diff_selected, |style| {
                    style.hover(|style| style.bg(theme.overlay))
                })
                .active(|style| style.bg(theme.overlay_strong))
                .child(icon("icons/file-diff.svg", 20.0, theme.text))
                .tooltip(Tooltip::text(tr!("environment.changes")))
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_right_panel_diff(cx);
                }))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if !event.keystroke.modifiers.modified()
                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        this.toggle_right_panel_diff(cx);
                        cx.stop_propagation();
                    }
                }))
                .into_any_element()
        };
        let weak = cx.entity().downgrade();
        let info = popover(
            trigger,
            &handle,
            MenuAlign::BelowRight,
            move |handle, window, cx| {
                // Re-read the selected session when the open surface renders:
                // polling/status changes must not retain a trigger-time snapshot.
                weak.update(cx, |this, cx| {
                    this.render_current_background_summary_card(handle, window, cx)
                })
                .unwrap_or_else(|_| div().into_any_element())
            },
        );
        div()
            .id("header-environment-controls")
            .tab_group()
            .tab_stop(false)
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(info)
            .child(diff)
            .child(self.render_right_panel_toggle(cx))
            .children(self.render_side_chat_trigger(cx))
            .into_any_element()
    }

    pub(super) fn render_background_work_surface(
        &self,
        key: &BackgroundWorkKey,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let session_id = self.state.selected_session;
        let registry = session_id.and_then(|session_id| self.background_work.get(&session_id));
        let item = registry.and_then(|registry| registry.items.get(key));
        let Some(item) = item else {
            return div()
                .id("background-work-surface")
                .tab_group()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(7.0))
                        .child(icon(work_kind_icon(key.kind), 22.0, theme.text_ghost))
                        .child(
                            div()
                                .text_size(sp(12.5))
                                .text_color(theme.text_secondary)
                                .child(tr!("background.no_work")),
                        ),
                );
        };
        let output = registry
            .and_then(|registry| registry.rendered_output.get(key))
            .cloned();
        let output_viewport = registry
            .and_then(|registry| registry.output_viewports.get(key))
            .cloned()
            .unwrap_or_default();
        let selection = registry
            .map(|registry| registry.selection.clone())
            .unwrap_or_default();
        let status_color = work_status_color(item.status, theme);
        let stop = session_id.and_then(|session_id| {
            (item.status.is_stoppable() && item.can_stop).then(|| {
                let focus = self.transcript_control_focus(
                    format!(
                        "background-surface-stop-{}-{}",
                        item.key.provider_id, item.key.kind as u8
                    ),
                    cx,
                );
                let click_key = item.key.clone();
                let click_weak = cx.entity().downgrade();
                let key_key = item.key.clone();
                let key_weak = cx.entity().downgrade();
                div()
                    .id(SharedString::from(format!(
                        "background-surface-stop-{}-{}",
                        item.key.provider_id, item.key.kind as u8
                    )))
                    .track_focus(&focus)
                    .tab_index(0)
                    .h(px(26.0))
                    .px(px(9.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .cursor_default()
                    .text_size(sp(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text_secondary)
                    .hover(|style| style.bg(theme.danger.opacity(0.10)))
                    .active(|style| style.bg(theme.danger.opacity(0.16)))
                    .focus_visible(|style| style.border_color(theme.accent))
                    .tooltip(Tooltip::text(tr!("background.stop")))
                    .child(icon("icons/stop-filled.svg", 11.0, theme.danger))
                    .child(tr!("background.stop"))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        let _ = click_weak.update(cx, |this, cx| {
                            this.stop_background_work(session_id, click_key.clone(), cx);
                        });
                    })
                    .on_key_down(move |event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            let _ = key_weak.update(cx, |this, cx| {
                                this.stop_background_work(session_id, key_key.clone(), cx);
                            });
                            cx.stop_propagation();
                        }
                    })
            })
        });
        let card = div()
            .w_full()
            .flex()
            .flex_col()
            .rounded(px(9.0))
            .border_1()
            .border_color(theme.border)
            .overflow_hidden()
            .bg(theme.surface)
            .child(
                div()
                    .min_h(px(54.0))
                    .px(px(11.0))
                    .py(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .child(icon(
                        work_kind_icon(item.key.kind),
                        15.0,
                        theme.text_secondary,
                    ))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(sp(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(single_line_label(&item.title)),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .text_size(sp(12.5))
                                    .text_color(theme.text_tertiary)
                                    .child(rendered_work_status_icon(
                                        item.status,
                                        9.0,
                                        status_color,
                                    ))
                                    .child(work_status_label(item.status))
                                    .child("·")
                                    .child(work_elapsed(item)),
                            ),
                    )
                    .when_some(stop, |header, stop| header.child(stop)),
            )
            .child(self.render_background_work_detail(
                item,
                output,
                output_viewport,
                selection,
                cx,
            ));
        div()
            .id("background-work-surface")
            .tab_group()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(12.0))
            .child(card)
    }

    fn render_background_work_detail(
        &self,
        item: &BackgroundWorkItem,
        output: Option<SharedString>,
        output_viewport: BackgroundOutputViewport,
        selection: TranscriptSelection,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let mut detail = div().w_full().flex().flex_col().bg(theme.surface);
        let mut metadata = Vec::new();
        // A subagent's "command" is the prompt it was launched with.
        let command_label = match item.key.kind {
            BackgroundWorkKind::Subagent => tr!("background.prompt"),
            BackgroundWorkKind::Process | BackgroundWorkKind::Monitor => tr!("background.command"),
        };
        for (label, value) in [
            (command_label, item.command.as_ref()),
            (tr!("background.cwd"), item.cwd.as_ref()),
            (tr!("background.role"), item.role.as_ref()),
            (tr!("background.model"), item.model.as_ref()),
            (tr!("background.latest_update"), item.detail.as_ref()),
        ] {
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                metadata.push((label, value.clone()));
            }
        }
        if let Some(exit_code) = item.exit_code {
            metadata.push((tr!("background.exit_code"), exit_code.to_string()));
        }
        for (label, value) in metadata {
            detail = detail.child(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .px(px(10.0))
                    .py(px(7.0))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .text_color(theme.text_tertiary)
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .font_family(md::render::MONO_FAMILY)
                            .text_color(theme.text_secondary)
                            .child(value),
                    ),
            );
        }
        let output = output.unwrap_or_else(|| SharedString::from(tr!("background.no_output")));
        let output_flat = md::render::flatten_plain(
            output,
            md::render::MONO_FAMILY,
            FontWeight::NORMAL,
            theme.text_secondary,
        );
        let output_text = md::render::selectable_flat_text(
            &output_flat,
            crate::md::selection::TextKey::new(
                format!(
                    "background-output-{}-{}",
                    item.key.provider_id, item.key.kind as u8
                ),
                0,
            ),
            selection.clone(),
            theme.code_wash,
            theme.selection,
            false,
        );
        detail.child(
            div()
                .border_t_1()
                .border_color(theme.border)
                .p(px(10.0))
                .flex()
                .flex_col()
                .gap(px(5.0))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .text_size(sp(12.5))
                        .text_color(theme.text_tertiary)
                        .child(tr!("background.output"))
                        .when(item.output_truncated, |header| {
                            header.child(tr!("background.output_truncated"))
                        }),
                )
                .child(
                    div()
                        .relative()
                        .max_h(px(320.0))
                        .rounded(px(6.0))
                        .overflow_hidden()
                        .bg(theme.terminal)
                        .child(md::render::frame_reset(selection.clone()))
                        .child(
                            div()
                                .id(SharedString::from(format!(
                                    "background-output-scroll-{}-{}",
                                    item.key.provider_id, item.key.kind as u8
                                )))
                                .max_h(px(320.0))
                                .overflow_y_scroll()
                                .track_scroll(&output_viewport.scroll_handle)
                                .on_scroll_wheel({
                                    let scroll = output_viewport.scroll_handle.clone();
                                    move |_, _, cx| contain_scroll(&scroll, cx)
                                })
                                .p(px(8.0))
                                .text_size(sp(12.5))
                                .line_height(sp(15.0))
                                .font_family(md::render::MONO_FAMILY)
                                .text_color(theme.text_secondary)
                                .child(output_text),
                        )
                        .child(scrollbar::vertical(
                            &output_viewport.scroll_handle,
                            &output_viewport.scrollbar,
                        ))
                        .child(background_work_selection_input(selection)),
                ),
        )
    }
}

fn background_work_selection_input(selection: TranscriptSelection) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |_, _, window, _| md::render::install_selection_input(window, &selection),
    )
    .absolute()
    .w(px(0.0))
    .h(px(0.0))
}

fn background_summary_navigation_index(
    key: &str,
    shift: bool,
    current: Option<usize>,
    count: usize,
) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let current = current.filter(|index| *index < count);
    match key {
        "home" => Some(0),
        "end" => Some(count - 1),
        "up" | "tab" if key == "up" || shift => Some(
            current
                .and_then(|index| index.checked_sub(1))
                .unwrap_or(count - 1),
        ),
        "down" | "tab" => Some(current.map(|index| (index + 1) % count).unwrap_or(0)),
        _ => None,
    }
}

fn render_background_summary_card(
    handle: &ContextMenuHandle,
    session_id: Uuid,
    environment: Option<EnvironmentSummary>,
    entries: Vec<BackgroundSummaryEntry>,
    weak: WeakEntity<Raku>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let theme = Theme::current(cx);
    let (agents, processes): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .partition(|entry| entry.kind == BackgroundWorkKind::Subagent);
    let mut rows = Vec::new();
    if let Some(environment) = environment {
        rows.push(BackgroundSummaryRow::Environment(environment));
    }
    if !rows.is_empty() && (!processes.is_empty() || !agents.is_empty()) {
        rows.push(BackgroundSummaryRow::Separator("environment-separator"));
    }
    for (key, label, entries) in [
        ("processes", tr!("background.processes"), processes),
        ("agents", tr!("background.agents"), agents),
    ] {
        if !entries.is_empty() {
            rows.push(BackgroundSummaryRow::Heading(key, label));
            rows.extend(entries.into_iter().map(BackgroundSummaryRow::Work));
        }
    }
    let height = rows.iter().map(BackgroundSummaryRow::height).sum::<f32>();
    let keys = rows
        .iter()
        .map(BackgroundSummaryRow::key)
        .collect::<Vec<_>>();
    let mut targets = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        row.focus_targets(index, &mut targets);
    }
    let state = window.use_keyed_state(
        SharedString::from(format!("background-summary-list-{session_id}")),
        cx,
        |_, cx| {
            let list = ListState::new(0, ListAlignment::Top, px(0.0));
            list.set_scroll_handler(|_, window, _| window.refresh());
            BackgroundSummaryListState {
                list,
                scrollbar: ScrollbarState::new(),
                row_keys: Vec::new(),
                environment_focus: cx.focus_handle(),
                navigation_focus: None,
                navigation_generation: 0,
            }
        },
    );
    let (list_state, scrollbar_state, environment_focus) = state.update(cx, |state, _| {
        if state.row_keys != keys {
            let prefix = state
                .row_keys
                .iter()
                .zip(&keys)
                .take_while(|(old, new)| old == new)
                .count();
            let suffix = state.row_keys[prefix..]
                .iter()
                .rev()
                .zip(keys[prefix..].iter().rev())
                .take_while(|(old, new)| old == new)
                .count();
            state.list.splice_focusable(
                prefix..state.row_keys.len() - suffix,
                rows[prefix..rows.len() - suffix]
                    .iter()
                    .map(|row| row.list_focus(&state.environment_focus)),
            );
            state.list = state.list.clone().with_uniform_item_height(px(34.0));
            state.row_keys = keys;
            state.navigation_focus = None;
            state.navigation_generation = state.navigation_generation.wrapping_add(1);
        }
        if state
            .navigation_focus
            .as_ref()
            .is_some_and(|focus| !targets.iter().any(|(_, candidate)| candidate == focus))
        {
            state.navigation_focus = None;
            state.navigation_generation = state.navigation_generation.wrapping_add(1);
        }
        (
            state.list.clone(),
            state.scrollbar.clone(),
            state.environment_focus.clone(),
        )
    });
    let targets = Rc::new(targets);
    let rows = Rc::new(rows);
    let row_handle = handle.clone();
    let keyboard_handle = handle.clone();
    let keyboard_list = list_state.clone();
    let keyboard_weak = weak.clone();
    let content = div()
        .id("background-summary-scroll")
        .relative()
        .w_full()
        .h(px(height.min(402.0)))
        .child(
            list(list_state.clone(), move |index, _, _| {
                let row = &rows[index];
                let child = match row {
                    BackgroundSummaryRow::Environment(environment) => {
                        render_environment_summary_section(
                            environment.clone(),
                            row_handle.clone(),
                            weak.clone(),
                            &theme,
                        )
                        .into_any_element()
                    }
                    BackgroundSummaryRow::Separator(_) => div()
                        .pt(px(8.0))
                        .child(div().mx(px(8.0)).h(px(1.0)).bg(theme.border))
                        .into_any_element(),
                    BackgroundSummaryRow::Heading(_, label) => div()
                        .h(px(26.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .text_size(sp(13.0))
                        .text_color(theme.text_tertiary)
                        .child(label.clone())
                        .into_any_element(),
                    BackgroundSummaryRow::Work(entry) => render_background_summary_row(
                        entry.clone(),
                        session_id,
                        row_handle.clone(),
                        weak.clone(),
                        &theme,
                    )
                    .into_any_element(),
                };
                div()
                    .id(SharedString::from(row.key()))
                    .w_full()
                    .h(px(row.height()))
                    .when(matches!(row, BackgroundSummaryRow::Environment(_)), |row| {
                        row.track_focus(&environment_focus)
                    })
                    .child(child)
                    .into_any_element()
            })
            .size_full(),
        )
        .child(scrollbar::vertical(&list_state, &scrollbar_state));
    div()
        .id("background-summary-card")
        .track_focus(handle.focus_handle())
        .w(px(300.0))
        .h(px((height + 18.0).min(420.0)))
        .p(px(8.0))
        .rounded(px(12.0))
        .border_1()
        .border_color(theme.border_strong)
        .overflow_hidden()
        .bg(theme.menu)
        .shadow_lg()
        .child(content)
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            let key = event.keystroke.key.as_str();
            if !matches!(key, "up" | "down" | "home" | "end" | "tab")
                || event.keystroke.modifiers.control
                || event.keystroke.modifiers.platform
                || event.keystroke.modifiers.alt
            {
                return;
            }
            let Some((index, generation)) = state.update(cx, |state, _| {
                let current = targets
                    .iter()
                    .position(|(_, focus)| focus.is_focused(window))
                    .or_else(|| {
                        state.navigation_focus.as_ref().and_then(|focus| {
                            targets.iter().position(|(_, candidate)| candidate == focus)
                        })
                    });
                let index = background_summary_navigation_index(
                    key,
                    event.keystroke.modifiers.shift,
                    current,
                    targets.len(),
                )?;
                state.navigation_focus = Some(targets[index].1.clone());
                state.navigation_generation = state.navigation_generation.wrapping_add(1);
                Some((index, state.navigation_generation))
            }) else {
                return;
            };
            let (row, focus) = &targets[index];
            if !matches!(key, "home" | "end") && keyboard_list.bounds_for_item(*row).is_some() {
                keyboard_list.scroll_to_reveal_item(*row);
            } else {
                // Unmeasured rows have no reliable pixel position yet.
                // Anchor directly by item so End reaches the unseen ID controls.
                keyboard_list.scroll_to(ListOffset {
                    item_ix: *row,
                    offset_in_item: px(0.0),
                });
            }
            // Keep dispatch on the card while the destination control is
            // mounted by the next layout, including offscreen copy/stop buttons.
            keyboard_handle.focus_handle().focus(window, cx);
            let focus = focus.clone();
            let state = state.downgrade();
            let handle = keyboard_handle.clone();
            let weak = keyboard_weak.clone();
            window.on_next_frame(move |window, _| {
                window.on_next_frame(move |window, cx| {
                    let Some(state) = state.upgrade() else {
                        return;
                    };
                    let same_session = weak.upgrade().is_some_and(|raku| {
                        raku.read(cx)
                            .selected_session()
                            .map(|session| session.id)
                            .unwrap_or_else(Uuid::nil)
                            == session_id
                    });
                    if same_session
                        && handle.is_open()
                        && state.read(cx).navigation_generation == generation
                    {
                        focus.focus(window, cx);
                        window.refresh();
                    }
                });
            });
            window.refresh();
            cx.stop_propagation();
        })
        .into_any_element()
}

fn render_environment_summary_section(
    environment: EnvironmentSummary,
    handle: ContextMenuHandle,
    weak: WeakEntity<Raku>,
    theme: &Theme,
) -> Div {
    let section = div().w_full().flex().flex_col().gap_0().child(
        div()
            .h(px(30.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .text_size(sp(13.5))
            .text_color(theme.text_tertiary)
            .child(tr!("environment.title")),
    );
    if environment.git_available != Some(true) {
        return section.child(
            div()
                .h(px(32.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .text_size(sp(13.0))
                .text_color(theme.text_secondary)
                .child(if environment.git_available.is_none() {
                    tr!("environment.checking_git")
                } else {
                    tr!("environment.git_unavailable")
                }),
        );
    }

    let commit_handle = handle.clone();
    let commit_weak = weak.clone();
    let commit_pending = environment.commit_status.is_some();
    let commit = render_environment_action_row(
        "environment-summary-commit",
        &environment.commit_focus,
        "icons/git-commit-horizontal.svg",
        environment
            .commit_status
            .unwrap_or_else(|| tr!("environment.commit_or_push")),
        None,
        !commit_pending,
        commit_pending,
        theme,
        move |window, cx| {
            commit_handle.close(window, cx);
            window.refresh();
            let _ = commit_weak.update(cx, |this, cx| {
                this.open_commit_dialog(window, cx);
            });
        },
    );

    let compare_handle = handle;
    let compare_weak = weak;
    let compare = render_environment_action_row(
        "environment-summary-compare",
        &environment.compare_focus,
        "icons/file-diff.svg",
        tr!("environment.review_changes"),
        environment
            .review_counts
            .filter(|(additions, deletions)| *additions > 0 || *deletions > 0),
        true,
        false,
        theme,
        move |window, cx| {
            compare_handle.close(window, cx);
            window.refresh();
            let _ = compare_weak.update(cx, |this, cx| {
                this.set_right_panel_diff_source(ReviewDiffSource::Branch, cx);
            });
        },
    );

    section.child(commit).child(compare)
}

fn render_environment_action_row(
    id: &'static str,
    focus: &FocusHandle,
    icon_path: &'static str,
    label: String,
    line_counts: Option<(u64, u64)>,
    enabled: bool,
    active: bool,
    theme: &Theme,
    action: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let foreground = if enabled {
        theme.text
    } else if active {
        theme.text_secondary
    } else {
        theme.text_ghost
    };
    let icon_foreground = if enabled || active {
        theme.text_secondary
    } else {
        theme.text_ghost
    };
    let indicator = if active {
        motion::spin_slow(icon("icons/loader-circle.svg", 14.0, theme.text_secondary))
    } else {
        icon(icon_path, 14.0, icon_foreground).into_any_element()
    };
    let action: Rc<dyn Fn(&mut Window, &mut App)> = Rc::new(action);
    let key_action = action.clone();
    div()
        .id(id)
        .track_focus(focus)
        .when(enabled, |row| row.tab_index(0))
        .min_h(px(32.0))
        .w_full()
        .px(px(8.0))
        .rounded(px(8.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .cursor_default()
        .focus_visible(|style| style.border_1().border_color(theme.accent))
        .when(enabled, |row| {
            row.hover(|style| style.bg(theme.overlay_strong))
        })
        .child(indicator)
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_size(sp(13.5))
                .text_color(foreground)
                .child(label),
        )
        .when_some(line_counts, |row, (additions, deletions)| {
            row.child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(sp(12.5))
                    .font_weight(FontWeight::MEDIUM)
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
            )
        })
        .when(enabled, |row| {
            row.on_click(move |_, window, cx| action(window, cx))
                .on_key_down(move |event: &KeyDownEvent, window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        key_action(window, cx);
                        cx.stop_propagation();
                    }
                })
        })
}

fn render_background_summary_row(
    entry: BackgroundSummaryEntry,
    session_id: Uuid,
    handle: ContextMenuHandle,
    weak: WeakEntity<Raku>,
    theme: &Theme,
) -> Stateful<Div> {
    let id = entry.target.control_id();
    let group_name = SharedString::from(format!("background-summary-group-{id}"));
    let status = entry.status.render_icon(entry.kind).then(|| {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .when(entry.status.is_stoppable() && entry.can_stop, |status| {
                status.group_hover(group_name.clone(), |style| style.invisible())
            })
            .child(entry.status.icon(*theme))
    });
    let stop = (entry.status.is_stoppable() && entry.can_stop).then(|| {
        let click_target = entry.target.clone();
        let click_weak = weak.clone();
        let key_target = entry.target.clone();
        let key_weak = weak.clone();
        div()
            .id(SharedString::from(format!("background-summary-stop-{id}")))
            .track_focus(&entry.stop_focus)
            .tab_index(0)
            .size(px(24.0))
            .rounded(px(6.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .opacity(0.0)
            .group_hover(group_name.clone(), |style| style.opacity(1.0))
            .hover(|style| style.bg(theme.overlay_strong))
            .focus_visible(|style| {
                style
                    .opacity(1.0)
                    .bg(theme.raised)
                    .border_1()
                    .border_color(theme.accent)
            })
            .tooltip(Tooltip::text(tr!("background.stop")))
            .child(icon("icons/stop-filled.svg", 12.0, theme.text_tertiary))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                let _ = click_weak.update(cx, |this, cx| {
                    click_target.stop(session_id, this, cx);
                });
            })
            .on_key_down(move |event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    let _ = key_weak.update(cx, |this, cx| {
                        key_target.stop(session_id, this, cx);
                    });
                    cx.stop_propagation();
                }
            })
    });
    let trailing = (status.is_some() || stop.is_some()).then(|| {
        div()
            .relative()
            .size(px(24.0))
            .flex_none()
            .children(status)
            .children(stop)
    });
    let is_process = entry.kind != BackgroundWorkKind::Subagent;
    let open_target = entry.target.clone();
    let key_target = entry.target.clone();
    let click_handle = handle.clone();
    let click_weak = weak.clone();
    let key_handle = handle;
    let key_weak = weak;
    div()
        .id(SharedString::from(format!("background-summary-row-{id}")))
        .group(group_name)
        .track_focus(&entry.row_focus)
        .tab_index(0)
        .h(px(32.0))
        .w_full()
        .px(px(8.0))
        .rounded(px(8.0))
        .flex()
        .items_center()
        .gap(px(9.0))
        .cursor_default()
        .focus_visible(|style| style.border_1().border_color(theme.accent))
        .hover(|style| style.bg(theme.overlay_strong))
        .tooltip(Tooltip::text(entry.tooltip))
        .child(icon(work_kind_icon(entry.kind), 14.0, theme.text_secondary))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_size(px(if is_process { 12.5 } else { 13.5 }))
                .text_color(if is_process {
                    theme.text_secondary
                } else {
                    theme.text
                })
                .child(single_line_label(&entry.title)),
        )
        .when(!is_process, |row| {
            row.child(
                div()
                    .flex_none()
                    .text_size(sp(11.0))
                    .text_color(theme.text_tertiary)
                    .child(entry.status.label()),
            )
        })
        .children(trailing)
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            click_handle.close(window, cx);
            window.refresh();
            let _ = click_weak.update(cx, |this, cx| {
                open_target.open(session_id, this, window, cx);
            });
        })
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                key_handle.close(window, cx);
                window.refresh();
                let _ = key_weak.update(cx, |this, cx| {
                    key_target.open(session_id, this, window, cx);
                });
                cx.stop_propagation();
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, status: BackgroundWorkStatus, background: bool) -> BackgroundWorkItem {
        let mut item = BackgroundWorkItem::new(
            BackgroundWorkKind::Process,
            id,
            format!("process {id}"),
            status,
        );
        item.background = background;
        item
    }

    #[test]
    fn summary_keyboard_navigation_reaches_and_wraps_all_controls() {
        assert_eq!(
            background_summary_navigation_index("home", false, Some(200), 204),
            Some(0)
        );
        assert_eq!(
            background_summary_navigation_index("end", false, Some(0), 204),
            Some(203)
        );
        assert_eq!(
            background_summary_navigation_index("down", false, Some(203), 204),
            Some(0)
        );
        assert_eq!(
            background_summary_navigation_index("up", false, Some(0), 204),
            Some(203)
        );
        assert_eq!(
            background_summary_navigation_index("tab", false, Some(200), 204),
            Some(201)
        );
        assert_eq!(
            background_summary_navigation_index("tab", true, Some(201), 204),
            Some(200)
        );
        assert_eq!(
            background_summary_navigation_index("down", false, None, 204),
            Some(0)
        );
        assert_eq!(
            background_summary_navigation_index("up", false, None, 204),
            Some(203)
        );
        assert_eq!(
            background_summary_navigation_index("end", false, None, 0),
            None
        );
        assert_eq!(
            background_summary_navigation_index("enter", false, Some(0), 204),
            None
        );
    }

    #[test]
    fn settled_foreground_commands_leave_the_registry() {
        let mut registry = BackgroundWorkRegistry::default();
        registry.upsert(item("one", BackgroundWorkStatus::Running, false));
        assert!(registry.has_live());
        registry.upsert(item("one", BackgroundWorkStatus::Completed, false));
        assert!(registry.items.is_empty());
    }

    #[test]
    fn foreground_command_with_exit_code_in_output_leaves_the_registry() {
        let mut registry = BackgroundWorkRegistry::default();
        registry.upsert(item("one", BackgroundWorkStatus::Running, false));
        assert!(registry.has_live());

        let mut finished = item("one", BackgroundWorkStatus::Running, false);
        finished.output = Some("Wall time: 44.30 seconds\nCommand exited with code 101\n".into());
        registry.upsert(finished);
        assert!(registry.items.is_empty());
        assert!(!registry.has_live());
    }

    #[test]
    fn background_process_with_exit_code_in_output_settles_and_stops_spinning() {
        let mut registry = BackgroundWorkRegistry::default();
        registry.upsert(item("one", BackgroundWorkStatus::Running, true));
        assert!(registry.has_live());

        let mut finished = item("one", BackgroundWorkStatus::Running, true);
        finished.output = Some("Wall time: 44.30 seconds\nCommand exited with code 101\n".into());
        registry.upsert(finished);
        let key = BackgroundWorkKey::new(BackgroundWorkKind::Process, "one");
        assert_eq!(registry.items[&key].status, BackgroundWorkStatus::Failed);
        assert_eq!(registry.items[&key].exit_code, Some(101));
        assert!(!registry.has_live());
        assert_eq!(registry.counts(), (0, 0));
    }

    #[test]
    fn append_output_with_exit_code_settles_foreground_and_background_items() {
        let mut registry = BackgroundWorkRegistry::default();
        let fg_key = BackgroundWorkKey::new(BackgroundWorkKind::Process, "fg");
        let bg_key = BackgroundWorkKey::new(BackgroundWorkKind::Process, "bg");
        registry.upsert(item("fg", BackgroundWorkStatus::Running, false));
        registry.upsert(item("bg", BackgroundWorkStatus::Running, true));

        registry.apply(BackgroundWorkEvent::OutputDelta {
            key: fg_key.clone(),
            delta: "Command exited with code 0\n".into(),
        });
        assert!(!registry.items.contains_key(&fg_key));

        registry.apply(BackgroundWorkEvent::OutputDelta {
            key: bg_key.clone(),
            delta: "Command exited with code 1\n".into(),
        });
        assert_eq!(registry.items[&bg_key].status, BackgroundWorkStatus::Failed);
        assert_eq!(registry.items[&bg_key].exit_code, Some(1));
        assert!(!registry.has_live());
    }

    #[test]
    fn live_reconciliation_and_non_process_outputs_are_not_overridden_by_stale_exit_lines() {
        let mut registry = BackgroundWorkRegistry::default();
        let proc_key = BackgroundWorkKey::new(BackgroundWorkKind::Process, "pty-1");
        let mut pty = item("pty-1", BackgroundWorkStatus::Running, true);
        pty.can_stop = true;
        pty.output = Some("subcommand failed\nExit code: 1\n".into());
        registry.upsert(pty);
        assert_eq!(
            registry.items[&proc_key].status,
            BackgroundWorkStatus::Failed
        );

        // Authoritative process-table poll (`output: None`) reporting the
        // background terminal still running must not be clobbered by old output.
        let mut live_poll = item("pty-1", BackgroundWorkStatus::Running, true);
        live_poll.can_stop = true;
        registry.reconcile_processes(vec![live_poll]);
        assert_eq!(
            registry.items[&proc_key].status,
            BackgroundWorkStatus::Running
        );
        assert!(registry.items[&proc_key].can_stop);

        let agent_key = BackgroundWorkKey::new(BackgroundWorkKind::Subagent, "agent-1");
        let mut subagent = BackgroundWorkItem::new(
            BackgroundWorkKind::Subagent,
            "agent-1",
            "Investigate failure",
            BackgroundWorkStatus::Running,
        );
        subagent.background = true;
        subagent.can_stop = true;
        registry.upsert(subagent);
        registry.apply(BackgroundWorkEvent::OutputDelta {
            key: agent_key.clone(),
            delta: "The test printed:\nCommand exited with code 101\n".into(),
        });
        assert_eq!(
            registry.items[&agent_key].status,
            BackgroundWorkStatus::Running
        );
        assert!(registry.items[&agent_key].can_stop);
    }

    #[test]
    fn reconciliation_marks_disappeared_background_process_lost() {
        let mut registry = BackgroundWorkRegistry::default();
        registry.upsert(item("one", BackgroundWorkStatus::Running, true));
        registry.reconcile_processes(Vec::new());
        assert_eq!(
            registry.items[&BackgroundWorkKey::new(BackgroundWorkKind::Process, "one")].status,
            BackgroundWorkStatus::Lost
        );
    }

    #[test]
    fn polling_does_not_reopen_a_pending_stop() {
        let mut registry = BackgroundWorkRegistry::default();
        let key = BackgroundWorkKey::new(BackgroundWorkKind::Process, "one");
        registry.upsert(item("one", BackgroundWorkStatus::Running, true));
        registry.apply(BackgroundWorkEvent::StopRequested(key.clone()));
        registry.upsert(item("one", BackgroundWorkStatus::Running, true));
        assert_eq!(registry.items[&key].status, BackgroundWorkStatus::Stopping);
        assert!(!registry.items[&key].status.is_stoppable());
    }

    #[test]
    fn output_is_bounded_on_utf8_boundaries() {
        let mut registry = BackgroundWorkRegistry::default();
        registry.upsert(item("one", BackgroundWorkStatus::Running, true));
        registry.append_output(
            &BackgroundWorkKey::new(BackgroundWorkKind::Process, "one"),
            &"界".repeat(MAX_BACKGROUND_OUTPUT_BYTES),
        );
        let output = registry.items.values().next().unwrap();
        assert!(output.output.as_ref().unwrap().len() <= MAX_BACKGROUND_OUTPUT_BYTES);
        assert!(output.output_truncated);
    }

    #[test]
    fn output_cache_strips_split_ansi_sequences() {
        let mut registry = BackgroundWorkRegistry::default();
        let key = BackgroundWorkKey::new(BackgroundWorkKind::Process, "one");
        registry.upsert(item("one", BackgroundWorkStatus::Running, true));
        registry.append_output(&key, "\u{1b}");
        registry.append_output(&key, "[31mred\u{1b}[0m");
        assert!(registry.refresh_output_cache());
        assert_eq!(registry.rendered_output[&key].as_ref(), "red");
    }

    #[test]
    fn output_cache_requests_a_retry_only_while_dirty() {
        let mut registry = BackgroundWorkRegistry::default();
        let key = BackgroundWorkKey::new(BackgroundWorkKind::Process, "one");
        registry.upsert(item("one", BackgroundWorkStatus::Running, true));
        registry.append_output(&key, "first");
        assert_eq!(registry.output_refresh_delay(), Some(Duration::ZERO));
        assert!(registry.refresh_output_cache());
        assert_eq!(registry.output_refresh_delay(), None);

        registry.append_output(&key, " second");
        let delay = registry
            .output_refresh_delay()
            .expect("new output should request one cache refresh");
        assert!(delay <= OUTPUT_CACHE_REFRESH_INTERVAL);
        assert!(!registry.refresh_output_cache());
    }

    #[test]
    fn unchanged_process_snapshots_do_not_rebuild_output() {
        let mut registry = BackgroundWorkRegistry::default();
        let mut process = item("one", BackgroundWorkStatus::Running, true);
        process.output = Some("same output".to_owned());
        registry.upsert(process.clone());
        assert!(registry.refresh_output_cache());

        registry.upsert(process);
        assert_eq!(registry.output_refresh_delay(), None);
    }

    #[test]
    fn turn_settlement_keeps_detached_work_live() {
        let mut registry = BackgroundWorkRegistry::default();
        registry.upsert(item("foreground", BackgroundWorkStatus::Running, false));
        registry.upsert(item("background", BackgroundWorkStatus::Running, true));
        registry.settle_foreground(BackgroundWorkStatus::Completed);
        assert!(!registry.items.contains_key(&BackgroundWorkKey::new(
            BackgroundWorkKind::Process,
            "foreground"
        )));
        assert_eq!(
            registry.items[&BackgroundWorkKey::new(BackgroundWorkKind::Process, "background")]
                .status,
            BackgroundWorkStatus::Running
        );
    }
}
