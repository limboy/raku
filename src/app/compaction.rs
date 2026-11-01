use super::*;
use waku_protocol::compaction::{CompactionRequest, CompactionTrigger, ContextCompaction};

pub(super) fn compact_invocation(prompt: &str) -> Option<Option<&str>> {
    let invocation = prompt.trim().strip_prefix('/')?;
    let (name, focus) = invocation
        .split_once(char::is_whitespace)
        .unwrap_or((invocation, ""));
    (name == "compact").then(|| (!focus.trim().is_empty()).then(|| focus.trim()))
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
        let capabilities = session.provider.compaction_capabilities();
        if !capabilities.manual {
            self.show_toast(tr!("compaction.unsupported"));
            return;
        }
        if session.is_busy() || session.active_compaction.is_some() {
            self.show_toast(tr!("compaction.busy"));
            return;
        }
        if instructions.is_some() && !capabilities.instructions {
            self.show_toast(tr!("compaction.focus_unsupported"));
            return;
        }
        if session.provider_cursor.is_none() {
            self.show_toast(tr!("compaction.no_history"));
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
