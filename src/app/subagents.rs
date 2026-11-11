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

/// A tool call the parent's model is blocked on.
struct PendingSubagentCall {
    parent: Uuid,
    request_id: String,
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
    /// composer's `@` list see them. Empty where the provider has no Raku
    /// `subagent` tool.
    pub(super) fn subagent_tool_agents(&self, provider: ProviderKind) -> Vec<SubagentToolAgent> {
        if provider != ProviderKind::Pi {
            return Vec::new();
        }
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
            };
        }
        if session.is_side_chat() {
            return SubagentLaunch::default();
        }
        SubagentLaunch {
            agents: self.subagent_tool_agents(session.provider),
            child: false,
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

    /// Start the child a `subagent` call asked for, or answer the call with
    /// the reason it cannot run.
    fn start_subagent(
        &mut self,
        parent_id: Uuid,
        request: SubagentRequest,
        cx: &mut Context<Self>,
    ) {
        let Some(parent) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == parent_id && !session.is_child())
        else {
            self.reject_subagent_call(
                parent_id,
                request,
                "Subagents cannot delegate to other subagents.".to_owned(),
            );
            return;
        };
        let agents = self.subagent_tool_agents(parent.provider);
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
        if request.prompt.is_empty() {
            self.reject_subagent_call(
                parent_id,
                request,
                "The subagent prompt is empty.".to_owned(),
            );
            return;
        }
        let live = self
            .subagents
            .pending
            .values()
            .filter(|call| call.parent == parent_id)
            .count();
        if live >= MAX_LIVE_SUBAGENTS {
            self.reject_subagent_call(
                parent_id,
                request,
                format!(
                    "{MAX_LIVE_SUBAGENTS} subagents are already running for this task. \
                     Wait for one to finish before starting another."
                ),
            );
            return;
        }

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
        let description = if request.description.is_empty() {
            agent.name.clone()
        } else {
            request.description.clone()
        };
        session.set_title(&description);
        session.subagent = Some(SubagentOrigin {
            parent_session_id: parent_id,
            call_id: request.call_id.clone(),
            agent: agent.name.clone(),
            description,
            read_only: agent.read_only,
            delivered: false,
        });
        let child_id = session.id;
        self.state.push_session(session);
        self.invalidate_subagent_index();
        self.subagents.pending.insert(
            child_id,
            PendingSubagentCall {
                parent: parent_id,
                request_id: request.request_id,
            },
        );
        self.submit_submission_for_session(
            child_id,
            ComposerSubmission {
                prompt: subagent_prompt(&agent.instructions, &request.prompt),
                display_content: Some(request.prompt),
                attachments: Vec::new(),
            },
            cx,
        );
        // The tool row gains its link to the child.
        if self.state.selected_session == Some(parent_id) {
            self.remeasure_transcript_tail();
        }
        cx.notify();
    }

    /// Start queued delegations and answer the calls whose child has
    /// settled. Cheap when nothing is pending, and safe to call from any
    /// path that changes a session's status.
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
            let Some(parent_id) = self
                .subagents
                .pending
                .get(&child_id)
                .map(|call| call.parent)
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
            if parent_waiting && child_busy {
                continue;
            }
            let Some(call) = self.subagents.pending.remove(&child_id) else {
                continue;
            };
            settled = true;
            if !parent_waiting {
                // The task stopped or failed; nothing is waiting for this
                // child any more, so it stops too.
                if child_busy {
                    self.cancel_session_turn(child_id, cx);
                }
            } else if let Some(child) = self
                .state
                .sessions
                .iter()
                .find(|session| session.id == child_id)
            {
                let (outcome, report) = subagent_report(child);
                self.answer_subagent_call(
                    call.parent,
                    call.request_id,
                    subagent_result(Some(child_id), outcome, &report),
                );
            } else {
                self.answer_subagent_call(
                    call.parent,
                    call.request_id,
                    subagent_result(
                        None,
                        SubagentOutcome::Cancelled,
                        "The subagent was deleted before it finished.",
                    ),
                );
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
    fn a_child_that_never_ran_reports_a_failure() {
        let parent = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        let child = child_of(&parent, "call-1");
        let (outcome, _) = subagent_report(&child);
        assert_eq!(outcome, SubagentOutcome::Failed);
    }
}
