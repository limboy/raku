use super::*;
use waku_protocol::compaction::{CompactionRequest, CompactionTrigger, ContextCompaction};

pub(super) fn compact_invocation(prompt: &str) -> Option<Option<&str>> {
    let invocation = prompt.trim().strip_prefix('/')?;
    let (name, focus) = invocation
        .split_once(char::is_whitespace)
        .unwrap_or((invocation, ""));
    (name == "compact").then(|| (!focus.trim().is_empty()).then(|| focus.trim()))
}

fn validate_compaction_request(
    session: &AgentSession,
    has_runtime: bool,
    has_instructions: bool,
) -> Result<(), String> {
    let capabilities = session.provider.capabilities().compaction;
    if !capabilities.manual {
        return Err(tr!("compaction.unsupported"));
    }
    if session.is_busy() || session.active_compaction.is_some() {
        return Err(tr!("compaction.busy"));
    }
    if has_instructions && !capabilities.instructions {
        return Err(tr!("compaction.focus_unsupported"));
    }
    if session.provider_cursor.is_none() {
        return Err(tr!("compaction.no_history"));
    }
    if !has_runtime {
        session
            .provider
            .validate_access_mode(session.runtime_mode)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

impl Waku {
    pub(super) fn request_session_compaction(
        &mut self,
        session_id: Uuid,
        instructions: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        if let Err(error) = validate_compaction_request(
            session,
            self.runtimes.contains_key(&session_id),
            instructions.is_some(),
        ) {
            self.show_toast(error);
            return;
        }
        let request = CompactionRequest {
            id: format!("msg_{}", Uuid::new_v4().simple()),
            instructions,
        };
        let item = ContextCompaction::new(
            request.id.clone(),
            Some(CompactionTrigger::Manual),
            unix_time_millis(),
        );
        if let Some(session) = self.state.session_mut(session_id) {
            session.active_compaction = Some(request.id.clone());
            push_transcript_activity(session, ActivityItem::from_compaction(item), false);
        }
        self.pending_compactions.insert(session_id, request);
        self.state.mark_session_dirty(session_id);
        self.drain_pending_compaction(session_id);
        if self.pending_compactions.contains_key(&session_id) {
            self.start_goal_runtime(session_id, cx);
        }
        self.save();
        cx.notify();
    }

    pub(super) fn drain_pending_compaction(&mut self, session_id: Uuid) {
        if let Some(runtime) = self.runtimes.get(&session_id)
            && let Some(request) = self.pending_compactions.remove(&session_id)
        {
            runtime.driver.compact(request);
        }
    }

    pub(super) fn interrupt_session_compactions(&mut self, session_id: Uuid) {
        self.pending_compactions.remove(&session_id);
        if let Some(session) = self.state.session_mut(session_id) {
            session.interrupt_compactions(unix_time_millis());
        }
        self.state.mark_session_dirty(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_compaction_requires_a_startable_mode_but_a_live_runtime_can_be_reused() {
        let provider = ProviderKind::Pi;
        let mut session = AgentSession::new(Uuid::new_v4(), provider);
        session.provider_cursor = Some(ProviderResumeCursor::from_session_id(
            provider,
            "native".into(),
        ));
        session.runtime_mode = RuntimeMode::Ask;
        assert!(validate_compaction_request(&session, false, false).is_err());
        assert!(validate_compaction_request(&session, true, false).is_ok());
        session.runtime_mode = RuntimeMode::FullAccess;
        assert!(validate_compaction_request(&session, false, false).is_ok());
    }
}
