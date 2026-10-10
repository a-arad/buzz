use super::*;
use crate::config::DedupMode;
use crate::queue::QueuedEvent;
use crate::scope::SessionScope;
use nostr::{EventBuilder, Keys, Kind};
use std::time::Instant;
use uuid::Uuid;

fn queued(text: &str) -> QueuedEvent {
    let channel_id = Uuid::new_v4();
    QueuedEvent {
        channel_id,
        scope: SessionScope::Conversation { channel_id },
        event: EventBuilder::new(Kind::Custom(9), text)
            .sign_with_keys(&Keys::generate())
            .unwrap(),
        received_at: Instant::now(),
        prompt_tag: "mention".into(),
        edit: None,
    }
}

#[test]
fn only_idle_durable_work_can_acknowledge_the_current_request() {
    let root = tempfile::tempdir().unwrap();
    let drain = Drain {
        root: Some(root.path().into()),
    };
    fs::create_dir(root.path().join("deployment")).unwrap();
    let mut queue = EventQueue::new(DedupMode::Queue);
    queue.push(queued("unstarted research question"));
    assert!(!drain.poll("identity", &queue, true));
    fs::write(root.path().join("deployment/request"), "nonce-1").unwrap();
    assert!(!drain.poll("identity", &queue, false));
    assert!(!root.path().join("deployment/drain.json").exists());
    assert!(drain.poll("identity", &queue, true));
    assert!(fs::read_dir(root.path()).unwrap().all(|entry| entry
        .unwrap()
        .path()
        .extension()
        .is_none_or(|ext| ext != "json")));
    let ack: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("deployment/drain.json")).unwrap())
            .unwrap();
    assert_eq!(ack["request"], "nonce-1");
    assert_eq!(ack["pid"], std::process::id());
    let mut restored = EventQueue::new(DedupMode::Queue);
    drain.restore("identity", &mut restored).unwrap();
    assert_eq!(
        restored.deployment_snapshot().unwrap().events[0]
            .event
            .content,
        "unstarted research question"
    );
    assert!(drain.restore("different-identity", &mut restored).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(root.path().join("deployment/backlog.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn failed_snapshot_never_acknowledges_or_discards_queued_events() {
    let root = tempfile::tempdir().unwrap();
    let drain = Drain {
        root: Some(root.path().into()),
    };
    fs::create_dir(root.path().join("deployment")).unwrap();
    let mut queue = EventQueue::new(DedupMode::Queue);
    queue.push(queued("preserve me"));
    fs::write(root.path().join("deployment/request"), "nonce").unwrap();
    fs::create_dir(root.path().join("deployment/backlog.json")).unwrap();
    assert!(!drain.poll("identity", &queue, true));
    assert!(!root.path().join("deployment/drain.json").exists());
    assert_eq!(queue.deployment_snapshot().unwrap().events.len(), 1);
}

#[test]
fn accepted_and_completed_events_are_pruned_before_replaying_a_backlog() {
    let root = tempfile::tempdir().unwrap();
    let drain = Drain {
        root: Some(root.path().into()),
    };
    fs::create_dir(root.path().join("deployment")).unwrap();
    let mut queue = EventQueue::new(DedupMode::Queue);
    let accepted = queued("already accepted");
    let completed = queued("already published");
    let waiting = queued("still waiting");
    for event in [&accepted, &completed, &waiting] {
        queue.push(event.clone());
    }
    fs::write(root.path().join("deployment/request"), "nonce").unwrap();
    assert!(drain.poll("identity", &queue, true));
    fs::write(
        root.path().join("0.json"),
        serde_json::to_vec(&serde_json::json!({"state": {
            "pending_turn": {"delivered": [accepted.event.id.to_hex()]},
            "deliveries": [[{}, {"delivered_event_ids": [completed.event.id.to_hex()]}]]
        }}))
        .unwrap(),
    )
    .unwrap();
    drain.retire_delivered("identity").unwrap();
    let mut restored = EventQueue::new(DedupMode::Queue);
    drain.restore("identity", &mut restored).unwrap();
    let replay_ids = drain.startup_events(&restored).unwrap();
    assert!(replay_ids.contains(&accepted.event.id.to_hex()));
    assert!(replay_ids.contains(&completed.event.id.to_hex()));
    assert!(replay_ids.contains(&waiting.event.id.to_hex()));
    let snapshot = restored.deployment_snapshot().unwrap();
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].event.id, waiting.event.id);
    fs::write(root.path().join("0.json"), b"corrupt checkpoint").unwrap();
    assert!(drain.restore("identity", &mut restored).is_err());
    assert!(root.path().join("deployment/backlog.json").exists());
}
