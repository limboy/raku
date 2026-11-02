use serde_json::Value;
use waku_protocol::compaction::{CompactionStatus, CompactionTrigger, ContextCompaction};

use super::DriverEventSink;
use crate::model::{DriverEvent, unix_time_millis};

#[derive(Clone, Debug, Default)]
pub(super) struct CompactionTracker {
    pub current: Option<ContextCompaction>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_ignores_old_boundaries_and_recovers_only_the_pending_operation_once() {
        let (events, received) = crossbeam_channel::unbounded();
        let mut old = ContextCompaction::new("old", None, 1);
        old.finish(CompactionStatus::Completed, 2);
        let mut pending = ContextCompaction::new("pending", None, 3);
        let mut tracker = CompactionTracker::default();
        tracker.reconcile([old.clone()].into_iter(), &events);
        assert!(received.is_empty());
        assert!(tracker.current.is_none());

        tracker.seed(pending.clone());
        tracker.reconcile([old.clone()].into_iter(), &events);
        assert!(received.is_empty());
        assert_eq!(tracker.current.as_ref().unwrap().id, "pending");
        pending.summary = Some("Recovered summary".into());
        pending.finish(CompactionStatus::Completed, 4);
        tracker.reconcile([old, pending.clone()].into_iter(), &events);
        tracker.reconcile([pending].into_iter(), &events);
        let records = received.try_iter().collect::<Vec<_>>();
        assert!(
            matches!(records.as_slice(), [DriverEvent::CompactionUpdated(item)]
            if item.id == "pending" && item.status == CompactionStatus::Completed
                && item.summary.as_deref() == Some("Recovered summary"))
        );
    }

    #[test]
    fn compaction_accumulates_exact_fragments_and_keeps_attempts_separate() {
        let (events, received) = crossbeam_channel::unbounded();
        let mut tracker = CompactionTracker::default();
        tracker.start(Some("first"), Some(CompactionTrigger::Auto), &events);
        tracker.delta("keep ", &events);
        tracker.delta("the constraint", &events);
        tracker.finish(CompactionStatus::Completed, None, None, &events);
        assert_eq!(
            tracker.current.as_ref().unwrap().summary.as_deref(),
            Some("keep the constraint")
        );
        tracker.start(Some("second"), Some(CompactionTrigger::Manual), &events);
        tracker.interrupt(&events);
        let records = received
            .try_iter()
            .filter_map(|event| match event {
                DriverEvent::CompactionUpdated(item) => Some(item),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(records[3].id, "first");
        assert_eq!(records.last().unwrap().id, "second");
        assert_eq!(
            records.last().unwrap().status,
            CompactionStatus::Interrupted
        );
        assert!(records.last().unwrap().summary.is_none());
    }

    #[test]
    fn native_after_tokens_take_precedence_over_estimates_and_survive_repeated_results() {
        let (events, _) = crossbeam_channel::unbounded();
        let mut tracker = CompactionTracker::default();
        tracker.finish(
            CompactionStatus::Completed,
            Some(&serde_json::json!({
                "tokensBefore": 30113,
                "tokensAfter": 26229,
                "estimatedTokensAfter": 27000
            })),
            None,
            &events,
        );
        tracker.finish(
            CompactionStatus::Completed,
            Some(&serde_json::json!({"estimatedTokensAfter": 27000})),
            None,
            &events,
        );
        let item = tracker.current.unwrap();
        assert_eq!(item.before_tokens, Some(30113));
        assert_eq!(item.after_tokens, Some(26229));
        assert!(!item.after_tokens_estimated);
    }
}

impl CompactionTracker {
    pub fn start(
        &mut self,
        id: Option<&str>,
        trigger: Option<CompactionTrigger>,
        events: &impl DriverEventSink,
    ) {
        if self.current.as_ref().is_some_and(|item| {
            item.status == CompactionStatus::Running || id == Some(item.id.as_str())
        }) {
            return;
        }
        let item = ContextCompaction::new(
            id.map(str::to_owned)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            trigger,
            unix_time_millis(),
        );
        let _ = events.send(DriverEvent::CompactionUpdated(item.clone()));
        self.current = Some(item);
    }

    pub fn seed(&mut self, item: ContextCompaction) {
        if self
            .current
            .as_ref()
            .is_none_or(|current| current.status != CompactionStatus::Running)
        {
            self.current = Some(item);
        }
    }

    #[allow(dead_code)]
    pub fn delta(&mut self, text: &str, events: &impl DriverEventSink) {
        if self
            .current
            .as_ref()
            .is_none_or(|item| item.status != CompactionStatus::Running)
        {
            self.start(None, None, events);
        }
        if let Some(item) = &mut self.current {
            item.summary.get_or_insert_default().push_str(text);
            let _ = events.send(DriverEvent::CompactionUpdated(item.clone()));
        }
    }

    /// Recover only an observed operation. Replaying an unrelated historical
    /// boundary would append it at the live transcript tail.
    #[allow(dead_code)]
    pub fn reconcile(
        &mut self,
        history: impl Iterator<Item = ContextCompaction>,
        events: &impl DriverEventSink,
    ) {
        let Some(pending) = self
            .current
            .as_ref()
            .filter(|item| item.status == CompactionStatus::Running)
        else {
            return;
        };
        if let Some(item) = history.into_iter().find(|item| item.id == pending.id) {
            self.current = Some(item.clone());
            let _ = events.send(DriverEvent::CompactionUpdated(item));
        }
    }

    pub fn finish(
        &mut self,
        status: CompactionStatus,
        result: Option<&Value>,
        error: Option<String>,
        events: &impl DriverEventSink,
    ) {
        if self.current.is_none() {
            let mut item = ContextCompaction::new(uuid::Uuid::new_v4().to_string(), None, 0);
            item.started_at_ms = 0;
            self.current = Some(item);
        }
        let item = self.current.as_mut().unwrap();
        if let Some(result) = result {
            if let Some(summary) = result
                .get("summary")
                .or_else(|| result.get("text"))
                .and_then(Value::as_str)
            {
                item.summary = Some(summary.to_owned());
            }
            item.before_tokens = result
                .get("tokensBefore")
                .and_then(Value::as_u64)
                .or(item.before_tokens);
            if let Some(after) = result.get("tokensAfter").and_then(Value::as_u64) {
                item.after_tokens = Some(after);
                item.after_tokens_estimated = false;
            } else if let Some(after) = result.get("estimatedTokensAfter").and_then(Value::as_u64)
                && (item.after_tokens.is_none() || item.after_tokens_estimated)
            {
                item.after_tokens = Some(after);
                item.after_tokens_estimated = true;
            }
        }
        item.error = error;
        item.finish(status, unix_time_millis());
        let _ = events.send(DriverEvent::CompactionUpdated(item.clone()));
    }

    #[allow(dead_code)]
    pub fn interrupt(&mut self, events: &impl DriverEventSink) {
        if self
            .current
            .as_ref()
            .is_some_and(|item| item.status == CompactionStatus::Running)
        {
            self.finish(CompactionStatus::Interrupted, None, None, events);
        }
    }
}
