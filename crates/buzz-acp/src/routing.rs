//! Private, append-only receipts for decisions made by the normal message router.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use nostr::Event;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub(crate) struct RoutingLog(Arc<Writer>);

#[derive(Debug)]
struct Writer {
    directory: PathBuf,
    recipient: String,
    instance: String,
    policy_sha256: String,
    lock: Mutex<()>,
}

impl RoutingLog {
    pub(crate) fn open(directory: PathBuf, recipient: String, policy: Value) -> io::Result<Self> {
        fs::create_dir_all(&directory)?;
        let policy_bytes = serde_json::to_vec(&policy)?;
        let result = Self(Arc::new(Writer {
            directory,
            recipient,
            instance: Uuid::new_v4().to_string(),
            policy_sha256: hex::encode(Sha256::digest(&policy_bytes)),
            lock: Mutex::new(()),
        }));
        result.append(json!({"type": "routing_policy", "policy": policy}))?;
        Ok(result)
    }

    fn append(&self, mut row: Value) -> io::Result<()> {
        let _guard = self
            .0
            .lock
            .lock()
            .map_err(|_| io::Error::other("routing log lock poisoned"))?;
        let now = Utc::now();
        row["recorded_at"] = json!(now.to_rfc3339());
        row["recipient_pubkey"] = json!(self.0.recipient);
        row["controller_instance"] = json!(self.0.instance);
        row["policy_sha256"] = json!(self.0.policy_sha256);
        let path = self.0.directory.join(format!(
            "{}.{}.jsonl",
            now.format("%Y-%m-%d"),
            self.0.instance
        ));
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        let mut bytes = serde_json::to_vec(&row)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_data()
    }

    pub(crate) fn record(&self, event: &Event, channel: Uuid, outcome: &str, detail: Value) {
        let row = json!({
            "type": "routing_outcome", "event_id": event.id.to_hex(),
            "channel_id": channel, "author_pubkey": event.pubkey.to_hex(),
            "event_created_at": event.created_at.as_secs(), "event_kind": event.kind.as_u16(),
            "outcome": outcome, "detail": detail,
        });
        if let Err(error) = self.append(row) {
            tracing::error!(event_id = %event.id, outcome, %error, "routing receipt could not be retained");
        }
    }
}

#[cfg(test)]
pub(crate) fn retained_rows(directory: &std::path::Path) -> Vec<Value> {
    fs::read_dir(directory)
        .expect("receipt directory")
        .flat_map(|entry| {
            fs::read_to_string(entry.expect("receipt entry").path())
                .expect("receipt contents")
                .lines()
                .map(|line| serde_json::from_str(line).expect("receipt JSON"))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipts_survive_reopen_without_retaining_message_bodies(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let keys = nostr::Keys::generate();
        let event =
            nostr::EventBuilder::new(nostr::Kind::Custom(9), "private body must not be copied")
                .sign_with_keys(&keys)?;
        let policy = json!({"respond_to": "allowlist", "allowlist": [keys.public_key().to_hex()]});
        let recorder = RoutingLog::open(
            directory.path().to_path_buf(),
            "recipient".into(),
            policy.clone(),
        )?;
        recorder.record(
            &event,
            Uuid::nil(),
            "author_rejected",
            json!({"reason": "author_policy"}),
        );
        drop(recorder);
        let second = RoutingLog::open(directory.path().to_path_buf(), "recipient".into(), policy)?;
        second.record(&event, Uuid::nil(), "queued", json!({"rule": "mention"}));
        let mut rows = Vec::new();
        for entry in fs::read_dir(directory.path())? {
            let text = fs::read_to_string(entry?.path())?;
            assert!(!text.contains("private body must not be copied"));
            for line in text.lines() {
                rows.push(serde_json::from_str::<Value>(line)?);
            }
        }
        let outcomes: Vec<_> = rows
            .iter()
            .filter(|r| r["type"] == "routing_outcome")
            .collect();
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|r| r["event_id"] == event.id.to_hex()));
        assert_eq!(outcomes[0]["policy_sha256"], outcomes[1]["policy_sha256"]);
        assert_ne!(
            outcomes[0]["controller_instance"],
            outcomes[1]["controller_instance"]
        );
        assert!(outcomes.iter().all(|r| r["recorded_at"].is_string()));
        Ok(())
    }
}
