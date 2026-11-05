use super::*;

impl Waku {
    /// Title for a turn-end banner, or `None` when the user is already
    /// looking at the task's project. A banner is due when Waku is in the
    /// background, or when the task belongs to a project other than the
    /// selected one — the sidebar dot alone is easy to miss there.
    fn turn_end_notification_title(&self, session_id: Uuid, cx: &App) -> Option<String> {
        let session = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)?;
        let other_project = self.state.selected_project != Some(session.project_id);
        if cx.active_window().is_some() && !other_project {
            return None;
        }
        let title = if session.display_title() == AgentSession::DEFAULT_TITLE {
            tr!("session.new_task")
        } else {
            session.display_title().to_owned()
        };
        let project = other_project
            .then(|| {
                self.state
                    .projects
                    .iter()
                    .find(|project| project.id == session.project_id)
            })
            .flatten()
            .filter(|project| !project.is_projectless());
        Some(match project {
            Some(project) => format!("{} · {title}", project.name),
            None => title,
        })
    }

    pub(super) fn finish_streaming_assistant(&mut self, session_id: Uuid) {
        if let Some(session) = self.state.session_mut(session_id) {
            for message in &mut session.messages {
                if message.role == MessageRole::Assistant && message.streaming {
                    message.streaming = false;
                }
            }
        }
    }

    pub(super) fn append_text_delta(
        &mut self,
        session_id: Uuid,
        runtime: &mut SessionRuntime,
        delta: String,
    ) {
        let previous_phase = runtime.stream_phase;
        let continuing = previous_phase == Some(StreamPhase::Text);
        if !continuing && delta.trim().is_empty() {
            // Claude Code emits whitespace-only text blocks (`"\n\n"`) between
            // consecutive tool calls. Starting a message for one would put an
            // empty row between activity groups, so leave the phase where it
            // is and let the next activity stay in the current cluster too.
            return;
        }
        if previous_phase == Some(StreamPhase::Reasoning) {
            self.complete_reasoning_activity(session_id);
        }
        append_text_delta_to_session(&mut self.state.sessions, session_id, continuing, delta);
        self.state.mark_session_dirty(session_id);
        runtime.stream_phase = Some(StreamPhase::Text);
    }

    fn complete_reasoning_activity(&mut self, session_id: Uuid) {
        if let Some(session) = self.state.session_mut(session_id) {
            complete_session_reasoning(session);
        }
    }

    pub(super) fn append_reasoning_delta(
        &mut self,
        session_id: Uuid,
        runtime: &mut SessionRuntime,
        delta: String,
    ) {
        if let Some(session) = self.state.session_mut(session_id)
            && append_session_reasoning(session, runtime.stream_phase, delta, unix_time_millis())
        {
            runtime.stream_phase = Some(StreamPhase::Reasoning);
        }
    }

    pub(super) fn update_activity(
        &mut self,
        session_id: Uuid,
        runtime: &mut SessionRuntime,
        item: ActivityItem,
    ) {
        let mut image_block = None;
        if let Some(session) = self.state.session_mut(session_id) {
            let replaced = !item.file_changes.is_empty();
            let image_changed = item.kind == ActivityKind::ImageGeneration;
            let (id, inserted) = update_session_activity(session, runtime.stream_phase, item);
            if image_changed {
                image_block = session
                    .transcript_blocks
                    .iter()
                    .position(|block| block.activities.iter().any(|activity| activity.id == id));
            }
            if inserted {
                runtime.stream_phase = Some(StreamPhase::Activity);
            }
            if replaced {
                self.activity_diffs.borrow_mut().remove(&id);
            }
        }
        // An image can finish after later parallel work has already advanced
        // beyond the small streaming tail. Invalidate its original row only.
        if self.state.selected_session == Some(session_id)
            && let Some(block) = image_block
        {
            self.sync_transcript_rows();
            if let Some(row) = self
                .transcript_row_kinds
                .borrow()
                .iter()
                .position(|row| *row == TranscriptRowKind::TurnBlock(block))
            {
                self.remeasure_transcript_rows(row..row + 1);
            }
        }
    }

    pub(super) fn complete_turn_blocks(&mut self, session_id: Uuid) {
        if let Some(session) = self.state.session_mut(session_id) {
            finish_session_activities(session, unix_time_millis());
        }
    }

    pub(super) fn turn_has_assistant_message(&self, session_id: Uuid) -> bool {
        self.state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .is_some_and(|session| {
                let Some(turn_id) = session.active_turn_id() else {
                    return false;
                };
                session.messages.iter().any(|message| {
                    message.role == MessageRole::Assistant && message.turn_id == Some(turn_id)
                })
            })
    }

    /// Whether the running turn was prompted — a provider-started wake has no
    /// user message of its own.
    pub(super) fn active_turn_has_user_message(&self, session_id: Uuid) -> bool {
        self.state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| {
                let turn_id = session.active_turn_id()?;
                Some(session.messages.iter().any(|message| {
                    message.turn_id == Some(turn_id) && message.role == MessageRole::User
                }))
            })
            .unwrap_or(false)
    }

    pub(super) fn accepts_turn_output(&mut self, session_id: Uuid) -> bool {
        // The turn begins at submission accept, before its prompt has reached
        // any provider. While preparation is still running, a reused runtime
        // could only be draining leftovers of a settled turn — output landing
        // in the new turn then would attribute stale text to it.
        if self.submission_preparations.contains(&session_id) {
            return false;
        }
        self.state
            .session_mut(session_id)
            .is_some_and(session_accepts_turn_output)
    }

    /// Returns whether the runtime should remain attached after this event.
    ///
    /// `allow_queue_drain` is false when the caller is flushing buffered
    /// events for a turn the user just stopped: a settling event must not
    /// start queued follow-ups then, because the user asked to stop, not to
    /// continue.
    pub(super) fn handle_driver_event(
        &mut self,
        session_id: Uuid,
        runtime: &mut SessionRuntime,
        event: DriverEvent,
        allow_queue_drain: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        runtime.last_active_at = Instant::now();
        match event {
            DriverEvent::RuntimeEventCursorAdvanced(cursor) => {
                if let Some(session) = self.state.session_mut(session_id) {
                    session.runtime_event_cursor = Some(cursor);
                }
            }
            DriverEvent::Connected { provider_cursor } => {
                runtime.last_driver_error = None;
                runtime.last_background_refresh_at = Instant::now();
                runtime.driver.refresh_background_work();
                if let Some(session) = self.state.session_mut(session_id) {
                    if let Some(ProviderResumeCursor::Claude {
                        resume_at: Some(message_id),
                        ..
                    }) = &provider_cursor
                    {
                        session.mark_active_turn_provider_resume_at(message_id.clone());
                    }
                    session.provider_cursor = provider_cursor;
                    if session.status == SessionStatus::Connecting {
                        session.status = SessionStatus::Working;
                    }
                }
            }
            DriverEvent::AgentPresetSelected(agent_preset) => {
                if let Some(session) = self.state.session_mut(session_id) {
                    session.agent_preset = agent_preset;
                }
            }
            DriverEvent::AutoTitleUpdated(title) => {
                if let Some(session) = self.state.session_mut(session_id) {
                    session.set_auto_title(title);
                }
            }
            DriverEvent::AvailableCommands(names) => {
                if let Some(session) = self
                    .state
                    .session_mut(session_id)
                    .filter(|session| session.available_commands != names)
                {
                    session.available_commands = names;
                    // The drain has no `Context`; the frame loop rebuilds the
                    // drawn index when it sees this.
                    self.composer_sources_stale = true;
                }
            }
            DriverEvent::TurnStarted => {
                runtime.last_driver_error = None;
                let turn_options = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| self.turn_model_options(session));
                if let Some(session) = self.state.session_mut(session_id) {
                    if session.active_turn_id().is_some() {
                        // Covers submissions and the optimistic pursuit turn
                        // a `/goal` began: the provider's start confirms it.
                        session.mark_active_turn_provider_started();
                        session.status = SessionStatus::Working;
                    } else if matches!(session.provider, ProviderKind::Codex | ProviderKind::Claude)
                    {
                        // Some providers start turns on their own: Codex goal
                        // continuation pursues an active goal whenever the
                        // thread is idle, Claude Code re-enters the model
                        // once a backgrounded command, subagent or monitor
                        // settles. Give the turn a transcript home —
                        // there is no user message for it — so its work
                        // streams in instead of being dropped.
                        session.begin_provider_turn();
                        session.mark_active_turn_provider_started();
                        session.status = SessionStatus::Working;
                        self.state.mark_session_dirty(session_id);
                    }
                    if let Some(session) = self.state.session_mut(session_id)
                        && let Some(turn) = session.turns.last_mut()
                        && turn.status == TurnStatus::Running
                        && let Some((model, effort)) = turn_options
                    {
                        turn.model = turn.model.take().or(model);
                        turn.reasoning_effort = turn.reasoning_effort.take().or(effort);
                    }
                }
            }
            DriverEvent::TurnParked => {
                // The reply ended while detached work the provider will wake
                // the session for is still running. The turn stays open for
                // that wake; only its streaming state settles, and the session
                // shows the wait instead of a finish. A prompted turn announces
                // the wait once; a wake that parks again stays quiet.
                if self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .and_then(AgentSession::active_turn_id)
                    .is_none()
                {
                    return true;
                }
                self.settle_foreground_work(session_id, BackgroundWorkStatus::Completed);
                let previous_kinds = self.snapshot_selected_transcript_rows(session_id);
                let announce =
                    !runtime.park_announced && self.active_turn_has_user_message(session_id);
                let task_notification = announce
                    .then(|| self.turn_end_notification_title(session_id, cx))
                    .flatten();
                self.finish_streaming_assistant(session_id);
                self.complete_turn_blocks(session_id);
                runtime.stream_phase = None;
                runtime.park_announced = true;
                if let Some(session) = self.state.session_mut(session_id) {
                    session.status = SessionStatus::Background;
                    session.updated_at = unix_time();
                }
                if let Some(previous_kinds) = previous_kinds.as_deref() {
                    self.splice_active_transcript_rows_after_visibility_change(previous_kinds);
                }
                if let Some(title) = task_notification {
                    crate::platform::show_task_notification(
                        &task_notification_tag(session_id),
                        &title,
                        &tr!("session.turn_waiting_background"),
                        cx,
                    );
                }
            }
            DriverEvent::TextDelta(delta) => {
                if self.accepts_turn_output(session_id) {
                    self.append_text_delta(session_id, runtime, delta);
                }
            }
            DriverEvent::ReasoningDelta(delta) => {
                if self.accepts_turn_output(session_id) {
                    self.append_reasoning_delta(session_id, runtime, delta);
                }
            }
            DriverEvent::ReasoningStarted { id, timestamp_ms } => {
                if self.accepts_turn_output(session_id)
                    && let Some(session) = self.state.session_mut(session_id)
                {
                    start_session_reasoning(session, runtime.stream_phase, id, timestamp_ms);
                    runtime.stream_phase = Some(StreamPhase::Reasoning);
                }
            }
            DriverEvent::ReasoningBlockDelta { id, delta } => {
                if self.accepts_turn_output(session_id)
                    && let Some(session) = self.state.session_mut(session_id)
                {
                    append_session_reasoning_block(
                        session,
                        runtime.stream_phase,
                        id,
                        delta,
                        unix_time_millis(),
                    );
                    runtime.stream_phase = Some(StreamPhase::Reasoning);
                }
            }
            DriverEvent::ReasoningCompleted {
                id,
                timestamp_ms,
                content,
            } => {
                if self.accepts_turn_output(session_id)
                    && let Some(session) = self.state.session_mut(session_id)
                {
                    finish_session_reasoning_block(
                        session,
                        runtime.stream_phase,
                        id,
                        content,
                        timestamp_ms,
                    );
                }
            }
            DriverEvent::Activity {
                id,
                kind,
                title,
                detail,
                complete,
            } => {
                if self.accepts_turn_output(session_id) {
                    let refresh_branch = should_refresh_branch_after_activity(kind, complete)
                        && self.state.selected_session == Some(session_id);
                    let item = ActivityItem::new(id, kind, title, detail, complete);
                    self.observe_foreground_command_activity(session_id, &item);
                    self.update_activity(session_id, runtime, item);
                    if refresh_branch {
                        self.refresh_selected_branch_snapshot(cx);
                    }
                }
            }
            DriverEvent::CompactionUpdated(item) => {
                use waku_protocol::compaction::CompactionStatus;
                let completed = item.status != CompactionStatus::Running;
                let succeeded = item.status == CompactionStatus::Completed;
                let resume_queue = matches!(
                    item.status,
                    CompactionStatus::Completed | CompactionStatus::Unconfirmed
                );
                let id = item.id.clone();
                let (was_active, previous_status) = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| {
                        let previous = session
                            .transcript_blocks
                            .iter()
                            .rev()
                            .flat_map(|block| block.activities.iter().rev())
                            .filter_map(|activity| activity.compaction.as_ref())
                            .find(|old| old.id == id)
                            .map(|old| old.status);
                        (
                            session.active_compaction.as_deref() == Some(id.as_str()),
                            previous,
                        )
                    })
                    .unwrap_or((false, None));
                if !completed
                    && previous_status.is_some_and(|status| status != CompactionStatus::Running)
                {
                    return true;
                }
                // Estimated post-compaction counts belong on the milestone;
                // the occupancy gauge waits for a measured provider update.
                let after = item.after_tokens.filter(|_| !item.after_tokens_estimated);
                if let Some(session) = self.state.session_mut(session_id) {
                    if !completed {
                        session.active_compaction = Some(id.clone());
                    } else if session.active_compaction.as_deref() == Some(&id) {
                        session.active_compaction = None;
                    }
                    if succeeded && previous_status != Some(CompactionStatus::Completed) {
                        session.context_usage_pending = after.is_none();
                        if let Some(tokens) = after {
                            session.context_usage.get_or_insert_default().tokens = tokens;
                        }
                    }
                }
                self.update_activity(session_id, runtime, ActivityItem::from_compaction(item));
                self.state.mark_session_dirty(session_id);
                if completed
                    && self
                        .pending_compactions
                        .get(&session_id)
                        .is_some_and(|request| request.id == id)
                {
                    self.pending_compactions.remove(&session_id);
                }
                if resume_queue && was_active && allow_queue_drain {
                    self.pending_queue_drains.push(session_id);
                }
            }
            DriverEvent::RichActivity(item) => {
                if self.accepts_turn_output(session_id) {
                    let refresh_branch =
                        should_refresh_branch_after_activity(item.kind, item.complete)
                            && self.state.selected_session == Some(session_id);
                    self.observe_foreground_command_activity(session_id, &item);
                    self.update_activity(session_id, runtime, item);
                    if refresh_branch {
                        self.refresh_selected_branch_snapshot(cx);
                    }
                }
            }
            DriverEvent::BackgroundWork(event) => {
                // Background work is session state, not turn output. It must
                // survive a settled or rewound turn and therefore bypasses
                // `accepts_turn_output` deliberately.
                self.handle_background_work_event(session_id, event);
            }
            DriverEvent::Permission {
                request_id,
                title,
                detail,
                options,
            } => {
                if self.accepts_turn_output(session_id) {
                    runtime.pending_permission = Some(PendingPermission {
                        request_id,
                        title,
                        detail,
                        options,
                    });
                    if let Some(session) = self.state.session_mut(session_id) {
                        session.status = SessionStatus::Waiting;
                    }
                }
            }
            DriverEvent::PlanProposed { request_id } => {
                if self.accepts_turn_output(session_id) {
                    let blocking = request_id.is_some();
                    self.pending_plans
                        .insert(session_id, PendingPlan { request_id });
                    if blocking && let Some(session) = self.state.session_mut(session_id) {
                        session.status = SessionStatus::Waiting;
                    }
                }
            }
            DriverEvent::UserInputRequested {
                request_id,
                questions,
                delivery,
            } => {
                if self.accepts_turn_output(session_id) && !questions.is_empty() {
                    runtime.pending_user_input =
                        Some(PendingUserInput::new(request_id, questions, delivery));
                    if self.state.selected_session == Some(session_id) {
                        self.user_input_answer
                            .update(cx, |input, cx| input.clear(cx));
                    }
                    // An async question does not block the provider: its turn
                    // keeps running and the answer becomes the next message.
                    if delivery == UserInputDelivery::Request
                        && let Some(session) = self.state.session_mut(session_id)
                    {
                        session.status = SessionStatus::Waiting;
                    }
                }
            }
            DriverEvent::SteerAccepted { message } => {
                let submission = runtime
                    .pending_steers
                    .iter()
                    .position(|submission| submission.prompt == message)
                    .and_then(|index| runtime.pending_steers.remove(index))
                    // Providers normally echo the exact transport text, but a
                    // normalized echo still acknowledges the oldest pending
                    // steer. Preserve its attachment presentation metadata.
                    .or_else(|| runtime.pending_steers.pop_front())
                    .unwrap_or_else(|| ComposerSubmission::plain(message.clone()));
                // The provider folded the message into the live turn. Append
                // it to the same turn so the transcript mirrors the provider
                // conversation (no new turn boundary).
                if let Some(session) = self.state.session_mut(session_id) {
                    session.push_user_message_with_presentation(
                        message,
                        submission.display_content,
                        submission.attachments,
                    );
                    session.updated_at = unix_time();
                }
            }
            DriverEvent::SteerRejected { message, reason } => {
                let submission = runtime
                    .pending_steers
                    .iter()
                    .position(|submission| submission.prompt == message)
                    .and_then(|index| runtime.pending_steers.remove(index))
                    .or_else(|| runtime.pending_steers.pop_front())
                    .unwrap_or_else(|| ComposerSubmission::plain(message));
                let (busy, settled_cleanly) = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| {
                        let settled_cleanly = session
                            .turns
                            .last()
                            .is_some_and(|turn| turn.status == TurnStatus::Completed);
                        (session.is_busy(), settled_cleanly)
                    })
                    .unwrap_or((false, false));
                if busy {
                    self.enqueue_follow_up_submission(session_id, submission, cx);
                    if self.state.selected_session == Some(session_id) {
                        self.show_toast(tr!(
                            "session.steer_rejected",
                            error = compact_driver_error(&reason)
                        ));
                    }
                } else if settled_cleanly {
                    // The turn settled before the steer arrived; run the
                    // message as a fresh turn instead of losing it. Submission
                    // is deferred through the queue-drain pass because this
                    // session's runtime is detached from the map while its
                    // events are handled — an inline submit would spawn a
                    // second driver process only to have it clobbered when the
                    // drain re-inserts the detached runtime.
                    if let Some(session) = self.state.session_mut(session_id) {
                        session
                            .queued_messages
                            .insert(0, submission.into_queued_message());
                    }
                    if allow_queue_drain {
                        self.pending_queue_drains.push(session_id);
                    }
                } else {
                    // The user stopped the turn (or the provider died) before
                    // the steer landed. Keep the message visible and
                    // user-controlled instead of auto-running it.
                    self.enqueue_follow_up_submission(session_id, submission, cx);
                }
            }
            DriverEvent::PlanUsageUpdated(usage) => {
                if let Some(provider) = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| session.provider)
                {
                    self.plan_usage
                        .insert(usage_meter::PlanUsageKey::Provider(provider), usage);
                }
            }
            DriverEvent::GoalUpdated(goal) => {
                // Conversation meta like usage: it applies regardless of turn
                // state, and `None` means the provider cleared the goal.
                if goal.is_some() {
                    self.goal_observed_at.insert(session_id, Instant::now());
                } else {
                    self.goal_observed_at.remove(&session_id);
                }
                if let Some(session) = self.state.session_mut(session_id) {
                    if let Some(goal) = &goal
                        && session.messages.is_empty()
                    {
                        // A goal-first task is named after its objective
                        // until the provider reports a better title.
                        session.set_title_from_prompt(&goal.objective);
                    }
                    if session.thread_goal != goal {
                        session.thread_goal = goal;
                        self.state.mark_session_dirty(session_id);
                    }
                }
            }
            DriverEvent::UsageUpdated {
                context_tokens,
                context_window,
            } => {
                // Meta about the conversation, not turn output: it applies
                // even while a rewound or cancelled turn's tail drains.
                if let Some(session) = self.state.session_mut(session_id) {
                    let usage = session.context_usage.get_or_insert(ContextUsage::default());
                    if let Some(tokens) = context_tokens {
                        usage.tokens = tokens;
                        session.context_usage_pending = false;
                    }
                    if let Some(window) = context_window {
                        usage.window = Some(window);
                    }
                    self.state.mark_session_dirty(session_id);
                }
            }
            DriverEvent::TurnFinished { success, summary } => {
                self.settle_foreground_work(
                    session_id,
                    if success {
                        BackgroundWorkStatus::Completed
                    } else {
                        BackgroundWorkStatus::Failed
                    },
                );
                let previous_kinds = self.snapshot_selected_transcript_rows(session_id);
                runtime.last_driver_error = None;
                // A settled turn moved the account's rate-limit needles; ask
                // that provider's plan meter to refresh once its backoff
                // allows.
                if let Some(key) = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .and_then(|session| self.plan_usage_key_for_session(session))
                {
                    self.plan_usage_stale.insert(key);
                }
                if self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .and_then(AgentSession::active_turn_id)
                    .is_none()
                {
                    return true;
                }
                let task_notification =
                    self.turn_end_notification_title(session_id, cx).map(|title| {
                        let body = if success {
                            tr!("session.turn_completed")
                        } else {
                            tr!("session.stopped")
                        };
                        (title, body)
                    });
                self.finish_streaming_assistant(session_id);
                self.complete_turn_blocks(session_id);
                runtime.stream_phase = None;
                runtime.park_announced = false;
                let compaction_only = !self.active_turn_has_user_message(session_id)
                    && self
                        .state
                        .sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .is_some_and(|session| {
                            session.transcript_blocks.last().is_some_and(|block| {
                                block.after_message == session.messages.len()
                                    && block
                                        .activities
                                        .last()
                                        .is_some_and(|item| item.compaction.is_some())
                            })
                        });
                let needs_fallback =
                    !self.turn_has_assistant_message(session_id) && !compaction_only;
                if let Some(session) = self.state.session_mut(session_id) {
                    session.status = if success {
                        SessionStatus::Idle
                    } else {
                        SessionStatus::Failed
                    };
                    if needs_fallback {
                        session.push_message(
                            MessageRole::Assistant,
                            summary.unwrap_or_else(|| {
                                if success {
                                    tr!("session.turn_completed")
                                } else {
                                    tr!("session.stopped_before_response")
                                }
                            }),
                        );
                    }
                }
                self.finish_active_turn_with_analytics(
                    session_id,
                    if success {
                        TurnStatus::Completed
                    } else {
                        TurnStatus::Failed
                    },
                    if success {
                        crate::analytics::TurnOutcome::Completed
                    } else {
                        crate::analytics::TurnOutcome::Failed
                    },
                );
                runtime.pending_permission = None;
                self.clear_blocking_plan(session_id);
                // An async question outlives the turn that asked it; its
                // answer is what starts the next one.
                if runtime
                    .pending_user_input
                    .as_ref()
                    .is_some_and(|pending| pending.delivery == UserInputDelivery::Request)
                {
                    runtime.pending_user_input = None;
                }
                // The agent may have edited files or switched branches, so the
                // cached view of the workspace is no longer trustworthy. This
                // handler has no `Context`, so the drain loop acts on the flag.
                if self.state.selected_session == Some(session_id) {
                    self.workspace_queries_stale = true;
                }
                runtime.driver.refresh_background_work();
                self.capture_latest_turn_checkpoint_for(session_id);
                if allow_queue_drain && success {
                    // Start the next queued follow-up once the runtime has
                    // been re-inserted so the same process is reused.
                    self.pending_queue_drains.push(session_id);
                }
                if let Some(previous_kinds) = previous_kinds.as_deref() {
                    self.splice_active_transcript_rows_after_visibility_change(previous_kinds);
                }
                if let Some((title, body)) = task_notification {
                    crate::platform::show_task_notification(
                        &task_notification_tag(session_id),
                        &title,
                        &body,
                        cx,
                    );
                }
            }
            DriverEvent::Error(error) => {
                let error = compact_driver_error(&error);
                runtime.last_driver_error = Some(error.clone());
                if self.state.selected_session == Some(session_id) {
                    self.show_toast(error.clone());
                }
                // An optimistic pursuit turn has no submission to fail with.
                // Unwind it so the error cannot strand a spinner; if the
                // pursuit does start later, its own start report recreates
                // the turn.
                self.unwind_unconfirmed_pursuit_turn(session_id);
                let has_active_turn = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .and_then(AgentSession::active_turn_id)
                    .is_some();
                let should_append = has_active_turn
                    && !self.turn_has_assistant_message(session_id)
                    && self
                        .state
                        .sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .is_some_and(|session| session.status != SessionStatus::Working);
                if let Some(session) = self.state.session_mut(session_id)
                    && has_active_turn
                {
                    if session.status != SessionStatus::Working {
                        session.status = SessionStatus::Failed;
                    }
                    if should_append {
                        session.push_message(MessageRole::Assistant, error);
                    }
                }
            }
            DriverEvent::ProcessExited => {
                self.interrupt_session_compactions(session_id);
                self.mark_background_work_lost(session_id);
                let previous_kinds = self.snapshot_selected_transcript_rows(session_id);
                self.finish_streaming_assistant(session_id);
                self.complete_turn_blocks(session_id);
                runtime.stream_phase = None;
                runtime.pending_permission = None;
                runtime.pending_user_input = None;
                self.clear_blocking_plan(session_id);
                let needs_fallback = !self.turn_has_assistant_message(session_id);
                let failure_message = runtime
                    .last_driver_error
                    .take()
                    .unwrap_or_else(|| tr!("session.codex_exited_before_response"));
                let should_finish_turn = if let Some(session) = self.state.session_mut(session_id)
                    && session.status.is_busy()
                {
                    session.status = SessionStatus::Failed;
                    session.updated_at = unix_time();
                    if needs_fallback {
                        session.push_message(MessageRole::Assistant, failure_message);
                    }
                    true
                } else {
                    false
                };
                let finished_turn = should_finish_turn
                    && self
                        .finish_active_turn_with_analytics(
                            session_id,
                            TurnStatus::Failed,
                            crate::analytics::TurnOutcome::ProcessExited,
                        )
                        .is_some();
                if finished_turn {
                    self.capture_latest_turn_checkpoint_for(session_id);
                }
                if let Some(previous_kinds) = previous_kinds.as_deref() {
                    self.splice_active_transcript_rows_after_visibility_change(previous_kinds);
                }
                return false;
            }
        }
        true
    }
}

/// Foreground output is stronger evidence of a started provider turn than a
/// replayed lifecycle cursor. Repair both pieces of transient state here so a
/// runtime attachment that missed `TurnStarted` cannot leave Cmd-Enter
/// permanently falling back to the follow-up queue while output is visible.
pub(super) fn session_accepts_turn_output(session: &mut AgentSession) -> bool {
    if session.active_turn_id().is_none() || !session.status.is_busy() {
        return false;
    }
    session.mark_active_turn_provider_started();
    if session.status == SessionStatus::Connecting {
        session.status = SessionStatus::Working;
    }
    true
}

/// A completed edit or shell command is the earliest provider-neutral point at
/// which its filesystem effects are stable enough to re-read. The actual Git
/// work remains behind the branch cache's background fetch.
pub(super) fn should_refresh_branch_after_activity(
    kind: crate::model::ActivityKind,
    complete: bool,
) -> bool {
    complete
        && matches!(
            kind,
            crate::model::ActivityKind::Command | crate::model::ActivityKind::FileChange
        )
}

pub(super) fn push_transcript_activity(
    session: &mut AgentSession,
    item: ActivityItem,
    continuing_work: bool,
) {
    let after_message = session.messages.len();
    let turn_id = session.active_turn_id();
    if continuing_work
        && !item.is_standalone()
        && let Some(block) = session.transcript_blocks.last_mut()
        && block.after_message == after_message
        && block.turn_id == turn_id
        && block
            .activities
            .iter()
            .all(|activity| !activity.is_standalone())
    {
        block.activities.push(item);
    } else {
        session.transcript_blocks.push(TranscriptBlock {
            after_message,
            turn_id,
            activities: vec![item],
        });
    }
}

pub(super) fn stream_delta_kind(event: &DriverEvent) -> Option<StreamDeltaKind> {
    match event {
        DriverEvent::TextDelta(_) => Some(StreamDeltaKind::Text),
        DriverEvent::ReasoningDelta(_) => Some(StreamDeltaKind::Reasoning),
        DriverEvent::ReasoningBlockDelta { .. } => Some(StreamDeltaKind::ReasoningBlock),
        _ => None,
    }
}

pub(super) fn stream_delta_text(event: &DriverEvent, kind: StreamDeltaKind) -> Option<&str> {
    match (kind, event) {
        (StreamDeltaKind::Text, DriverEvent::TextDelta(text))
        | (StreamDeltaKind::Reasoning, DriverEvent::ReasoningDelta(text)) => Some(text),
        (StreamDeltaKind::ReasoningBlock, DriverEvent::ReasoningBlockDelta { delta, .. }) => {
            Some(delta)
        }
        _ => None,
    }
}

pub(super) fn compact_driver_error(error: &str) -> String {
    const MAX_LINES: usize = 6;
    const MAX_CHARS: usize = 800;

    let lines = error.lines().collect::<Vec<_>>();
    let mut compact = lines
        .iter()
        .take(MAX_LINES)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > MAX_LINES {
        compact.push_str("\n…");
    }
    if compact.chars().count() > MAX_CHARS {
        compact = compact.chars().take(MAX_CHARS - 1).collect();
        compact.push('…');
    }
    compact
}

/// Coalesce every adjacent delta of one kind while retaining provider order.
/// Runtime cursors are acknowledgements rather than visible boundaries, so the
/// newest cursor follows the combined delta. The full text enters layout in
/// this pass; Markdown's paint-only veil provides the progressive dissolve.
pub(super) fn pop_stream_batch(
    events: &mut VecDeque<DriverEvent>,
    kind: StreamDeltaKind,
) -> Option<DriverEvent> {
    let mut chunk = String::new();
    let mut latest_cursor = None;
    let reasoning_id = match events.front() {
        Some(DriverEvent::ReasoningBlockDelta { id, .. }) => Some(id.clone()),
        _ => None,
    };
    loop {
        match events.front() {
            Some(DriverEvent::RuntimeEventCursorAdvanced(_)) => {
                latest_cursor = events.pop_front();
            }
            Some(event)
                if stream_delta_text(event, kind).is_some()
                    && match event {
                        DriverEvent::ReasoningBlockDelta { id, .. } => {
                            reasoning_id.as_ref() == Some(id)
                        }
                        _ => true,
                    } =>
            {
                let event = events.pop_front()?;
                match (kind, event) {
                    (StreamDeltaKind::Text, DriverEvent::TextDelta(text))
                    | (StreamDeltaKind::Reasoning, DriverEvent::ReasoningDelta(text)) => {
                        chunk.push_str(&text);
                    }
                    (
                        StreamDeltaKind::ReasoningBlock,
                        DriverEvent::ReasoningBlockDelta { delta, .. },
                    ) => {
                        chunk.push_str(&delta);
                    }
                    _ => unreachable!("the stream kind was checked before removing the event"),
                }
            }
            _ => break,
        }
    }
    if let Some(cursor) = latest_cursor {
        events.push_front(cursor);
    }
    match kind {
        StreamDeltaKind::Text => Some(DriverEvent::TextDelta(chunk)),
        StreamDeltaKind::Reasoning => Some(DriverEvent::ReasoningDelta(chunk)),
        StreamDeltaKind::ReasoningBlock => Some(DriverEvent::ReasoningBlockDelta {
            id: reasoning_id?,
            delta: chunk,
        }),
    }
}

pub(super) fn append_text_delta_to_session(
    sessions: &mut [AgentSession],
    session_id: Uuid,
    continuing: bool,
    delta: String,
) {
    let Some(session) = sessions.iter_mut().find(|session| session.id == session_id) else {
        return;
    };
    // Whitespace alone has nothing to show. Starting a message for it leaves a
    // blank transcript row; providers that separate tool calls with `"\n\n"`
    // text blocks would stack one between every pair of activity groups.
    if !continuing && delta.trim().is_empty() {
        return;
    }
    if !continuing {
        for message in &mut session.messages {
            if message.role == MessageRole::Assistant && message.streaming {
                message.streaming = false;
            }
        }
    }
    let existing = continuing.then(|| {
        session
            .messages
            .iter_mut()
            .rev()
            .find(|message| message.role == MessageRole::Assistant && message.streaming)
    });
    if let Some(Some(message)) = existing {
        message.content.push_str(&delta);
    } else {
        let mut message = session
            .active_turn_id()
            .map(|turn_id| Message::new_for_turn(MessageRole::Assistant, delta.clone(), turn_id))
            .unwrap_or_else(|| Message::new(MessageRole::Assistant, delta));
        message.streaming = true;
        session.messages.push(message);
    }
    session.updated_at = unix_time();
}

pub(super) fn finish_session_messages(session: &mut AgentSession) {
    for message in &mut session.messages {
        if message.role == MessageRole::Assistant {
            message.streaming = false;
        }
    }
}

pub(super) fn finish_session_activities(session: &mut AgentSession, now: u64) {
    session.interrupt_compactions(now);
    for block in &mut session.transcript_blocks {
        for activity in &mut block.activities {
            if !activity.complete
                && activity.source_id.is_some()
                && let Some(reasoning) = activity.reasoning.as_mut()
            {
                reasoning.finished_at_ms = now.max(reasoning.started_at_ms);
            }
            activity.complete = true;
        }
    }
}

pub(super) fn complete_session_reasoning(session: &mut AgentSession) {
    if let Some(activity) = session
        .transcript_blocks
        .iter_mut()
        .rev()
        .flat_map(|block| block.activities.iter_mut().rev())
        .find(|activity| {
            activity.reasoning.is_some() && activity.source_id.is_none() && !activity.complete
        })
    {
        activity.complete = true;
        session.updated_at = unix_time();
    }
}

fn reasoning_activity_mut<'a>(
    session: &'a mut AgentSession,
    id: &str,
) -> Option<&'a mut ActivityItem> {
    session
        .transcript_blocks
        .iter_mut()
        .rev()
        .flat_map(|block| block.activities.iter_mut().rev())
        .find(|activity| activity.reasoning.is_some() && activity.source_id.as_deref() == Some(id))
}

pub(super) fn start_session_reasoning(
    session: &mut AgentSession,
    previous_phase: Option<StreamPhase>,
    id: String,
    now: u64,
) {
    if reasoning_activity_mut(session, &id).is_some() {
        return;
    }
    finish_session_messages(session);
    complete_session_reasoning(session);
    let mut activity = ActivityItem::from_reasoning(
        ReasoningBlock {
            content: String::new(),
            started_at_ms: now,
            finished_at_ms: now,
        },
        false,
    );
    activity.source_id = Some(id);
    push_transcript_activity(
        session,
        activity,
        matches!(
            previous_phase,
            Some(StreamPhase::Reasoning | StreamPhase::Activity)
        ),
    );
    session.updated_at = unix_time();
}

pub(super) fn append_session_reasoning_block(
    session: &mut AgentSession,
    previous_phase: Option<StreamPhase>,
    id: String,
    delta: String,
    now: u64,
) {
    start_session_reasoning(session, previous_phase, id.clone(), 0);
    if let Some(activity) = reasoning_activity_mut(session, &id)
        && !activity.complete
        && let Some(reasoning) = activity.reasoning.as_mut()
    {
        reasoning.content.push_str(&delta);
        reasoning.finished_at_ms = now;
        session.updated_at = unix_time();
    }
}

pub(super) fn finish_session_reasoning_block(
    session: &mut AgentSession,
    previous_phase: Option<StreamPhase>,
    id: String,
    content: Option<String>,
    now: u64,
) {
    start_session_reasoning(session, previous_phase, id.clone(), 0);
    if let Some(activity) = reasoning_activity_mut(session, &id)
        && let Some(reasoning) = activity.reasoning.as_mut()
    {
        if let Some(content) = content {
            reasoning.content = content;
        }
        if !activity.complete {
            reasoning.finished_at_ms = now.max(reasoning.started_at_ms);
        }
        activity.complete = true;
        session.updated_at = unix_time();
    }
}

/// Main and delegated streams share phase boundaries and activity grouping.
pub(super) fn append_session_reasoning(
    session: &mut AgentSession,
    previous_phase: Option<StreamPhase>,
    delta: String,
    now: u64,
) -> bool {
    let continuing = previous_phase == Some(StreamPhase::Reasoning);
    if !continuing && delta.trim().is_empty() {
        return false;
    }
    if !continuing {
        finish_session_messages(session);
    }
    if continuing
        && let Some(reasoning) = session
            .transcript_blocks
            .last_mut()
            .and_then(|block| block.activities.last_mut())
            .filter(|activity| activity.source_id.is_none() && !activity.complete)
            .and_then(|activity| activity.reasoning.as_mut())
    {
        reasoning.content.push_str(&delta);
        reasoning.finished_at_ms = now;
    } else {
        push_transcript_activity(
            session,
            ActivityItem::from_reasoning(
                ReasoningBlock {
                    content: delta,
                    started_at_ms: now,
                    finished_at_ms: now,
                },
                false,
            ),
            matches!(
                previous_phase,
                Some(StreamPhase::Reasoning | StreamPhase::Activity)
            ),
        );
    }
    session.updated_at = unix_time();
    true
}

/// Merge provider snapshots without moving their original ordered row.
pub(super) fn update_session_activity(
    session: &mut AgentSession,
    previous_phase: Option<StreamPhase>,
    item: ActivityItem,
) -> (Uuid, bool) {
    for block in session.transcript_blocks.iter_mut().rev() {
        if let Some(activity) = block.activities.iter_mut().rev().find(|activity| {
            item.source_id
                .as_ref()
                .is_some_and(|id| activity.source_id.as_ref() == Some(id))
                || (item.source_id.is_none() && activity.title == item.title && !activity.complete)
        }) {
            let has_arguments = item.arguments.is_some();
            if item.compaction.is_some() {
                activity.compaction = item.compaction.clone();
            }
            activity.kind = item.kind;
            activity.title = item.title;
            activity.complete = item.complete;
            activity.failed = item.failed;
            if item.tool_name.is_some() {
                activity.tool_name = item.tool_name;
            }
            if item.mcp_server.is_some() {
                activity.mcp_server = item.mcp_server;
            }
            if item.detail.is_some() {
                activity.detail = item.detail;
            }
            if item.arguments.is_some() {
                activity.arguments = item.arguments;
            }
            if item.output.is_some() {
                activity.output = item.output;
            }
            if !item.image_urls.is_empty() {
                activity.image_urls = item.image_urls;
            }
            if !item.file_changes.is_empty() {
                activity.file_changes = item.file_changes;
            }
            if item.display_target.is_some() && (activity.display_target.is_none() || has_arguments)
            {
                activity.display_target = item.display_target;
            }
            if item.display_description.is_some()
                && (activity.display_description.is_none() || has_arguments)
            {
                activity.display_description = item.display_description;
            }
            if item.reasoning.is_some() {
                activity.reasoning = item.reasoning;
            }
            session.updated_at = unix_time();
            return (activity.id, false);
        }
    }
    // Updating an earlier tool is background progress, not a new transcript
    // boundary. Only a newly inserted activity can close legacy prose/thought.
    if previous_phase == Some(StreamPhase::Text) {
        finish_session_messages(session);
    }
    if previous_phase == Some(StreamPhase::Reasoning) {
        complete_session_reasoning(session);
    }
    let id = item.id;
    push_transcript_activity(
        session,
        item,
        matches!(
            previous_phase,
            Some(StreamPhase::Reasoning | StreamPhase::Activity)
        ),
    );
    session.updated_at = unix_time();
    (id, true)
}
