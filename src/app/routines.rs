//! Scheduled session routines: silent background checks that run in an
//! isolated read-only context and post to their session only when new
//! progress is found.

use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use chrono::{Local, TimeZone as _};
use parking_lot::Mutex;
use serde::Deserialize;

use super::sidebar::localized_session_title;
use super::*;
use crate::model::{
    DriverEvent, Message, MessageRole, ProviderKind, ROUTINE_MAX_CONSECUTIVE_FAILURES,
    RoutineRunOutcome, RoutineSchedule, SessionRoutine, SubagentDefinition,
};

/// How often the in-app scheduler wakes to check for due routines.
const ROUTINE_SCHEDULER_TICK_INTERVAL: Duration = Duration::from_secs(15);
/// Hard upper bound for a single isolated routine check.
const ROUTINE_CHECK_TIMEOUT: Duration = Duration::from_secs(180);
/// Maximum simultaneous routine probes running across all sessions.
const MAX_CONCURRENT_ROUTINE_CHECKS: usize = 2;
/// Deferral window when a session is actively streaming a user turn.
const ROUTINE_BUSY_DEFER_SECS: u64 = 60;
/// Cap on unseen routine context prepended to a follow-up user prompt.
const ROUTINE_CONTEXT_MAX_CHARS: usize = 8000;
/// Failed transcript loads before a found update is recorded as a failed
/// check instead of being retried.
const ROUTINE_DELIVERY_MAX_HYDRATION_ATTEMPTS: u32 = 3;

/// Routine probe bookkeeping owned by the UI entity.
#[derive(Default)]
pub(super) struct RoutineChecks {
    in_flight: HashMap<Uuid, InFlightRoutineCheck>,
    /// Updates found by a finished probe that their session cannot take yet.
    pending: HashMap<Uuid, PendingRoutineUpdate>,
    /// Sessions whose transcript is being fetched to take a pending update.
    hydrating: HashSet<Uuid>,
    next_generation: u64,
}

/// One running probe. `generation` ties its outcome to the routine version
/// that started it; `cancel` and `driver` let removal, edits, and quit stop it.
struct InFlightRoutineCheck {
    generation: u64,
    cancel: Arc<AtomicBool>,
    driver: Arc<Mutex<Option<DriverHandle>>>,
}

impl InFlightRoutineCheck {
    fn stop(&self) {
        self.cancel.store(true, AtomicOrdering::Release);
    }

    /// Closes the probe's daemon runtime from this thread. Only for quit,
    /// when the probe thread may never run again.
    fn close_now(&self) {
        self.stop();
        if let Some(driver) = self.driver.lock().take() {
            driver.cancel();
            driver.close();
        }
    }
}

struct PendingRoutineUpdate {
    checked_at: u64,
    duration_ms: u64,
    manual: bool,
    probe: RoutineProbeResult,
    hydrated_session: Option<AgentSession>,
    hydration_attempts: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RoutineProbeResult {
    has_update: bool,
    summary: Option<String>,
    message: Option<String>,
    snapshot: Option<String>,
}

struct RoutineCheckOutcome {
    session_id: Uuid,
    generation: u64,
    manual: bool,
    checked_at: u64,
    duration_ms: u64,
    hydrated_session: Option<AgentSession>,
    result: anyhow::Result<RoutineProbeResult>,
}

#[derive(Debug, Deserialize)]
struct RawRoutineProbeJson {
    has_update: bool,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    snapshot: Option<String>,
}

/// Human-readable label for a routine schedule in the active locale.
pub(super) fn routine_schedule_label(schedule: RoutineSchedule) -> String {
    match schedule {
        RoutineSchedule::EveryMinutes { minutes } if minutes >= 60 && minutes % 60 == 0 => {
            tr!("routine.every_hours", count = minutes / 60)
        }
        RoutineSchedule::EveryMinutes { minutes } => {
            tr!("routine.every_minutes", count = minutes)
        }
        RoutineSchedule::Daily { hour, minute } => {
            let time = format!("{hour:02}:{minute:02}");
            tr!("routine.daily_at", time = time)
        }
    }
}

/// Formats a unix timestamp concisely for routine pills and status rows.
pub(super) fn format_routine_timestamp(unix_secs: u64) -> String {
    let Some(dt) = Local.timestamp_opt(unix_secs as i64, 0).single() else {
        return String::new();
    };
    let now = Local::now();
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else if crate::i18n::uses_east_asian_date_format() {
        dt.format("%m/%d %H:%M").to_string()
    } else {
        dt.format("%b %-d, %H:%M").to_string()
    }
}

/// What one check runs on: the task's own provider and model, or a chosen
/// subagent definition's.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RoutineRunnerConfig {
    provider: ProviderKind,
    /// `None` leaves the choice to the provider's preferred model.
    model: Option<String>,
    reasoning_effort: Option<String>,
    /// Running the task's exact model, so its tier and context window apply.
    on_task_model: bool,
    /// The runner's extra instructions, empty without one.
    instructions: String,
}

/// Resolves a runner the way a delegated subagent is launched: the agent's
/// provider, else the task's; its model, else the task's when the provider
/// is the same; its effort, else the task's when it also kept the task's
/// model.
fn resolve_routine_runner(
    task_provider: ProviderKind,
    task_model: Option<String>,
    task_effort: Option<String>,
    runner: Option<&SubagentDefinition>,
) -> RoutineRunnerConfig {
    let Some(agent) = runner else {
        return RoutineRunnerConfig {
            provider: task_provider,
            model: task_model,
            reasoning_effort: task_effort,
            on_task_model: true,
            instructions: String::new(),
        };
    };
    let provider = agent.provider.unwrap_or(task_provider);
    let on_task_model = provider == task_provider && agent.model.is_none();
    RoutineRunnerConfig {
        provider,
        model: agent
            .model
            .clone()
            .or_else(|| (provider == task_provider).then_some(task_model).flatten()),
        reasoning_effort: agent
            .reasoning_effort
            .clone()
            .or_else(|| on_task_model.then_some(task_effort).flatten()),
        on_task_model,
        instructions: agent.instructions.trim().to_owned(),
    }
}

/// Builds the isolated probe prompt sent to the read-only routine runner.
pub(super) fn build_routine_probe_prompt(
    runner_instructions: &str,
    reply_language: &str,
    goal: &str,
    last_snapshot: Option<&str>,
    last_checked_at: Option<u64>,
    now_secs: u64,
) -> String {
    let now_label = Local
        .timestamp_opt(now_secs as i64, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M %Z").to_string())
        .unwrap_or_else(|| now_secs.to_string());
    let mut prompt = String::new();
    prompt.push_str(
        "You are running a scheduled background routine check inside Raku.\n\
         Use your available read-only or web tools to fetch fresh, current information for the monitoring target below.\n\
         Compare what you find against the previous baseline snapshot (if provided) and decide whether there is genuinely new, meaningful progress worth notifying the user about.\n\n",
    );
    if !runner_instructions.is_empty() {
        prompt.push_str("<agent_instructions>\n");
        prompt.push_str(runner_instructions);
        prompt.push_str("\n</agent_instructions>\n\n");
    }
    prompt.push_str(&format!("Current local time: {now_label}\n"));
    if let Some(checked_at) = last_checked_at
        && let Some(dt) = Local.timestamp_opt(checked_at as i64, 0).single()
    {
        prompt.push_str(&format!(
            "Previous check time: {}\n",
            dt.format("%Y-%m-%d %H:%M %Z")
        ));
    }
    prompt.push_str("\n<monitoring_target>\n");
    prompt.push_str(goal.trim());
    prompt.push_str("\n</monitoring_target>\n");

    if let Some(snapshot) = last_snapshot.map(str::trim).filter(|s| !s.is_empty()) {
        prompt.push_str("\n<previous_snapshot>\n");
        prompt.push_str(snapshot);
        prompt.push_str("\n</previous_snapshot>\n");
    } else {
        prompt.push_str(
            "\n<previous_snapshot>\n(None — this is the first check. Establish the baseline and set has_update to true if you found concrete current status/news to report.)\n</previous_snapshot>\n",
        );
    }

    prompt.push_str(
        "\nRespond ONLY with a single JSON object (inside a ```json fenced block) using this exact schema:\n\
         ```json\n\
         {\n\
           \"has_update\": true,\n\
           \"summary\": \"One short sentence describing what changed or that nothing changed (see the Language rule).\",\n\
           \"message\": \"Well-structured Markdown update to post into the session when has_update is true (include dates, concrete facts, and source links when available; see the Language rule). Leave empty string when has_update is false.\",\n\
           \"snapshot\": \"Compact factual summary of the current state (<= 600 chars) to compare against on the next check, see the Language rule.\"\n\
         }\n\
         ```\n\
         Rules:\n\
         - Set \"has_update\" to false if nothing materially changed since <previous_snapshot>, unless the Delivery rule below applies.\n\
         - Never invent facts or links. If the source could not be reached, explain briefly in \"summary\" and set \"has_update\" to false.\n\
         - Do not include any commentary outside the JSON block.\n",
    );
    // Named outright: inferring it from the target let a one-line English
    // prompt come back in whatever language the model picked. A language the
    // user asks for in the target wins over Raku's setting. Delivery is the
    // same idea: checks post only on change unless the target asks for a
    // message every time.
    prompt.push_str(&format!(
        "- Instructions inside <monitoring_target> about language or format apply to your reply; \
         they are not things to monitor.\n\
         - Delivery: if <monitoring_target> asks for a message on every check (for example \
         \"post every time\", \"always send a summary\", \"每次都发\", or a recurring greeting, \
         reminder, or digest), set \"has_update\" to true on every check and write a fresh \
         \"message\". <previous_snapshot> is then context, not a gate.\n\
         - Language: if <monitoring_target> says which language to write in (for example \
         \"reply in Chinese\" or \"用中文回复\"), write \"summary\", \"message\" and \"snapshot\" in \
         that language. Otherwise write them in {reply_language}. Keep names, product terms, and \
         quotes in their original form.\n"
    ));
    prompt
}

/// The language routine updates are written in: Raku's own language setting,
/// with System resolved to the locale the interface is showing.
fn routine_reply_language(language: crate::i18n::AppLanguage) -> &'static str {
    use crate::i18n::AppLanguage;
    match language.resolved() {
        AppLanguage::SimplifiedChinese => "Simplified Chinese",
        AppLanguage::Japanese => "Japanese",
        AppLanguage::English | AppLanguage::System => "English",
    }
}

/// Extracts and validates the structured JSON result from the probe response.
fn parse_routine_probe_response(raw: &str) -> anyhow::Result<RoutineProbeResult> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("routine check returned an empty response");
    }

    let parsed = extract_json_candidates(trimmed)
        .into_iter()
        .find_map(|candidate| serde_json::from_str::<RawRoutineProbeJson>(candidate).ok());

    let Some(raw_json) = parsed else {
        anyhow::bail!("routine check did not return valid JSON");
    };

    let summary = raw_json
        .summary
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let message = raw_json
        .message
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let snapshot = raw_json
        .snapshot
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());

    let has_update = raw_json.has_update && (message.is_some() || summary.is_some());
    let message = if has_update {
        message.or_else(|| summary.clone())
    } else {
        None
    };

    Ok(RoutineProbeResult {
        has_update,
        summary,
        message,
        snapshot,
    })
}

fn extract_json_candidates(text: &str) -> Vec<&str> {
    let mut candidates = Vec::new();

    // 1. Prefer fenced ```json ... ``` or ``` ... ``` blocks (search from last block first
    //    in case the model quoted an example earlier).
    let mut fenced = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after_ticks = &rest[start + 3..];
        let Some(newline) = after_ticks.find('\n') else {
            break;
        };
        let body = &after_ticks[newline + 1..];
        let Some(end) = body.find("```") else {
            break;
        };
        let inner = body[..end].trim();
        if inner.starts_with('{') && inner.ends_with('}') {
            fenced.push(inner);
        }
        rest = &body[end + 3..];
    }
    for block in fenced.into_iter().rev() {
        candidates.push(block);
    }

    // 2. Try the whole trimmed text directly.
    if text.starts_with('{') && text.ends_with('}') {
        candidates.push(text);
    }

    // 3. Try outermost `{ ... }` span.
    if let (Some(first), Some(last)) = (text.find('{'), text.rfind('}'))
        && first < last
    {
        candidates.push(&text[first..=last]);
    }

    candidates
}

/// If routine updates were appended to `session` after its most recent real
/// provider turn, prepend them to the provider-facing prompt while keeping
/// `display_content` as the user's clean visible message.
pub(super) fn attach_unseen_routine_updates(
    session: &AgentSession,
    mut submission: ComposerSubmission,
) -> ComposerSubmission {
    // A command must stay the first word on the wire: templates expand only
    // from a token at offset 0, and Claude's CLI only treats a message that
    // starts with `/` as a command. Prepending would send it as plain text.
    if submission
        .display_content
        .as_deref()
        .unwrap_or(&submission.prompt)
        .trim_start()
        .starts_with('/')
    {
        return submission;
    }
    let last_turn_end = session
        .turns
        .last()
        .map(|turn| turn.completed_at.unwrap_or(turn.started_at))
        .unwrap_or(0);

    let mut updates = Vec::new();
    for message in session.messages.iter().rev() {
        if message.turn_id.is_some() {
            break;
        }
        if message.role == MessageRole::Assistant
            && message.created_at >= last_turn_end
            && !message.content.trim().is_empty()
        {
            updates.push((message.created_at, message.content.trim()));
        }
    }
    if updates.is_empty() {
        return submission;
    }
    updates.reverse();

    let mut context = String::from(
        "<routine_updates>\nThe following scheduled routine updates were delivered to this session since your last reply:\n",
    );
    for (ts, body) in updates {
        let time_label = format_routine_timestamp(ts);
        context.push_str(&format!("\n--- Update ({time_label}) ---\n"));
        context.push_str(body);
        context.push('\n');
        if context.len() >= ROUTINE_CONTEXT_MAX_CHARS {
            break;
        }
    }
    if context.chars().count() > ROUTINE_CONTEXT_MAX_CHARS {
        context = context
            .chars()
            .take(ROUTINE_CONTEXT_MAX_CHARS)
            .collect::<String>();
        context.push_str("\n…\n");
    }
    context.push_str("</routine_updates>\n\n");

    let visible = submission
        .display_content
        .clone()
        .unwrap_or_else(|| submission.prompt.clone());
    submission.display_content = Some(visible);
    submission.prompt = format!("{context}{}", submission.prompt);
    submission
}

#[allow(clippy::too_many_arguments)]
fn execute_routine_probe(
    daemon: raku_client::DaemonSupervisor,
    target_session_id: Uuid,
    generation: u64,
    needs_hydration: bool,
    manual: bool,
    provider: ProviderKind,
    options: DriverStartOptions,
    probe_prompt: String,
    cancel: Arc<AtomicBool>,
    driver_slot: Arc<Mutex<Option<DriverHandle>>>,
) -> RoutineCheckOutcome {
    let started = Instant::now();
    let probe_session_id = Uuid::new_v4();
    let (wake_tx, _wake_rx) = smol::channel::bounded(1);
    let (event_tx, event_rx) = driver::event_channel(wake_tx);

    let run = || -> anyhow::Result<RoutineProbeResult> {
        let handle = driver::start_remote(
            daemon.clone(),
            probe_session_id,
            provider,
            options,
            event_tx,
        )?;
        // Published so cancellation and app quit can close the runtime: the
        // daemon outlives this process and would keep an abandoned probe's
        // provider running.
        *driver_slot.lock() = Some(handle.clone());
        let stop = |handle: &DriverHandle| {
            driver_slot.lock().take();
            handle.cancel();
            handle.close();
        };
        if cancel.load(AtomicOrdering::Acquire) {
            stop(&handle);
            anyhow::bail!("routine check cancelled");
        }
        handle.prompt(probe_prompt);

        let deadline = Instant::now() + ROUTINE_CHECK_TIMEOUT;
        let mut response_text = String::new();
        let mut last_error: Option<String> = None;
        let mut finished = false;

        loop {
            if cancel.load(AtomicOrdering::Acquire) {
                stop(&handle);
                anyhow::bail!("routine check cancelled");
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                stop(&handle);
                match last_error {
                    Some(error) => anyhow::bail!("routine check timed out: {error}"),
                    None => anyhow::bail!("routine check timed out"),
                }
            };
            match event_rx.recv_timeout(remaining.min(Duration::from_millis(500))) {
                Ok(DriverEvent::TextDelta(delta)) => response_text.push_str(&delta),
                Ok(DriverEvent::TurnFinished { success, summary }) => {
                    finished = true;
                    if !success {
                        last_error = summary.or(last_error).or_else(|| {
                            Some("provider ended routine check unsuccessfully".to_owned())
                        });
                    }
                    break;
                }
                Ok(DriverEvent::Error(error)) => {
                    last_error = Some(error);
                }
                Ok(DriverEvent::ProcessExited) => {
                    break;
                }
                Ok(DriverEvent::Permission {
                    request_id,
                    options,
                    ..
                }) => {
                    if let Some(reject) = options.iter().find(|opt| {
                        let lower = opt.label.to_lowercase();
                        lower.contains("deny")
                            || lower.contains("reject")
                            || lower.contains("cancel")
                    }) {
                        handle.respond(request_id, reject.id.clone());
                    } else {
                        stop(&handle);
                        anyhow::bail!("routine check requested interactive permission");
                    }
                }
                Ok(DriverEvent::UserInputRequested { .. })
                | Ok(DriverEvent::PlanProposed { .. }) => {
                    stop(&handle);
                    anyhow::bail!("routine check requested interactive user input");
                }
                Ok(_) => {}
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }

        driver_slot.lock().take();
        handle.close();

        if !finished && response_text.trim().is_empty() {
            anyhow::bail!(
                "{}",
                last_error.unwrap_or_else(|| "routine check exited before completing".to_owned())
            );
        }
        if let Some(error) = last_error
            && response_text.trim().is_empty()
        {
            anyhow::bail!("{error}");
        }

        parse_routine_probe_response(&response_text)
    };

    let result = run();
    // Fetched here, off the UI thread, so a skeleton can usually take the
    // update on arrival. Delivery falls back to its own fetch when this one
    // fails or goes stale.
    let hydrated_session = if result.as_ref().is_ok_and(|res| res.has_update)
        && needs_hydration
        && !cancel.load(AtomicOrdering::Acquire)
    {
        raku_client::persistence::hydrate_session(&daemon, target_session_id)
            .ok()
            .flatten()
    } else {
        None
    };

    RoutineCheckOutcome {
        session_id: target_session_id,
        generation,
        manual,
        checked_at: unix_time(),
        duration_ms: started.elapsed().as_millis() as u64,
        hydrated_session,
        result,
    }
}

impl Raku {
    /// Starts the in-app scheduler that checks for due session routines.
    pub(super) fn start_routine_scheduler(&mut self, cx: &mut Context<Self>) {
        // Probes run as daemon runtimes, which outlive this process. Close
        // them on quit so an interrupted check does not leave its provider
        // running with no client to ever close it.
        cx.on_app_quit(|this, _| {
            for check in this.routine_checks.in_flight.values() {
                check.close_now();
            }
            async {}
        })
        .detach();
        self.routine_scheduler_task = Some(cx.spawn(async move |this, cx| {
            // Let initial provider probing finish before kicking off overdue runs.
            cx.background_executor().timer(Duration::from_secs(5)).await;
            loop {
                let alive = this
                    .update(cx, |raku, cx| {
                        raku.tick_routine_scheduler(cx);
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                cx.background_executor()
                    .timer(ROUTINE_SCHEDULER_TICK_INTERVAL)
                    .await;
            }
        }));
    }

    pub(super) fn is_routine_running(&self, session_id: Uuid) -> bool {
        self.routine_checks.in_flight.contains_key(&session_id)
    }

    /// Stops any probe for `session_id` and drops an update it found but has
    /// not delivered. Called when the routine is removed or retargeted, or
    /// its session is deleted, so a superseded result can never land.
    pub(super) fn cancel_routine_check(&mut self, session_id: Uuid) {
        if let Some(check) = self.routine_checks.in_flight.remove(&session_id) {
            // The probe notices within one poll and closes its own runtime;
            // closing here would put a daemon round trip on the UI thread.
            check.stop();
        }
        self.routine_checks.pending.remove(&session_id);
    }

    pub(super) fn tick_routine_scheduler(&mut self, cx: &mut Context<Self>) {
        // Updates waiting for an idle, loaded session go first.
        let pending = self
            .routine_checks
            .pending
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for session_id in pending {
            self.try_deliver_routine_update(session_id, cx);
        }

        if self.routine_checks.in_flight.len() >= MAX_CONCURRENT_ROUTINE_CHECKS {
            return;
        }
        let now = unix_time();
        let mut due_ids = Vec::new();
        let mut deferred_any = false;

        for session in &mut self.state.sessions {
            if session.is_child() {
                continue;
            }
            let session_id = session.id;
            let busy = session.is_busy();
            let Some(routine) = session.routine.as_mut() else {
                continue;
            };
            if !routine.enabled {
                continue;
            }
            let Some(next_run) = routine.next_run_at else {
                routine.next_run_at = Some(routine.schedule.next_run_after(now));
                continue;
            };
            if now < next_run {
                continue;
            }
            if self.routine_checks.in_flight.contains_key(&session_id)
                || self.routine_checks.pending.contains_key(&session_id)
            {
                continue;
            }
            if busy {
                routine.next_run_at = Some(now.saturating_add(ROUTINE_BUSY_DEFER_SECS));
                deferred_any = true;
                continue;
            }
            due_ids.push(session_id);
        }

        // Only probes that actually start take a slot. A routine that cannot
        // start (its provider is not detected yet, say) must not hold one, or
        // it would starve every routine after it on every tick.
        for session_id in due_ids {
            if self.routine_checks.in_flight.len() >= MAX_CONCURRENT_ROUTINE_CHECKS {
                break;
            }
            self.start_routine_check(session_id, false, cx);
        }
        if deferred_any {
            cx.notify();
        }
    }

    pub(super) fn trigger_session_routine_now(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        if self.routine_checks.in_flight.contains_key(&session_id)
            || self.routine_checks.pending.contains_key(&session_id)
        {
            return;
        }
        if self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .is_some_and(AgentSession::is_busy)
        {
            self.show_info_toast(tr!("routine.outcome_skipped_busy"));
            cx.notify();
            return;
        }
        self.start_routine_check(session_id, true, cx);
    }

    pub(super) fn save_session_routine(
        &mut self,
        session_id: Uuid,
        prompt: String,
        schedule: RoutineSchedule,
        runner: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let prompt = prompt.trim().to_owned();
        if prompt.is_empty() {
            return;
        }
        let now = unix_time();
        let Some(session) = self.state.session_mut(session_id) else {
            return;
        };
        if session.is_child() {
            return;
        }
        let mut prompt_changed = false;
        if let Some(existing) = session.routine.as_mut() {
            prompt_changed = existing.prompt.trim() != prompt;
            existing.prompt = prompt.clone();
            existing.schedule = schedule;
            existing.runner = runner;
            existing.enabled = true;
            existing.updated_at = now;
            existing.consecutive_failures = 0;
            existing.last_error = None;
            existing.next_run_at = Some(schedule.next_run_after(now));
            if prompt_changed {
                existing.last_snapshot = None;
            }
        } else {
            let mut routine = SessionRoutine::new(prompt.clone(), schedule, now);
            routine.runner = runner;
            session.routine = Some(routine);
        }
        session.set_title_from_prompt(&prompt);
        session.updated_at = now;
        if prompt_changed {
            // A probe of the old target would otherwise store its snapshot as
            // the new target's baseline.
            self.cancel_routine_check(session_id);
        }

        self.save();
        self.show_success_toast(tr!("routine.toast_saved"));
        cx.notify();
    }

    pub(super) fn set_session_routine_enabled(
        &mut self,
        session_id: Uuid,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let now = unix_time();
        let Some(session) = self.state.session_mut(session_id) else {
            return;
        };
        let Some(routine) = session.routine.as_mut() else {
            return;
        };
        routine.set_enabled(enabled, now);
        session.updated_at = now;

        self.save();
        self.show_info_toast(if enabled {
            tr!("routine.toast_resumed")
        } else {
            tr!("routine.toast_paused")
        });
        cx.notify();
    }

    pub(super) fn remove_session_routine(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let now = unix_time();
        let Some(session) = self.state.session_mut(session_id) else {
            return;
        };
        if session.routine.take().is_none() {
            return;
        }
        session.updated_at = now;
        self.cancel_routine_check(session_id);

        self.save();
        self.show_info_toast(tr!("routine.toast_removed"));
        cx.notify();
    }

    /// Starts one probe and reports whether it is now running, so the
    /// scheduler counts only real starts against its concurrency limit.
    fn start_routine_check(
        &mut self,
        session_id: Uuid,
        manual: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(runner_name) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| session.routine.as_ref())
            .map(|routine| routine.runner.clone())
        else {
            return false;
        };
        let runner = match runner_name {
            None => None,
            Some(name) => match self
                .state
                .subagents
                .iter()
                .find(|agent| agent.enabled && agent.name == name)
            {
                Some(agent) => Some(agent.clone()),
                None => {
                    // Never fall back to the task's model: the runner was
                    // usually chosen to keep checks cheap.
                    let error = tr!("routine.runner_missing", name = name);
                    self.record_routine_failure(session_id, 0, error, manual, cx);
                    return false;
                }
            },
        };

        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return false;
        };
        let Some(routine) = session.routine.as_ref() else {
            return false;
        };
        if session.is_child() || routine.prompt.trim().is_empty() {
            return false;
        }

        let task_options = self.session_options(session);
        let (task_model, task_effort) = self.turn_model_options(session);
        let RoutineRunnerConfig {
            provider,
            model,
            reasoning_effort,
            on_task_model,
            instructions,
        } = resolve_routine_runner(session.provider, task_model, task_effort, runner.as_ref());
        // A runner on another provider without its own model gets that
        // provider's preferred model, as a delegated subagent would.
        let model = model.or_else(|| {
            self.provider_probe(provider)
                .and_then(ProviderProbe::preferred_model)
                .map(|model| model.id.clone())
        });
        let binary = self
            .probes
            .iter()
            .find(|probe| probe.provider == provider)
            .and_then(|probe| probe.path.clone());
        let Some(binary) = binary else {
            let error = tr!(
                "errors.provider_not_found",
                provider = provider.display_name()
            );
            if manual {
                self.show_toast(error);
                cx.notify();
            } else if self.provider_detection_remaining == 0
                && self.provider_detection_checked_at.is_some()
            {
                // Detection finished and found no binary. Record it like any
                // failed check: the schedule advances, the error shows, and
                // repeated misses pause the routine instead of retrying it
                // silently forever. Before detection finishes the routine just
                // stays due.
                self.record_routine_failure(session_id, 0, error, false, cx);
            }
            return false;
        };

        let cwd = self
            .workspace_path_for_session(session)
            .map(std::path::Path::to_path_buf)
            .or_else(crate::projectless::workspace_root)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let options = DriverStartOptions {
            binary,
            cwd,
            mode: side_chat::side_chat_runtime_mode(provider),
            plan_mode: false,
            model,
            reasoning_effort,
            // Tier and context window are choices for the task's model; a
            // runner on another model keeps that model's defaults.
            service_tier: on_task_model.then_some(task_options.service_tier).flatten(),
            context_window: on_task_model
                .then_some(task_options.context_window)
                .flatten(),
            agent_preset: None,
            provider_cursor: None,
            // Always, even for an agent with write access: nobody watches an
            // unattended check.
            read_only: true,
            subagent: Default::default(),
        };
        let now = unix_time();
        let probe_prompt = build_routine_probe_prompt(
            &instructions,
            routine_reply_language(self.state.language),
            &routine.prompt,
            routine.last_snapshot.as_deref(),
            routine.last_checked_at,
            now,
        );
        let needs_hydration = !session.detail_loaded;
        let daemon = self.daemon.clone();

        // Advance `next_run_at` immediately so sleep/missed-run bursts coalesce
        // into a single execution and won't re-trigger while in flight.
        if let Some(session_mut) = self.state.session_mut(session_id)
            && let Some(routine_mut) = session_mut.routine.as_mut()
            && routine_mut.enabled
        {
            routine_mut.next_run_at = Some(routine_mut.schedule.next_run_after(now));
        }

        self.routine_checks.next_generation += 1;
        let generation = self.routine_checks.next_generation;
        let cancel = Arc::new(AtomicBool::new(false));
        let driver_slot = Arc::new(Mutex::new(None));
        self.routine_checks.in_flight.insert(
            session_id,
            InFlightRoutineCheck {
                generation,
                cancel: cancel.clone(),
                driver: driver_slot.clone(),
            },
        );
        // No "checking" toast: the header and sidebar spinners show it.
        cx.notify();

        let background = self.background_executor.clone();
        cx.spawn(async move |this, cx| {
            let outcome = background
                .spawn(async move {
                    execute_routine_probe(
                        daemon,
                        session_id,
                        generation,
                        needs_hydration,
                        manual,
                        provider,
                        options,
                        probe_prompt,
                        cancel,
                        driver_slot,
                    )
                })
                .await;
            let _ = this.update(cx, |raku, cx| {
                raku.apply_routine_check_outcome(outcome, cx);
            });
        })
        .detach();
        true
    }

    fn apply_routine_check_outcome(
        &mut self,
        outcome: RoutineCheckOutcome,
        cx: &mut Context<Self>,
    ) {
        let RoutineCheckOutcome {
            session_id,
            generation,
            manual,
            checked_at,
            duration_ms,
            hydrated_session,
            result,
        } = outcome;

        // Only the probe this routine version started may land. A removed,
        // retargeted, or replaced routine has dropped or bumped its entry.
        if self
            .routine_checks
            .in_flight
            .get(&session_id)
            .is_none_or(|check| check.generation != generation)
        {
            return;
        }
        self.routine_checks.in_flight.remove(&session_id);

        let probe = match result {
            Ok(probe) if probe.has_update => {
                // Nothing is recorded yet: the snapshot advances only once the
                // update is actually in the transcript.
                self.routine_checks.pending.insert(
                    session_id,
                    PendingRoutineUpdate {
                        checked_at,
                        duration_ms,
                        manual,
                        probe,
                        hydrated_session,
                        hydration_attempts: 0,
                    },
                );
                self.try_deliver_routine_update(session_id, cx);
                cx.notify();
                return;
            }
            Ok(probe) => probe,
            Err(error) => {
                self.record_routine_failure(session_id, duration_ms, error.to_string(), manual, cx);
                return;
            }
        };

        let Some(session) = self.state.session_mut(session_id) else {
            cx.notify();
            return;
        };
        let Some(routine) = session.routine.as_mut() else {
            cx.notify();
            return;
        };
        routine.record_run(
            checked_at,
            duration_ms,
            RoutineRunOutcome::NoUpdate,
            probe.summary,
            probe.snapshot,
            None,
        );
        session.updated_at = checked_at;
        self.save();
        if manual {
            self.show_info_toast(tr!("routine.toast_no_update"));
        }
        cx.notify();
    }

    /// Records a failed check, which advances the schedule and pauses the
    /// routine after repeated failures.
    fn record_routine_failure(
        &mut self,
        session_id: Uuid,
        duration_ms: u64,
        message: String,
        manual: bool,
        cx: &mut Context<Self>,
    ) {
        let now = unix_time();
        let Some(session) = self.state.session_mut(session_id) else {
            return;
        };
        let Some(routine) = session.routine.as_mut() else {
            return;
        };
        let was_enabled = routine.enabled;
        routine.record_run(
            now,
            duration_ms,
            RoutineRunOutcome::Failed,
            Some(message.clone()),
            None,
            Some(message.clone()),
        );
        let auto_paused = was_enabled
            && !routine.enabled
            && routine.consecutive_failures >= ROUTINE_MAX_CONSECUTIVE_FAILURES;
        session.updated_at = now;
        self.save();
        if auto_paused {
            self.show_toast(tr!("routine.toast_auto_paused"));
        } else if manual {
            self.show_toast(tr!("routine.toast_failed", error = message));
        }
        cx.notify();
    }

    /// Delivers a found update once its session can take it: not mid-turn,
    /// and with its transcript loaded. Otherwise the update stays pending and
    /// the scheduler tick retries.
    fn try_deliver_routine_update(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let Some((busy, loaded, has_routine)) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(|session| {
                (
                    session.is_busy(),
                    session.detail_loaded,
                    session.routine.is_some(),
                )
            })
        else {
            self.routine_checks.pending.remove(&session_id);
            return;
        };
        if !has_routine {
            self.routine_checks.pending.remove(&session_id);
            return;
        }
        if busy {
            // A running turn owns the end of the transcript. The probe's
            // transcript copy is stale once that turn adds to it.
            if let Some(pending) = self.routine_checks.pending.get_mut(&session_id) {
                pending.hydrated_session = None;
            }
            return;
        }
        if !loaded {
            let hydrated = self
                .routine_checks
                .pending
                .get_mut(&session_id)
                .and_then(|pending| pending.hydrated_session.take());
            match hydrated {
                Some(hydrated) => self.adopt_routine_hydration(hydrated, cx),
                None => {
                    self.hydrate_for_routine_update(session_id, cx);
                    return;
                }
            }
        }
        let Some(pending) = self.routine_checks.pending.remove(&session_id) else {
            return;
        };
        self.deliver_routine_update(session_id, pending, cx);
    }

    /// Installs a fetched transcript into a still-unloaded session the way
    /// `ensure_session_loaded` does — the whole session, so no detail field
    /// is reset to a skeleton default and then saved over the daemon's copy —
    /// while keeping the routine state this client has recorded since.
    fn adopt_routine_hydration(&mut self, mut hydrated: AgentSession, cx: &mut Context<Self>) {
        let session_id = hydrated.id;
        let Some(existing) = self
            .state
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        if existing.detail_loaded {
            return;
        }
        hydrated.routine = existing.routine.take();
        *existing = hydrated;
        if self.state.selected_session == Some(session_id) {
            self.reset_visible_state();
            self.reset_transcript_rows(self.transcript_row_count());
            self.refresh_composer_sources(cx);
        }
    }

    fn hydrate_for_routine_update(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        // A selection's own hydration is already fetching it; the next tick
        // delivers once that lands.
        if self.session_hydrations.contains(&session_id)
            || !self.routine_checks.hydrating.insert(session_id)
        {
            return;
        }
        let daemon = self.daemon.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(
                    async move { raku_client::persistence::hydrate_session(&daemon, session_id) },
                )
                .await;
            let _ = this.update(cx, |raku, cx| {
                raku.routine_checks.hydrating.remove(&session_id);
                if !raku.routine_checks.pending.contains_key(&session_id) {
                    return;
                }
                match result {
                    Ok(Some(session)) => {
                        raku.adopt_routine_hydration(session, cx);
                        raku.try_deliver_routine_update(session_id, cx);
                    }
                    Ok(None) => {
                        raku.routine_checks.pending.remove(&session_id);
                    }
                    Err(error) => {
                        let Some(pending) = raku.routine_checks.pending.get_mut(&session_id) else {
                            return;
                        };
                        pending.hydration_attempts += 1;
                        if pending.hydration_attempts < ROUTINE_DELIVERY_MAX_HYDRATION_ATTEMPTS {
                            return;
                        }
                        // Give up without advancing the snapshot, so the next
                        // check finds the same news again.
                        if let Some(pending) = raku.routine_checks.pending.remove(&session_id) {
                            raku.record_routine_failure(
                                session_id,
                                pending.duration_ms,
                                format!("could not load the task to deliver an update: {error}"),
                                pending.manual,
                                cx,
                            );
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn deliver_routine_update(
        &mut self,
        session_id: Uuid,
        pending: PendingRoutineUpdate,
        cx: &mut Context<Self>,
    ) {
        let PendingRoutineUpdate {
            checked_at,
            duration_ms,
            manual,
            probe,
            ..
        } = pending;
        let previous_kinds = self.snapshot_selected_transcript_rows(session_id);
        let Some(session) = self.state.session_mut(session_id) else {
            return;
        };
        let Some(routine) = session.routine.as_mut() else {
            return;
        };
        routine.record_run(
            checked_at,
            duration_ms,
            RoutineRunOutcome::Updated,
            probe.summary.clone(),
            probe.snapshot,
            None,
        );
        let body = probe
            .message
            .or_else(|| probe.summary.clone())
            .unwrap_or_default();
        // Stamped at delivery, not at the check: a turn may have finished in
        // between, and only messages after the last turn count as unseen.
        let delivered_at = unix_time();
        session.messages.push(Message::new(
            MessageRole::System,
            tr!(
                "routine.update_pill",
                time = format_routine_timestamp(checked_at)
            ),
        ));
        session
            .messages
            .push(Message::new(MessageRole::Assistant, body));
        session.last_reply_at = Some(delivered_at);
        session.updated_at = delivered_at;
        let project_id = session.project_id;
        let notification_body = probe
            .summary
            .unwrap_or_else(|| tr!("routine.notification_body"));

        let selected = self.state.selected_session == Some(session_id);
        let pending_same = self
            .pending_session_activation
            .map(|pending| pending.session_id)
            == Some(session_id);
        if !selected && !pending_same {
            self.finished_session_ids.insert(session_id);
        }
        if self.state.selected_project != Some(project_id) {
            self.finished_project_ids.insert(project_id);
        }
        if let Some(title) = self.routine_notification_title(session_id, cx) {
            crate::platform::show_task_notification(
                &task_notification_tag(session_id),
                &title,
                &notification_body,
                cx,
            );
        }
        if let Some(previous_kinds) = previous_kinds.as_deref() {
            self.splice_transcript_rows_after_visibility_change(previous_kinds);
        }

        self.save();
        if manual {
            self.show_info_toast(tr!("routine.toast_updated"));
        }
        cx.notify();
    }

    fn routine_notification_title(&self, session_id: Uuid, cx: &Context<Self>) -> Option<String> {
        let session = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)?;
        if session.is_child() {
            return None;
        }
        if cx.active_window().is_some() && self.state.selected_session == Some(session_id) {
            return None;
        }
        let task = localized_session_title(session);
        let project = self
            .state
            .projects
            .iter()
            .find(|project| project.id == session.project_id && !project.is_projectless())
            .map(|project| project.name.as_str());
        Some(match project {
            Some(project) => format!("{project} · {task}"),
            None => task,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentTurn, TurnStatus};

    #[test]
    fn parses_fenced_json_probe_response() {
        let raw = "Here is the result:\n```json\n{\"has_update\": true, \"summary\": \"v2.0 released\", \"message\": \"## v2.0\\nNew release\", \"snapshot\": \"latest=v2.0\"}\n```";
        let parsed = parse_routine_probe_response(raw).expect("valid json");
        assert!(parsed.has_update);
        assert_eq!(parsed.summary.as_deref(), Some("v2.0 released"));
        assert_eq!(parsed.message.as_deref(), Some("## v2.0\nNew release"));
        assert_eq!(parsed.snapshot.as_deref(), Some("latest=v2.0"));
    }

    #[test]
    fn parses_no_update_and_clears_message() {
        let raw = "{\"has_update\": false, \"summary\": \"Still v1.9\", \"message\": \"ignored\", \"snapshot\": \"latest=v1.9\"}";
        let parsed = parse_routine_probe_response(raw).expect("valid json");
        assert!(!parsed.has_update);
        assert!(parsed.message.is_none());
        assert_eq!(parsed.summary.as_deref(), Some("Still v1.9"));
        assert_eq!(parsed.snapshot.as_deref(), Some("latest=v1.9"));
    }

    #[test]
    fn rejects_invalid_json_probe_response() {
        assert!(parse_routine_probe_response("no json here").is_err());
        assert!(parse_routine_probe_response("   ").is_err());
    }

    #[test]
    fn attaches_only_unseen_routine_updates_after_last_turn() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        let turn_id = Uuid::new_v4();
        session.turns.push(AgentTurn {
            id: turn_id,
            turn_count: 1,
            status: TurnStatus::Completed,
            provider_turn_started: true,
            provider_resume_at: None,
            model: None,
            reasoning_effort: None,
            started_at: 100,
            completed_at: Some(120),
            checkpoint: None,
        });
        session.messages.push(Message {
            id: Uuid::new_v4(),
            turn_id: Some(turn_id),
            role: MessageRole::Assistant,
            content: "old reply".into(),
            display_content: None,
            attachments: Vec::new(),
            tokens: Vec::new(),
            created_at: 120,
            streaming: false,
            error: None,
        });
        session.messages.push(Message {
            id: Uuid::new_v4(),
            turn_id: None,
            role: MessageRole::System,
            content: "Routine update".into(),
            display_content: None,
            attachments: Vec::new(),
            tokens: Vec::new(),
            created_at: 200,
            streaming: false,
            error: None,
        });
        session.messages.push(Message {
            id: Uuid::new_v4(),
            turn_id: None,
            role: MessageRole::Assistant,
            content: "Breaking: v2.0 shipped".into(),
            display_content: None,
            attachments: Vec::new(),
            tokens: Vec::new(),
            created_at: 200,
            streaming: false,
            error: None,
        });

        let submission = ComposerSubmission {
            prompt: "Tell me more about v2.0".into(),
            display_content: None,
            attachments: Vec::new(),
            tokens: Vec::new(),
        };
        let enriched = attach_unseen_routine_updates(&session, submission);
        assert_eq!(
            enriched.display_content.as_deref(),
            Some("Tell me more about v2.0")
        );
        assert!(enriched.prompt.contains("<routine_updates>"));
        assert!(enriched.prompt.contains("Breaking: v2.0 shipped"));
        assert!(enriched.prompt.ends_with("Tell me more about v2.0"));

        // A command keeps the first word, so no context goes in front of it.
        let command = ComposerSubmission {
            prompt: "/review the release".into(),
            display_content: None,
            attachments: Vec::new(),
            tokens: Vec::new(),
        };
        let untouched = attach_unseen_routine_updates(&session, command);
        assert_eq!(untouched.prompt, "/review the release");
        assert!(untouched.display_content.is_none());
    }

    fn agent(
        provider: Option<ProviderKind>,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> SubagentDefinition {
        SubagentDefinition {
            name: "watcher".into(),
            description: String::new(),
            provider,
            model: model.map(str::to_owned),
            reasoning_effort: effort.map(str::to_owned),
            read_only: false,
            instructions: "  Only material news.  ".into(),
            enabled: true,
            builtin: false,
        }
    }

    #[test]
    fn routine_runner_defaults_to_the_task() {
        let config = resolve_routine_runner(
            ProviderKind::Claude,
            Some("opus".into()),
            Some("high".into()),
            None,
        );
        assert_eq!(config.provider, ProviderKind::Claude);
        assert_eq!(config.model.as_deref(), Some("opus"));
        assert_eq!(config.reasoning_effort.as_deref(), Some("high"));
        assert!(config.on_task_model);
        assert!(config.instructions.is_empty());
    }

    #[test]
    fn routine_runner_uses_the_agent_model_and_not_the_task_effort() {
        let runner = agent(None, Some("haiku"), None);
        let config = resolve_routine_runner(
            ProviderKind::Claude,
            Some("opus".into()),
            Some("high".into()),
            Some(&runner),
        );
        assert_eq!(config.provider, ProviderKind::Claude);
        assert_eq!(config.model.as_deref(), Some("haiku"));
        // The task's effort belongs to the task's model.
        assert_eq!(config.reasoning_effort, None);
        assert!(!config.on_task_model);
        assert_eq!(config.instructions, "Only material news.");
    }

    #[test]
    fn routine_runner_without_a_model_inherits_only_on_the_same_provider() {
        let same = agent(None, None, Some("low"));
        let config = resolve_routine_runner(
            ProviderKind::Claude,
            Some("opus".into()),
            Some("high".into()),
            Some(&same),
        );
        assert_eq!(config.model.as_deref(), Some("opus"));
        assert_eq!(config.reasoning_effort.as_deref(), Some("low"));
        assert!(config.on_task_model);

        let other = agent(Some(ProviderKind::Codex), None, None);
        let config = resolve_routine_runner(
            ProviderKind::Claude,
            Some("opus".into()),
            Some("high".into()),
            Some(&other),
        );
        assert_eq!(config.provider, ProviderKind::Codex);
        // Left to the other provider's preferred model at launch.
        assert_eq!(config.model, None);
        assert_eq!(config.reasoning_effort, None);
        assert!(!config.on_task_model);
    }

    #[test]
    fn probe_prompt_carries_runner_instructions_only_when_set() {
        let with = build_routine_probe_prompt(
            "Cite URLs.",
            "English",
            "watch x",
            None,
            None,
            1_700_000_000,
        );
        assert!(with.contains("<agent_instructions>\nCite URLs.\n</agent_instructions>"));
        let without =
            build_routine_probe_prompt("", "English", "watch x", None, None, 1_700_000_000);
        assert!(!without.contains("<agent_instructions>"));
    }

    #[test]
    fn probe_prompt_names_the_reply_language() {
        let prompt = build_routine_probe_prompt(
            "",
            routine_reply_language(crate::i18n::AppLanguage::SimplifiedChinese),
            "say hello to me.",
            None,
            None,
            1_700_000_000,
        );
        assert!(prompt.contains("Otherwise write them in Simplified Chinese."));
        // A language asked for in the target takes precedence over the setting.
        assert!(prompt.contains("if <monitoring_target> says which language to write in"));
        // The target can opt into a message on every check.
        assert!(prompt.contains("set \"has_update\" to true on every check"));
        assert!(prompt.contains("unless the Delivery rule below applies"));
        assert!(!prompt.contains("same language as the monitoring target"));
        assert_eq!(
            routine_reply_language(crate::i18n::AppLanguage::Japanese),
            "Japanese"
        );
        assert_eq!(
            routine_reply_language(crate::i18n::AppLanguage::English),
            "English"
        );
    }

    #[test]
    fn schedule_next_run_clamps_minimum_interval() {
        let next = RoutineSchedule::EveryMinutes { minutes: 1 }.next_run_after(1000);
        assert_eq!(next, 1000 + 5 * 60);
    }

    #[test]
    fn routine_auto_pauses_after_consecutive_failures() {
        let mut routine = SessionRoutine::new(
            "watch x".into(),
            RoutineSchedule::EveryMinutes { minutes: 60 },
            1000,
        );
        for i in 0..ROUTINE_MAX_CONSECUTIVE_FAILURES {
            routine.record_run(
                1000 + (i as u64 + 1) * 60,
                500,
                RoutineRunOutcome::Failed,
                Some("timeout".into()),
                None,
                Some("timeout".into()),
            );
        }
        assert!(!routine.enabled);
        assert!(routine.next_run_at.is_none());
        assert_eq!(
            routine.consecutive_failures,
            ROUTINE_MAX_CONSECUTIVE_FAILURES
        );
    }
}
