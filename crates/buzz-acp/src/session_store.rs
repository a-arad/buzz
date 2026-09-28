//! Opt-in durable conversation references. A checkpoint never contains a prompt to replay.
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::pool::SessionState;

pub(crate) struct Store {
    path: PathBuf,
    identity: String,
    _lock: File,
}

impl Drop for Store {
    fn drop(&mut self) {
        // A concurrently forked child may hold this descriptor until exec.
        // End ownership explicitly instead of waiting for its last close.
        let _ = fs2::FileExt::unlock(&self._lock);
    }
}

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    identity: String,
    state: SessionState,
}

impl SessionState {
    pub(crate) fn restore(index: usize, identity: &str) -> Result<Self> {
        match std::env::var_os("BUZZ_ACP_SESSION_STORE") {
            Some(base) => Self::open(Path::new(&base), index, identity),
            None => Ok(Self::default()),
        }
    }

    pub(crate) fn open(base: &Path, index: usize, identity: &str) -> Result<Self> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(base)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            anyhow::ensure!(
                fs::symlink_metadata(base)?.is_dir(),
                "session store is not a directory"
            );
            anyhow::ensure!(
                fs::metadata(base)?.permissions().mode() & 0o077 == 0,
                "session store must be private (0700)"
            );
        }
        let lock_path = base.join(format!("{index}.lock"));
        let lock = private_file(&lock_path, false)?;
        fs2::FileExt::try_lock_exclusive(&lock).context("conversation store already owned")?;
        let path = base.join(format!("{index}.json"));
        let mut state =
            match File::open(&path) {
                Ok(file) => {
                    anyhow::ensure!(
                        fs::symlink_metadata(&path)?.is_file(),
                        "invalid session checkpoint"
                    );
                    let mut bytes = Vec::new();
                    file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                    anyhow::ensure!(
                        bytes.len() <= 16 * 1024 * 1024,
                        "session checkpoint too large"
                    );
                    let saved: Checkpoint = serde_json::from_slice(&bytes)?;
                    anyhow::ensure!(
                        saved.version == 1 && saved.identity == identity,
                        "conversation checkpoint identity or version mismatch"
                    );
                    anyhow::ensure!(!saved.state.turn_active,
                    "interrupted conversation requires reconciliation; work will not be replayed");
                    saved.state
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
                Err(e) => return Err(e.into()),
            };
        state.pending_loads = state.sessions.values().cloned().collect();
        state
            .pending_loads
            .extend(state.heartbeat_session.iter().cloned());
        state.store = Some(Arc::new(Store {
            path,
            identity: identity.into(),
            _lock: lock,
        }));
        Ok(state)
    }

    pub(crate) fn checkpoint(&self) -> Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        if let Some(error) = &self.store_error {
            anyhow::bail!("conversation checkpoint unavailable: {error}");
        }
        let base = store
            .path
            .parent()
            .context("missing checkpoint directory")?;
        let temporary = base.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = private_file(&temporary, true)?;
            serde_json::to_writer(
                &mut file,
                &serde_json::json!({
                    "version": 1, "identity": store.identity, "state": self,
                }),
            )?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, &store.path)?;
            #[cfg(unix)]
            File::open(base)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    pub(crate) fn persist_invalidation(&mut self) {
        if let Err(error) = self.checkpoint() {
            self.store_error = Some(error.to_string());
        }
    }
}

fn private_file(path: &Path, exclusive: bool) -> Result<File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(metadata.is_file(), "invalid checkpoint file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}

pub(crate) mod scope_map {
    use super::*;
    use crate::scope::SessionScope;
    use serde::{Deserializer, Serializer};

    pub fn serialize<T: Serialize, S: Serializer>(
        value: &HashMap<SessionScope, T>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HashMap<SessionScope, T>, D::Error> {
        let entries = Vec::<(SessionScope, T)>::deserialize(deserializer)?;
        let mut result = HashMap::new();
        for (key, value) in entries {
            if result.insert(key, value).is_some() {
                return Err(serde::de::Error::custom("duplicate session scope"));
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::ChannelDeliveryState;
    use crate::scope::SessionScope;

    #[test]
    fn cold_restart_restores_scopes_delivery_and_settings_without_replaying() {
        let directory = tempfile::tempdir().unwrap();
        let scope = SessionScope::Thread {
            channel_id: uuid::Uuid::new_v4(),
            root_event_id: "a".repeat(64),
        };
        let mut state =
            SessionState::open(&directory.path().join("sessions"), 0, "identity").unwrap();
        state
            .sessions
            .insert(scope.clone(), "original-native-conversation".into());
        state.deliveries.insert(
            scope.clone(),
            ChannelDeliveryState {
                standing_context_sent: true,
                delivered_event_ids: ["delivered-event".to_owned()].into(),
            },
        );
        state.turn_counts.insert(scope.clone(), 7);
        state
            .core_sections
            .insert(scope.clone(), "original-core".into());
        state
            .canvas_sections
            .insert(scope.clone(), "original-canvas".into());
        state.model_override = Some("original-model".into());
        state.heartbeat_session = Some("heartbeat-native".into());
        state.heartbeat_standing_context_sent = true;
        state.checkpoint().unwrap();
        assert!(SessionState::open(&directory.path().join("sessions"), 0, "identity").is_err());
        drop(state);
        let mut restored =
            SessionState::open(&directory.path().join("sessions"), 0, "identity").unwrap();
        assert_eq!(restored.sessions[&scope], "original-native-conversation");
        assert!(restored
            .pending_loads
            .contains("original-native-conversation"));
        assert!(restored.pending_loads.contains("heartbeat-native"));
        assert!(restored.deliveries[&scope].standing_context_sent);
        assert!(restored.deliveries[&scope]
            .delivered_event_ids
            .contains("delivered-event"));
        assert_eq!(restored.turn_counts[&scope], 7);
        assert_eq!(restored.core_sections[&scope], "original-core");
        assert_eq!(restored.canvas_sections[&scope], "original-canvas");
        assert_eq!(restored.model_override.as_deref(), Some("original-model"));
        restored.invalidate_all();
        assert_eq!(restored.sessions[&scope], "original-native-conversation");
        restored.invalidate_scope(&scope);
        drop(restored);
        let again = SessionState::open(&directory.path().join("sessions"), 0, "identity").unwrap();
        assert!(!again.sessions.contains_key(&scope));
    }

    #[test]
    fn interrupted_corrupt_or_foreign_checkpoint_never_becomes_an_empty_conversation() {
        let directory = tempfile::tempdir().unwrap();
        let mut state = SessionState::open(&directory.path().join("sessions"), 0, "owner").unwrap();
        state.turn_active = true;
        state.checkpoint().unwrap();
        drop(state);
        assert!(SessionState::open(&directory.path().join("sessions"), 0, "owner").is_err());
        assert!(SessionState::open(&directory.path().join("sessions"), 0, "foreign").is_err());
        fs::write(directory.path().join("sessions/0.json"), b"{").unwrap();
        assert!(SessionState::open(&directory.path().join("sessions"), 0, "owner").is_err());
    }
}
