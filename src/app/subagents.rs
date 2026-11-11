//! Subagents: child sessions a task's agent delegates work to.
//!
//! A provider's `subagent` tool call arrives as
//! `DriverEvent::SubagentRequested` and blocks until it is answered. The app
//! carries the call out the way it runs any session: it creates a hidden
//! child `AgentSession`, submits the delegated prompt, and when the child's
//! turn settles answers the call with the child's final message. See
//! [docs/subagents.md](../../docs/subagents.md).

use super::*;
use crate::model::{
    SubagentDefinition, SubagentLaunch, SubagentOrigin, SubagentOutcome, SubagentRequest,
    SubagentToolAgent, subagent_result,
};

/// Live subagents one task may have at once. A further call is rejected with
/// a result the delegating model can act on.
const MAX_LIVE_SUBAGENTS: usize = 4;

/// What a new subagent is asked to do, and who is waiting on it.
struct NewSubagent<'a> {
    description: &'a str,
    /// The parent's tool call id, linking the child to its transcript row.
    call_id: Option<String>,
    prompt: String,
    /// The blocked call to answer when the child settles; `None` launches it
    /// in the background.
    request_id: Option<String>,
}

/// A subagent still working for its task.
struct PendingSubagentCall {
    parent: Uuid,
    /// The tool call the parent's model is blocked on. `None` once nothing
    /// is: a background launch, `/agent`, or a call promoted by a steer. Its
    /// result is then delivered to the parent as a message.
    request_id: Option<String>,
}

#[derive(Debug, Default)]
struct SubagentIndex {
    /// Parent task → its subagents, oldest first.
    by_parent: HashMap<Uuid, Vec<Uuid>>,
    /// (parent task, tool call id) → the subagent that call launched.
    by_call: HashMap<(Uuid, String), Uuid>,
    /// Every subagent session, for the per-frame "is this tab a subagent".
    children: HashSet<Uuid>,
}

/// Everything the desktop keeps for subagents beyond their sessions.
pub(super) struct SubagentsState {
    /// Calls waiting on a child, keyed by that child's session.
    pending: HashMap<Uuid, PendingSubagentCall>,
    /// Calls seen while draining driver events, started once the drain has
    /// put every runtime back.
    requests: Vec<(Uuid, SubagentRequest)>,
    /// Parent and call lookups, rebuilt only when subagents change.
    index: RefCell<Option<Rc<SubagentIndex>>>,
    /// Settling a call can stop a session, which settles calls in turn.
    reconciling: bool,
    pub(super) settings: settings_page::SubagentSettingsState,
}

impl SubagentsState {
    pub(super) fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            pending: HashMap::new(),
            requests: Vec::new(),
            index: RefCell::new(None),
            reconciling: false,
            settings: settings_page::SubagentSettingsState::new(window, cx),
        }
    }

    pub(super) fn queue_request(&mut self, parent: Uuid, request: SubagentRequest) {
        self.requests.push((parent, request));
    }

    /// Whether a drain has anything to start or settle.
    pub(super) fn has_work(&self) -> bool {
        !self.requests.is_empty() || !self.pending.is_empty()
    }
}

fn build_subagent_index(sessions: &[AgentSession]) -> SubagentIndex {
    let mut children = sessions
        .iter()
        .filter_map(|session| Some((session, session.subagent.as_ref()?)))
        .collect::<Vec<_>>();
    children.sort_by_key(|(session, _)| session.created_at);
    let mut index = SubagentIndex::default();
    for (session, origin) in children {
        index.children.insert(session.id);
        index
            .by_parent
            .entry(origin.parent_session_id)
            .or_default()
            .push(session.id);
        if let Some(call_id) = &origin.call_id {
            index
                .by_call
                .insert((origin.parent_session_id, call_id.clone()), session.id);
        }
    }
    index
}

/// The prompt a fresh subagent receives: a line saying what it is, the
/// agent's own instructions, then the delegated task. The transcript shows
/// only the task.
fn subagent_prompt(instructions: &str, task: &str) -> String {
    let instructions = instructions.trim();
    let mut prompt = String::from(crate::model::SUBAGENT_PROMPT_PREFIX);
    if !instructions.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(instructions);
    }
    prompt.push('\n');
    prompt.push_str(crate::model::SUBAGENT_TASK_MARKER);
    prompt.push_str(task);
    prompt
}

/// Splits `/name task` into the name and a non-empty task.
fn parse_agent_command(prompt: &str) -> Option<(&str, &str)> {
    let rest = prompt.trim().strip_prefix('/')?;
    let (name, task) = rest.split_once(char::is_whitespace)?;
    let task = task.trim();
    (SubagentDefinition::is_valid_name(name) && !task.is_empty()).then_some((name, task))
}

/// How a settled child's last turn ended, and the text to report for it.
fn subagent_report(session: &AgentSession) -> (SubagentOutcome, String) {
    let Some(turn) = session.turns.last() else {
        return (
            SubagentOutcome::Failed,
            "The subagent could not be started.".to_owned(),
        );
    };
    let last_reply = session
        .messages
        .iter()
        .rev()
        .take_while(|message| message.role != MessageRole::User)
        .find(|message| {
            message.role == MessageRole::Assistant && !message.visible_content().trim().is_empty()
        });
    match turn.status {
        TurnStatus::Completed => match last_reply.filter(|message| message.error.is_none()) {
            Some(message) => (
                SubagentOutcome::Completed,
                message.visible_content().to_owned(),
            ),
            None => (
                SubagentOutcome::Failed,
                "The subagent finished without a report.".to_owned(),
            ),
        },
        TurnStatus::Interrupted | TurnStatus::Running => (
            SubagentOutcome::Cancelled,
            "The subagent was stopped before it finished.".to_owned(),
        ),
        TurnStatus::Failed => {
            let mut report = last_reply
                .map(|message| message.visible_content().to_owned())
                .unwrap_or_else(|| "The subagent failed.".to_owned());
            if let Some(detail) = last_reply
                .and_then(|message| message.error.as_ref())
                .and_then(|error| error.detail.as_deref())
            {
                report.push('\n');
                report.push_str(detail);
            }
            (SubagentOutcome::Failed, report)
        }
    }
}

impl Raku {
    pub(super) fn invalidate_subagent_index(&self) {
        self.subagents.index.borrow_mut().take();
    }

    fn subagent_index(&self) -> Rc<SubagentIndex> {
        self.subagents
            .index
            .borrow_mut()
            .get_or_insert_with(|| Rc::new(build_subagent_index(&self.state.sessions)))
            .clone()
    }

    /// Subagents `parent` delegated to, oldest first.
    pub(super) fn subagent_ids_for(&self, parent: Uuid) -> Vec<Uuid> {
        self.subagent_index()
            .by_parent
            .get(&parent)
            .cloned()
            .unwrap_or_default()
    }

    /// The subagent a `subagent` tool row launched, once it exists.
    pub(super) fn subagent_for_call(&self, parent: Uuid, call_id: &str) -> Option<Uuid> {
        self.subagent_index()
            .by_call
            .get(&(parent, call_id.to_owned()))
            .copied()
    }

    /// Subagents of `parent` still working. Reads only the pending-call map,
    /// so the header badge can ask on every frame.
    pub(super) fn live_subagent_count(&self, parent: Uuid) -> usize {
        self.subagents
            .pending
            .values()
            .filter(|call| call.parent == parent)
            .count()
    }

    /// The newest subagents of `parent`, newest first, each with its state
    /// in the background-work vocabulary. The work menu renders this on
    /// every frame it is open: one pass over the sessions, no lookups.
    pub(super) fn subagent_work_statuses(
        &self,
        parent: Uuid,
        limit: usize,
    ) -> Vec<(&AgentSession, BackgroundWorkStatus)> {
        let mut children = self
            .state
            .sessions
            .iter()
            .filter(|session| {
                session
                    .subagent
                    .as_ref()
                    .is_some_and(|origin| origin.parent_session_id == parent)
            })
            .collect::<Vec<_>>();
        children.sort_by_key(|session| std::cmp::Reverse(session.created_at));
        children.truncate(limit);
        children
            .into_iter()
            .map(|session| {
                let status = if self.submission_preparations.contains(&session.id) {
                    BackgroundWorkStatus::Starting
                } else if session.is_busy() {
                    BackgroundWorkStatus::Running
                } else {
                    match session.turns.last().map(|turn| turn.status) {
                        Some(TurnStatus::Completed) => BackgroundWorkStatus::Completed,
                        Some(TurnStatus::Interrupted) => BackgroundWorkStatus::Stopped,
                        Some(TurnStatus::Running) => BackgroundWorkStatus::Lost,
                        // A skeleton has not loaded its turns; it ran before.
                        None if !session.detail_loaded => BackgroundWorkStatus::Completed,
                        Some(TurnStatus::Failed) | None => BackgroundWorkStatus::Failed,
                    }
                };
                (session, status)
            })
            .collect()
    }

    /// A runtime learns its agents when it starts — Pi's extension and Claude
    /// Code's tool list at launch, a resumed Codex thread from its first
    /// prompt's note. After an agent edit, release every idle task runtime,
    /// by the same rule the idle sweep uses, so the next prompt starts one
    /// with the current list. Busy tasks keep theirs until they are idle.
    pub(super) fn release_runtimes_for_agent_change(&mut self) {
        let stale = self
            .runtimes
            .keys()
            .copied()
            .filter(|session_id| {
                let session = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == *session_id);
                session.is_some_and(|session| !session.is_child())
                    && !self.submission_preparations.contains(session_id)
                    && !self.goal_runtime_starts.contains(session_id)
                    && self.live_subagent_count(*session_id) == 0
                    && session_is_reapable(
                        session,
                        Duration::MAX,
                        self.session_has_live_background_work(*session_id),
                    )
            })
            .collect::<Vec<_>>();
        for session_id in stale {
            if let Some(runtime) = self.runtimes.remove(&session_id) {
                runtime.driver.close();
            }
        }
    }

    /// Whether `child` is still working for its task. One map lookup, safe
    /// on a frame.
    pub(super) fn subagent_is_working(&self, child: Uuid) -> bool {
        self.subagents.pending.contains_key(&child)
    }

    pub(super) fn is_subagent_session(&self, id: Uuid) -> bool {
        self.subagent_index().children.contains(&id)
    }

    /// A deleted session takes its place in any delegation with it.
    pub(super) fn forget_subagent_session(&mut self, id: Uuid) {
        self.subagents.pending.remove(&id);
        self.subagents.pending.retain(|_, call| call.parent != id);
        self.subagents.requests.retain(|(parent, _)| *parent != id);
        self.invalidate_subagent_index();
    }

    fn provider_is_available(&self, provider: ProviderKind) -> bool {
        !self.state.disabled_providers.contains(&provider)
            && self
                .provider_probe(provider)
                .is_some_and(|probe| probe.path.is_some())
    }

    /// Whether `agent` can run for a task on `task_provider` right now.
    pub(super) fn subagent_is_available(
        &self,
        agent: &SubagentDefinition,
        task_provider: ProviderKind,
    ) -> bool {
        self.provider_is_available(agent.provider.unwrap_or(task_provider))
    }

    /// The agents a task on `provider` may delegate to, as its model and the
    /// composer's `@` list see them. Every provider carries Raku's `subagent`
    /// tool: Pi through its extension, Claude Code through an in-process MCP
    /// server, Codex as a dynamic tool.
    pub(super) fn subagent_tool_agents(&self, provider: ProviderKind) -> Vec<SubagentToolAgent> {
        self.state
            .subagents
            .iter()
            .filter(|agent| agent.enabled && self.subagent_is_available(agent, provider))
            .map(|agent| SubagentToolAgent {
                name: agent.name.clone(),
                description: agent.description.clone(),
            })
            .collect()
    }

    /// A session's part in delegation, decided when its runtime starts.
    pub(super) fn subagent_launch_for(&self, session: &AgentSession) -> SubagentLaunch {
        if session.is_subagent() {
            return SubagentLaunch {
                agents: Vec::new(),
                child: true,
                delegates: false,
            };
        }
        if session.is_side_chat() {
            return SubagentLaunch::default();
        }
        SubagentLaunch {
            agents: self.subagent_tool_agents(session.provider),
            child: false,
            delegates: true,
        }
    }

    fn answer_subagent_call(&self, parent: Uuid, request_id: String, result: String) {
        if let Some(runtime) = self.runtimes.get(&parent) {
            runtime.driver.respond_subagent(request_id, result);
        }
    }

    fn reject_subagent_call(&self, parent: Uuid, request: SubagentRequest, reason: String) {
        self.answer_subagent_call(
            parent,
            request.request_id,
            subagent_result(None, SubagentOutcome::Rejected, &reason),
        );
    }

    /// The answer to a call whose subagent keeps working after it returns.
    fn running_result(child: Uuid) -> String {
        subagent_result(
            Some(child),
            SubagentOutcome::Running,
            "The subagent is working in the background. Its report will arrive as a \
             message when it finishes. Do not sleep, poll, or duplicate its work.",
        )
    }

    /// Start a fresh subagent session for `parent_id` and submit its task.
    /// The caller has already checked the agent and the live-subagent limit.
    fn launch_subagent(
        &mut self,
        parent_id: Uuid,
        agent: &SubagentDefinition,
        launch: NewSubagent<'_>,
        cx: &mut Context<Self>,
    ) -> Option<Uuid> {
        let NewSubagent {
            description,
            call_id,
            prompt,
            request_id: call,
        } = launch;
        let parent = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == parent_id && !session.is_child())?;
        let provider = agent.provider.unwrap_or(parent.provider);
        let same_provider = provider == parent.provider;
        let mut session = AgentSession::new(parent.project_id, provider);
        session.workspace = match &parent.workspace {
            // A subagent works where its task works and never creates a
            // worktree of its own.
            SessionWorkspace::NewWorktree { .. } => SessionWorkspace::Local,
            workspace => workspace.clone(),
        };
        session.model = agent
            .model
            .clone()
            .or_else(|| same_provider.then(|| parent.model.clone()).flatten());
        session.reasoning_effort = agent.reasoning_effort.clone().or_else(|| {
            (same_provider && agent.model.is_none())
                .then(|| parent.reasoning_effort.clone())
                .flatten()
        });
        session.runtime_mode = if agent.read_only {
            side_chat::side_chat_runtime_mode(provider)
        } else if provider.validate_access_mode(parent.runtime_mode).is_ok() {
            parent.runtime_mode
        } else {
            // The agent's provider cannot run under the task's access mode.
            // Fall back to its narrowest supported one rather than a wider.
            side_chat::side_chat_runtime_mode(provider)
        };
        let description = if description.is_empty() {
            agent.name.clone()
        } else {
            description.to_owned()
        };
        session.set_title(&description);
        session.subagent = Some(SubagentOrigin {
            parent_session_id: parent_id,
            call_id,
            agent: agent.name.clone(),
            description,
            read_only: agent.read_only,
            background: call.is_none(),
            delivered: false,
        });
        let child_id = session.id;
        self.state.push_session(session);
        self.invalidate_subagent_index();
        self.subagents.pending.insert(
            child_id,
            PendingSubagentCall {
                parent: parent_id,
                request_id: call,
            },
        );
        self.submit_submission_for_session(
            child_id,
            ComposerSubmission {
                prompt: subagent_prompt(&agent.instructions, &prompt),
                display_content: Some(prompt),
                attachments: Vec::new(),
            },
            cx,
        );
        // The tool row gains its link to the child.
        if self.state.selected_session == Some(parent_id) {
            self.remeasure_transcript_tail();
        }
        cx.notify();
        Some(child_id)
    }

    fn live_subagents_for(&self, parent: Uuid) -> usize {
        self.live_subagent_count(parent)
    }

    fn too_many_subagents(&self) -> String {
        format!(
            "{MAX_LIVE_SUBAGENTS} subagents are already running for this task. \
             Wait for one to finish before starting another."
        )
    }

    /// Carry out a `subagent` call: start or continue a child, or answer the
    /// call with the reason it cannot run.
    fn start_subagent(
        &mut self,
        parent_id: Uuid,
        request: SubagentRequest,
        cx: &mut Context<Self>,
    ) {
        let Some(parent_provider) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == parent_id && !session.is_child())
            .map(|session| session.provider)
        else {
            self.reject_subagent_call(
                parent_id,
                request,
                "Subagents cannot delegate to other subagents.".to_owned(),
            );
            return;
        };
        if request.prompt.is_empty() {
            self.reject_subagent_call(
                parent_id,
                request,
                "The subagent prompt is empty.".to_owned(),
            );
            return;
        }
        if let Some(session_id) = request.session_id.clone() {
            self.continue_subagent(parent_id, &session_id, request, cx);
            return;
        }
        let agents = self.subagent_tool_agents(parent_provider);
        let Some(agent) = self
            .state
            .subagents
            .iter()
            .find(|agent| {
                agent.name == request.agent && agents.iter().any(|tool| tool.name == agent.name)
            })
            .cloned()
        else {
            let available = agents
                .iter()
                .map(|agent| agent.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            self.reject_subagent_call(
                parent_id,
                request.clone(),
                format!(
                    "There is no agent named \"{}\". Available agents: {available}.",
                    request.agent
                ),
            );
            return;
        };
        if self.live_subagents_for(parent_id) >= MAX_LIVE_SUBAGENTS {
            let reason = self.too_many_subagents();
            self.reject_subagent_call(parent_id, request, reason);
            return;
        }
        let background = request.background;
        let request_id = request.request_id.clone();
        let Some(child) = self.launch_subagent(
            parent_id,
            &agent,
            NewSubagent {
                description: &request.description,
                call_id: request.call_id,
                prompt: request.prompt,
                request_id: (!background).then(|| request_id.clone()),
            },
            cx,
        ) else {
            return;
        };
        if background {
            self.answer_subagent_call(parent_id, request_id, Self::running_result(child));
        }
    }

    /// Send an existing subagent a follow-up. An idle child takes it as a new
    /// turn, tracked like a fresh call; a running background child is
    /// steered, and its pending report covers both.
    fn continue_subagent(
        &mut self,
        parent_id: Uuid,
        session_id: &str,
        request: SubagentRequest,
        cx: &mut Context<Self>,
    ) {
        let Some(child) = Uuid::parse_str(session_id.trim())
            .ok()
            .filter(|id| self.subagent_ids_for(parent_id).contains(id))
        else {
            self.reject_subagent_call(
                parent_id,
                request,
                format!("There is no subagent of this task with session_id \"{session_id}\"."),
            );
            return;
        };
        let busy = self.submission_preparations.contains(&child)
            || self
                .state
                .sessions
                .iter()
                .any(|session| session.id == child && session.is_busy());
        if busy {
            let background = self
                .subagents
                .pending
                .get(&child)
                .is_some_and(|call| call.request_id.is_none());
            let can_steer = self
                .state
                .sessions
                .iter()
                .find(|session| session.id == child)
                .is_some_and(|session| self.session_can_steer(session));
            if background && can_steer {
                if let Some(runtime) = self.runtimes.get_mut(&child) {
                    runtime.driver.steer(request.prompt.clone());
                    runtime
                        .pending_steers
                        .push_back(ComposerSubmission::plain(request.prompt));
                }
                self.answer_subagent_call(
                    parent_id,
                    request.request_id,
                    Self::running_result(child),
                );
            } else {
                self.reject_subagent_call(
                    parent_id,
                    request,
                    "That subagent is still working. Its result will arrive when it finishes."
                        .to_owned(),
                );
            }
            return;
        }
        if self.live_subagents_for(parent_id) >= MAX_LIVE_SUBAGENTS {
            let reason = self.too_many_subagents();
            self.reject_subagent_call(parent_id, request, reason);
            return;
        }
        let background = request.background;
        if let Some(origin) = self
            .state
            .session_mut(child)
            .and_then(|session| session.subagent.as_mut())
        {
            origin.background = background;
            origin.delivered = false;
        }
        self.subagents.pending.insert(
            child,
            PendingSubagentCall {
                parent: parent_id,
                request_id: (!background).then(|| request.request_id.clone()),
            },
        );
        self.submit_submission_for_session(child, ComposerSubmission::plain(request.prompt), cx);
        if background {
            self.answer_subagent_call(parent_id, request.request_id, Self::running_result(child));
        }
        cx.notify();
    }

    /// `/agent <task>`: start a subagent for the selected task without a
    /// model turn. Its report reaches the task as a message, so this works
    /// for every provider. Returns whether the composer text was consumed.
    pub(super) fn launch_agent_command(
        &mut self,
        agent_name: &str,
        task: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(parent) = self
            .selected_session()
            .filter(|session| !session.is_child())
            .map(|session| (session.id, session.provider))
        else {
            return false;
        };
        let Some(agent) = self
            .state
            .subagents
            .iter()
            .find(|agent| {
                agent.name == agent_name
                    && agent.enabled
                    && self.subagent_is_available(agent, parent.1)
            })
            .cloned()
        else {
            return false;
        };
        if self.live_subagents_for(parent.0) >= MAX_LIVE_SUBAGENTS {
            self.show_toast(tr!("subagent.too_many", count = MAX_LIVE_SUBAGENTS));
            return true;
        }
        let description = single_line_label(task).chars().take(60).collect::<String>();
        self.launch_subagent(
            parent.0,
            &agent,
            NewSubagent {
                description: &description,
                call_id: None,
                prompt: task.to_owned(),
                request_id: None,
            },
            cx,
        );
        true
    }

    /// Agents `/name <task>` can launch for the selected task: every enabled,
    /// available agent, whatever the task's provider, since the app runs the
    /// child itself. A command of the same name already in `commands` wins.
    pub(super) fn agent_slash_commands(
        &self,
        commands: &[crate::composer_complete::SlashCommand],
    ) -> Vec<crate::composer_complete::SlashCommand> {
        let Some(provider) = self
            .selected_session()
            .filter(|session| !session.is_child())
            .map(|session| session.provider)
        else {
            return Vec::new();
        };
        self.state
            .subagents
            .iter()
            .filter(|agent| agent.enabled && self.subagent_is_available(agent, provider))
            .filter(|agent| !commands.iter().any(|command| command.name == agent.name))
            .map(|agent| crate::composer_complete::SlashCommand {
                name: agent.name.clone(),
                description: agent.description.clone(),
                scope: crate::composer_complete::CommandScope::Raku,
                argument_hint: Some(tr!("subagent.command_hint")),
                template: None,
            })
            .collect()
    }

    /// `/name <task>` naming an agent in the slash index runs it in the
    /// background. A bare `/name` is left for the user to finish.
    pub(super) fn execute_agent_command(&mut self, prompt: &str, cx: &mut Context<Self>) -> bool {
        let Some((name, task)) = parse_agent_command(prompt) else {
            return false;
        };
        let is_agent = self.slash_command_index.iter().any(|command| {
            command.name == name && command.scope == crate::composer_complete::CommandScope::Raku
        }) && self.state.subagents.iter().any(|agent| agent.name == name);
        if !is_agent || !self.launch_agent_command(name, task, cx) {
            return false;
        }
        self.composer.update(cx, |input, cx| input.clear(cx));
        true
    }

    /// The user is talking to `parent` while its model waits on subagents.
    /// Answer those calls as running so the model can respond now; the
    /// reports arrive later as messages.
    pub(super) fn promote_subagent_calls(&mut self, parent: Uuid) {
        let promoted = self
            .subagents
            .pending
            .iter_mut()
            .filter(|(_, call)| call.parent == parent && call.request_id.is_some())
            .filter_map(|(child, call)| Some((*child, call.request_id.take()?)))
            .collect::<Vec<_>>();
        for (child, request_id) in promoted {
            if let Some(origin) = self
                .state
                .session_mut(child)
                .and_then(|session| session.subagent.as_mut())
            {
                origin.background = true;
            }
            self.answer_subagent_call(parent, request_id, Self::running_result(child));
        }
    }

    /// Whether `parent`'s model is blocked on a subagent call right now.
    pub(super) fn has_blocking_subagent_calls(&self, parent: Uuid) -> bool {
        self.subagents
            .pending
            .values()
            .any(|call| call.parent == parent && call.request_id.is_some())
    }

    /// Hand a background subagent's report to its task: steered into a
    /// running turn when the runtime can, otherwise sent as the next turn,
    /// which waits in the queue while the task is busy. The message shows as
    /// a compact notice, not a user bubble.
    fn deliver_subagent_report(
        &mut self,
        parent: Uuid,
        child: Uuid,
        outcome: SubagentOutcome,
        report: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(label) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == child)
            .and_then(|session| session.subagent.as_ref())
            .map(|origin| format!("{} · {}", origin.agent, origin.description))
        else {
            return;
        };
        let submission = ComposerSubmission {
            prompt: subagent_result(Some(child), outcome, report),
            display_content: Some(label),
            attachments: Vec::new(),
        };
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == parent)
        else {
            return;
        };
        if session.is_busy() && self.session_can_steer(session) {
            if let Some(runtime) = self.runtimes.get_mut(&parent) {
                runtime.driver.steer(submission.prompt.clone());
                runtime.pending_steers.push_back(submission);
            }
        } else {
            self.submit_submission_for_session(parent, submission, cx);
        }
    }

    /// Start queued delegations and settle the subagents that finished.
    /// Cheap when nothing is pending, and safe to call from any path that
    /// changes a session's status.
    pub(super) fn reconcile_subagents(&mut self, cx: &mut Context<Self>) {
        if self.subagents.reconciling || !self.subagents.has_work() {
            return;
        }
        self.subagents.reconciling = true;
        for (parent, request) in std::mem::take(&mut self.subagents.requests) {
            self.start_subagent(parent, request, cx);
        }
        let children = self.subagents.pending.keys().copied().collect::<Vec<_>>();
        let mut settled = false;
        for child_id in children {
            let Some((parent_id, blocking)) = self
                .subagents
                .pending
                .get(&child_id)
                .map(|call| (call.parent, call.request_id.is_some()))
            else {
                continue;
            };
            let parent_waiting = self
                .state
                .sessions
                .iter()
                .any(|session| session.id == parent_id && session.is_busy());
            let child_busy = self.submission_preparations.contains(&child_id)
                || self
                    .state
                    .sessions
                    .iter()
                    .any(|session| session.id == child_id && session.is_busy());
            // A blocking call waits while both run; a background child only
            // needs to finish, whatever its task is doing.
            if child_busy && (parent_waiting || !blocking) {
                continue;
            }
            let Some(call) = self.subagents.pending.remove(&child_id) else {
                continue;
            };
            settled = true;
            let child = self
                .state
                .sessions
                .iter()
                .find(|session| session.id == child_id);
            match (call.request_id, child) {
                (Some(_), _) if !parent_waiting => {
                    // The task stopped or failed; nothing is waiting for this
                    // child any more, so it stops too.
                    if child_busy {
                        self.cancel_session_turn(child_id, cx);
                    }
                }
                (Some(request_id), Some(child)) => {
                    let (outcome, report) = subagent_report(child);
                    self.answer_subagent_call(
                        call.parent,
                        request_id,
                        subagent_result(Some(child_id), outcome, &report),
                    );
                }
                (Some(request_id), None) => {
                    self.answer_subagent_call(
                        call.parent,
                        request_id,
                        subagent_result(
                            None,
                            SubagentOutcome::Cancelled,
                            "The subagent was deleted before it finished.",
                        ),
                    );
                }
                (None, Some(child)) => {
                    let (outcome, report) = subagent_report(child);
                    self.deliver_subagent_report(call.parent, child_id, outcome, &report, cx);
                }
                (None, None) => {}
            }
            if let Some(origin) = self
                .state
                .session_mut(child_id)
                .and_then(|session| session.subagent.as_mut())
            {
                origin.delivered = true;
            }
        }
        self.subagents.reconciling = false;
        if settled {
            self.save();
            cx.notify();
        }
    }
}

pub(super) mod settings_page;

#[cfg(test)]
mod tests {
    use super::*;

    fn child_of(parent: &AgentSession, call_id: &str) -> AgentSession {
        let mut child = AgentSession::new(parent.project_id, ProviderKind::Pi);
        child.subagent = Some(SubagentOrigin {
            parent_session_id: parent.id,
            call_id: Some(call_id.to_owned()),
            agent: "explore".into(),
            description: "Find callers".into(),
            read_only: true,
            background: false,
            delivered: false,
        });
        child
    }

    #[test]
    fn subagents_index_by_parent_and_call_and_stay_out_of_task_lists() {
        let parent = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        let mut child = child_of(&parent, "call-1");
        child.push_message(MessageRole::User, "find callers");
        let index = build_subagent_index(&[parent.clone(), child.clone()]);
        assert_eq!(index.by_parent.get(&parent.id), Some(&vec![child.id]));
        assert_eq!(
            index.by_call.get(&(parent.id, "call-1".to_owned())),
            Some(&child.id)
        );
        assert!(child.is_child());
        assert!(!child.is_listed_task());
        assert!(!child.is_task_draft());
        assert_eq!(child.parent_session(), Some(parent.id));
    }

    #[test]
    fn the_first_prompt_hides_everything_but_the_task() {
        let prompt = subagent_prompt("Be terse.", "Find every caller of foo.");
        assert!(prompt.starts_with(crate::model::SUBAGENT_PROMPT_PREFIX));
        assert!(prompt.contains("Be terse."));
        assert_eq!(
            crate::model::subagent_task(&prompt),
            "Find every caller of foo."
        );
        let bare = subagent_prompt("  ", "Do it.");
        assert_eq!(crate::model::subagent_task(&bare), "Do it.");
    }

    #[test]
    fn a_background_answer_names_the_child_and_parses_as_running() {
        let child = Uuid::new_v4();
        let text = Raku::running_result(child);
        let parsed = crate::model::parse_subagent_result(&text).expect("a result envelope");
        assert_eq!(parsed.outcome, SubagentOutcome::Running);
        assert_eq!(parsed.session_id, Some(child));
    }

    #[test]
    fn agent_commands_need_a_name_and_a_task() {
        assert_eq!(
            parse_agent_command("  /explore find the parser \n"),
            Some(("explore", "find the parser"))
        );
        assert_eq!(parse_agent_command("/explore"), None);
        assert_eq!(parse_agent_command("/explore   "), None);
        assert_eq!(parse_agent_command("explore find"), None);
        assert_eq!(parse_agent_command("/Explore find"), None);
    }

    #[test]
    fn a_child_that_never_ran_reports_a_failure() {
        let parent = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        let child = child_of(&parent, "call-1");
        let (outcome, _) = subagent_report(&child);
        assert_eq!(outcome, SubagentOutcome::Failed);
    }
}
