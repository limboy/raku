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
    BackgroundWorkKey, DriverEvent, ExecutionPolicy, GoalOperation, ProviderKind,
    ProviderResumeCursor, RuntimeMode, UserInputAnswer,
};

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
    pub execution_policy: ExecutionPolicy,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub context_window: Option<String>,
    pub agent_preset: Option<String>,
    pub provider_cursor: Option<ProviderResumeCursor>,
    pub delegation_session_path: Option<PathBuf>,
    pub delegation_registry_dir: Option<PathBuf>,
}

/// The subset of `DriverStartOptions` a user can change without starting a new
/// task. Transports that carry these per turn can absorb a change in place;
/// the rest have to be restarted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOptions {
    pub mode: RuntimeMode,
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
    validate_start(provider, options.mode, options.execution_policy)?;
    let title_language = Arc::new(title_language::TitleLanguageContext::new(
        provider,
        options.execution_policy,
    ));
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

/// Validate before starting a transport or replacing a running session.
pub(crate) fn validate_start(
    provider: ProviderKind,
    mode: RuntimeMode,
    policy: ExecutionPolicy,
) -> anyhow::Result<()> {
    match policy {
        ExecutionPolicy::Interactive => provider.validate_access_mode(mode)?,
        ExecutionPolicy::ReviewReadOnly => anyhow::ensure!(
            provider.capabilities().delegation.review_subtask,
            "{} does not support read-only review subtasks",
            provider.display_name()
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incompatible_execution_policies_fail_before_opening_a_transport() {
        for (provider, mode, execution_policy) in [
            (
                ProviderKind::Pi,
                RuntimeMode::Ask,
                ExecutionPolicy::Interactive,
            ),
            (
                ProviderKind::Pi,
                RuntimeMode::Auto,
                ExecutionPolicy::Interactive,
            ),
        ] {
            let (wake, _) = smol::channel::bounded(1);
            let (events, _) = event_channel(wake);
            let result = start_local(
                provider,
                DriverStartOptions {
                    binary: "/missing/waku-capability-test-provider".into(),
                    cwd: "/missing/waku-capability-test-workspace".into(),
                    mode,
                    execution_policy,
                    model: None,
                    reasoning_effort: None,
                    service_tier: None,
                    context_window: None,
                    agent_preset: None,
                    provider_cursor: None,
                    delegation_session_path: None,
                    delegation_registry_dir: None,
                },
                events,
            );
            let error = result.err().expect("unsupported start must fail");
            if execution_policy == ExecutionPolicy::Interactive {
                let error = error
                    .downcast::<waku_protocol::capabilities::UnsupportedAccessMode>()
                    .unwrap();
                assert_eq!(error.provider, provider);
                assert_eq!(error.mode, mode);
            } else {
                assert!(error.to_string().contains("read-only"), "{error}");
            }
        }
        // Pi's restricted tool allowlist is independent of the permission
        // modes offered to interactive sessions.
        for provider in [ProviderKind::Pi] {
            assert!(
                validate_start(provider, RuntimeMode::Ask, ExecutionPolicy::ReviewReadOnly).is_ok()
            );
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
