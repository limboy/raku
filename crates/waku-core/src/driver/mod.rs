//! Local provider runtime owned by `waku-daemon`.

mod activity;
mod claude;
mod codex;
mod compaction;
mod pi;
mod support;
mod title_language;
mod title_refresh;

pub(crate) use title_language::hide_title_language_context;

use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::{Receiver, SendError, Sender, unbounded};
use waku_protocol::capabilities::RuntimeCapabilities;

use crate::model::{
    BackgroundWorkKey, DriverEvent, GoalOperation, PlanDecision, ProviderKind,
    ProviderResumeCursor, RuntimeMode, UserInputAnswer,
};

/// Appended to every provider's system instructions so agents know they can
/// hand the user a runnable command. Waku renders `sh run` fences with a Run
/// button and reports the result as the user's next message.
pub(crate) const USER_RUN_INSTRUCTIONS: &str = concat!(
    "You are running inside Waku, a desktop client that can run shell commands in the user's own ",
    "terminal. When the user should run a command on their machine themselves rather than you ",
    "running it with your own tools (for example it touches their private data, needs their ",
    "credentials or a password prompt, or they asked for a command to run), put it in a fenced ",
    "code block whose info string is `sh run` (open the fence with ```sh run). Waku shows a Run ",
    "button on that block. When the user clicks it, Waku runs the block in an interactive terminal ",
    "with their login shell in the session's working directory, and when it exits sends you the ",
    "command, its exit code, and the terminal output so you can continue. Only mark self-contained ",
    "commands that are safe to run as written. Use a plain ```sh or ```bash fence for examples, ",
    "explanations, and commands you intend to run yourself. Never mark destructive commands ",
    "without stating clearly what they will do."
);

/// System instructions for a read-only side chat. The transport also removes
/// every tool that could modify the project; this tells the model why.
pub(crate) const SIDE_CHAT_INSTRUCTIONS: &str = concat!(
    "You are a side-chat assistant inside Waku. The user is asking a side question about a ",
    "conversation they are having with a separate coding agent; you do not share its session. ",
    "Their first message gives you that conversation. Treat it as context you both already ",
    "share: answer from it directly, and refer to it naturally rather than describing it. ",
    "When the question needs more than the conversation contains, search: read and search ",
    "project files, or search and fetch the web when your tools allow it. You cannot modify files, run ",
    "commands, or take actions; if the user asks for a change, explain what to change instead. ",
    "Answer concisely."
);

/// Provider events remain synchronous to send from reader threads, while the
/// bounded wake channel lets the UI sleep until at least one event is ready.
/// Multiple provider writes coalesce into one wake without ever blocking the
/// provider or dropping the events themselves.
#[derive(Clone)]
pub struct DriverEventSender {
    events: Sender<DriverEvent>,
    wake: smol::channel::Sender<()>,
}

impl DriverEventSender {
    pub fn send(&self, event: DriverEvent) -> Result<(), SendError<DriverEvent>> {
        self.events.send(event)?;
        let _ = self.wake.try_send(());
        Ok(())
    }
}

pub(crate) trait DriverEventSink {
    fn send(&self, event: DriverEvent) -> Result<(), SendError<DriverEvent>>;
}

impl DriverEventSink for DriverEventSender {
    fn send(&self, event: DriverEvent) -> Result<(), SendError<DriverEvent>> {
        DriverEventSender::send(self, event)
    }
}

#[cfg(test)]
impl DriverEventSink for Sender<DriverEvent> {
    fn send(&self, event: DriverEvent) -> Result<(), SendError<DriverEvent>> {
        Sender::send(self, event)
    }
}

pub fn event_channel(
    wake: smol::channel::Sender<()>,
) -> (DriverEventSender, Receiver<DriverEvent>) {
    let (events, receiver) = unbounded();
    (DriverEventSender { events, wake }, receiver)
}

#[cfg(test)]
pub(crate) fn test_event_channel() -> (DriverEventSender, Receiver<DriverEvent>) {
    let (wake, _wakes) = smol::channel::bounded(1);
    event_channel(wake)
}

#[derive(Clone)]
pub struct DriverHandle {
    inner: Arc<dyn DriverControl>,
    title_language: Arc<title_language::TitleLanguageContext>,
}

impl DriverHandle {
    pub fn from_control(control: Arc<dyn DriverControl>) -> Self {
        Self {
            inner: control,
            title_language: Arc::default(),
        }
    }

    pub fn provider(&self) -> ProviderKind {
        self.inner.provider()
    }

    pub fn compact(&self, request: waku_protocol::compaction::CompactionRequest) {
        self.inner.compact(request);
    }

    pub fn prompt(&self, prompt: String) {
        self.inner.prompt(self.title_language.prepare(prompt));
    }

    /// Support reported by this live transport, without I/O or locking.
    pub fn capabilities(&self) -> RuntimeCapabilities {
        self.inner.capabilities()
    }

    pub fn steer(&self, prompt: String) {
        self.inner.steer(prompt);
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }

    pub fn refresh_background_work(&self) {
        self.inner.refresh_background_work();
    }

    pub fn stop_background_work(&self, key: BackgroundWorkKey, control_id: String) {
        self.inner.stop_background_work(key, control_id);
    }

    pub fn respond(&self, request_id: String, option_id: String) {
        self.inner.respond(request_id, option_id);
    }

    pub fn respond_user_input(&self, request_id: String, answers: Vec<UserInputAnswer>) {
        self.inner.respond_user_input(request_id, answers);
    }

    pub fn respond_plan(&self, request_id: String, decision: PlanDecision) {
        self.inner.respond_plan(request_id, decision);
    }

    /// Read or mutate the provider-persisted thread goal. Outcomes arrive
    /// asynchronously as `DriverEvent::GoalUpdated` or `DriverEvent::Error`.
    pub fn goal(&self, operation: GoalOperation) {
        self.inner.goal(operation);
    }

    pub fn apply_options(&self, options: SessionOptions) -> bool {
        self.inner.apply_options(options)
    }

    pub fn rollback(&self, turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        self.inner.rollback(turns)
    }

    pub fn fork(&self, turns_to_remove: usize) -> anyhow::Result<ProviderResumeCursor> {
        self.inner.fork(turns_to_remove)
    }
}

pub trait DriverControl: Send + Sync {
    fn provider(&self) -> ProviderKind;
    fn compact(&self, _request: waku_protocol::compaction::CompactionRequest) {}
    fn prompt(&self, prompt: String);
    fn capabilities(&self) -> RuntimeCapabilities;
    /// Deliver a steering message to the running turn. Implementations report
    /// the outcome asynchronously through `DriverEvent::SteerAccepted` or
    /// `DriverEvent::SteerRejected`.
    fn steer(&self, _prompt: String) {}
    fn cancel(&self);
    fn refresh_background_work(&self) {}
    fn stop_background_work(&self, _key: BackgroundWorkKey, _control_id: String) {}
    fn respond(&self, request_id: String, option_id: String);
    fn respond_user_input(&self, _request_id: String, _answers: Vec<UserInputAnswer>) {}
    /// Answer a plan the provider is blocked on. Only transports that emit
    /// `PlanProposed` with a request id implement this.
    fn respond_plan(&self, _request_id: String, _decision: PlanDecision) {}
    /// Providers without persisted goals ignore the request; the UI only
    /// offers goal controls where the provider reports one.
    fn goal(&self, _operation: GoalOperation) {}
    /// Applies changed turn options to the live session, returning whether the
    /// transport could do it without being restarted. A `false` answer is the
    /// driver asking to be torn down and recreated with the new options.
    fn apply_options(&self, _options: SessionOptions) -> bool {
        false
    }
    fn rollback(&self, turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>>;
    fn fork(&self, _turns_to_remove: usize) -> anyhow::Result<ProviderResumeCursor> {
        anyhow::bail!("conversation forking is not supported by this provider transport")
    }
}

pub struct DriverStartOptions {
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub mode: RuntimeMode,
    /// Start in the provider's native plan mode. See `AgentSession::plan_mode`.
    pub plan_mode: bool,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub context_window: Option<String>,
    pub agent_preset: Option<String>,
    pub provider_cursor: Option<ProviderResumeCursor>,
    /// Side-chat execution policy: the agent may read and search the project
    /// but has no tools that modify files, run commands, or reach MCP servers.
    pub read_only: bool,
}

/// The subset of `DriverStartOptions` a user can change without starting a new
/// task. Transports that carry these per turn can absorb a change in place;
/// the rest have to be restarted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOptions {
    pub mode: RuntimeMode,
    pub plan_mode: bool,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub context_window: Option<String>,
}

pub(crate) fn start_local(
    provider: ProviderKind,
    options: DriverStartOptions,
    events: DriverEventSender,
) -> anyhow::Result<DriverHandle> {
    provider.validate_access_mode(options.mode)?;
    let title_language = Arc::new(title_language::TitleLanguageContext::new(provider));
    let inner: Arc<dyn DriverControl> = match provider {
        ProviderKind::Codex => Arc::new(codex::CodexDriver::start(options, events)?),
        ProviderKind::Pi => Arc::new(pi::PiDriver::start(options, events)?),
        // Claude serves a realtime stream of user messages on stdin — the same
        // transport the Agent SDK drives — which is what lets its Supervised
        // mode ask rather than decide alone.
        ProviderKind::Claude => Arc::new(claude::ClaudeDriver::start(options, events)?),
    };
    Ok(DriverHandle {
        inner,
        title_language,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_access_modes_fail_before_opening_a_transport() {
        for mode in [RuntimeMode::Ask, RuntimeMode::Auto] {
            let (wake, _) = smol::channel::bounded(1);
            let (events, _) = event_channel(wake);
            let result = start_local(
                ProviderKind::Pi,
                DriverStartOptions {
                    binary: "/missing/waku-capability-test-provider".into(),
                    cwd: "/missing/waku-capability-test-workspace".into(),
                    mode,
                    plan_mode: false,
                    model: None,
                    reasoning_effort: None,
                    service_tier: None,
                    context_window: None,
                    agent_preset: None,
                    provider_cursor: None,
                    read_only: false,
                },
                events,
            );
            let error = result
                .err()
                .expect("unsupported start must fail")
                .downcast::<waku_protocol::capabilities::UnsupportedAccessMode>()
                .unwrap();
            assert_eq!(error.provider, ProviderKind::Pi);
            assert_eq!(error.mode, mode);
        }
    }

    #[test]
    fn provider_events_coalesce_wakes_without_dropping_payloads() {
        let (wake, wakes) = smol::channel::bounded(1);
        let (events, received) = event_channel(wake);

        events.send(DriverEvent::TextDelta("one".into())).unwrap();
        events.send(DriverEvent::TextDelta("two".into())).unwrap();

        assert_eq!(wakes.try_recv(), Ok(()));
        assert!(matches!(
            wakes.try_recv(),
            Err(smol::channel::TryRecvError::Empty)
        ));
        assert!(matches!(received.try_recv(), Ok(DriverEvent::TextDelta(text)) if text == "one"));
        assert!(matches!(received.try_recv(), Ok(DriverEvent::TextDelta(text)) if text == "two"));
    }
}
