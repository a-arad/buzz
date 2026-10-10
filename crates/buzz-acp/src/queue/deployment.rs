use super::*;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub(crate) events: Vec<QueuedEvent>,
    retries: Vec<(SessionScope, u32, u64)>,
}

impl EventQueue {
    pub(crate) fn deployment_snapshot(&self) -> Result<Snapshot> {
        ensure!(
            self.in_flight_scopes.is_empty()
                && self.cancelled_batches.is_empty()
                && self.withheld_native_steer.is_empty(),
            "unsettled queue cannot be checkpointed for deployment"
        );
        let mut events: Vec<_> = self.queues.values().flatten().cloned().collect();
        events.sort_by_key(|event| event.received_at);
        let now = Instant::now();
        let retries = self
            .retry_counts
            .iter()
            .map(|(scope, count)| {
                let delay = self
                    .retry_after
                    .get(scope)
                    .map(|t| t.saturating_duration_since(now).as_millis() as u64)
                    .unwrap_or(0);
                (scope.clone(), *count, delay)
            })
            .collect();
        Ok(Snapshot { events, retries })
    }

    pub(crate) fn restore_deployment(&mut self, saved: Snapshot) -> Result<()> {
        let mut counts = HashMap::new();
        for event in &saved.events {
            ensure!(
                event.channel_id == event.scope.channel_id(),
                "backlog scope mismatch"
            );
            let count = counts.entry(event.channel_id).or_insert(0);
            *count += 1;
            ensure!(
                *count <= MAX_PENDING_PER_CHANNEL,
                "deployment backlog exceeds channel limit"
            );
        }
        for (scope, count, delay) in &saved.retries {
            ensure!(
                *count <= MAX_RETRIES && *delay <= 2 * MAX_RETRY_DELAY_SECS * 1000,
                "invalid saved retry budget"
            );
            self.retry_counts.insert(scope.clone(), *count);
            self.retry_after.insert(
                scope.clone(),
                Instant::now() + Duration::from_millis(*delay),
            );
        }
        for event in saved.events {
            self.push(event);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{conv, make_queued};
    use super::*;

    #[test]
    fn retry_budget_survives_deployment_and_active_work_refuses_snapshot() {
        let channel = Uuid::new_v4();
        let mut queue = EventQueue::new(DedupMode::Queue);
        queue.push(make_queued(channel, "retained"));
        let batch = queue.flush_next().unwrap();
        assert!(queue.deployment_snapshot().is_err());
        queue.requeue(batch);
        queue.mark_complete(channel);
        let saved = serde_json::to_vec(&queue.deployment_snapshot().unwrap()).unwrap();
        let mut restored = EventQueue::new(DedupMode::Queue);
        restored
            .restore_deployment(serde_json::from_slice(&saved).unwrap())
            .unwrap();
        assert_eq!(restored.retry_counts[&conv(channel)], 1);
        assert!(restored.retry_after[&conv(channel)] > Instant::now());
        assert_eq!(restored.queues[&conv(channel)][0].event.content, "retained");
    }
    #[test]
    fn edited_message_routing_survives_deployment() {
        let channel = Uuid::new_v4();
        let mut queue = EventQueue::new(DedupMode::Queue);
        let mut event = make_queued(channel, "edited message");
        let routing = ResolvedEdit {
            target_event_id: "b".repeat(64),
            target_thread_tags: ThreadTags {
                root_event_id: Some("a".repeat(64)),
                parent_event_id: Some("b".repeat(64)),
                mentioned_pubkeys: vec!["c".repeat(64)],
            },
        };
        event.edit = Some(routing.clone());
        queue.push(event);
        let saved = serde_json::to_vec(&queue.deployment_snapshot().unwrap()).unwrap();
        let mut restored = EventQueue::new(DedupMode::Queue);
        restored
            .restore_deployment(serde_json::from_slice(&saved).unwrap())
            .unwrap();
        let batch = restored.flush_next().unwrap();
        assert_eq!(batch.events[0].edit, Some(routing));
    }
}
