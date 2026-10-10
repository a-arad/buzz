use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

use crate::queue::{EventQueue, Snapshot};

#[derive(Clone, Default)]
pub(crate) struct Drain {
    pub(crate) root: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct Backlog {
    identity: String,
    queue: Snapshot,
}

impl Drain {
    pub(crate) fn from_env() -> Self {
        Self {
            root: std::env::var_os("BUZZ_ACP_SESSION_STORE").map(PathBuf::from),
        }
    }

    pub(crate) fn requested(&self) -> bool {
        self.root
            .as_ref()
            .is_some_and(|root| root.join("deployment/request").try_exists().unwrap_or(true))
    }

    fn write(&self, name: &str, value: &impl Serialize) -> Result<()> {
        let Some(root) = &self.root else {
            anyhow::bail!("drain requires a durable session store")
        };
        #[cfg(unix)]
        let parent = root;
        let root = root.join("deployment");
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&root)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        let temporary = root.join(format!(".drain-{}", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        let result = (|| {
            serde_json::to_writer(&mut file, value)?;
            ensure!(
                file.metadata()?.len() <= 16 * 1024 * 1024,
                "deployment backlog too large"
            );
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, root.join(name))?;
            #[cfg(unix)]
            fs::File::open(&root)?.sync_all()?;
            Ok(())
        })();
        if temporary.exists() {
            fs::remove_file(temporary)?;
        }
        result
    }

    fn read(path: &std::path::Path) -> Result<Vec<u8>> {
        ensure!(
            !path.symlink_metadata()?.file_type().is_symlink(),
            "drain record must not be a symlink"
        );
        let file = fs::File::open(path)?;
        ensure!(
            file.metadata()?.is_file(),
            "drain record must be a regular file"
        );
        let mut bytes = Vec::new();
        file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 16 * 1024 * 1024, "drain record too large");
        Ok(bytes)
    }

    fn represented(&self) -> Result<HashSet<String>> {
        let mut ids = HashSet::new();
        let Some(root) = &self.root else {
            return Ok(ids);
        };
        if !root.try_exists()? {
            return Ok(ids);
        }
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json")
                || !path
                    .file_stem()
                    .and_then(|x| x.to_str())
                    .is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()))
            {
                continue;
            }
            let record: serde_json::Value = serde_json::from_slice(&Self::read(&path)?)?;
            for id in record
                .pointer("/state/pending_turn/delivered")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(id) = id.as_str() {
                    ids.insert(id.to_owned());
                }
            }
            for delivery in record
                .pointer("/state/deliveries")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                for id in delivery
                    .pointer("/1/delivered_event_ids")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    if let Some(id) = id.as_str() {
                        ids.insert(id.to_owned());
                    }
                }
            }
        }
        Ok(ids)
    }

    pub(crate) fn startup_events(&self, queue: &EventQueue) -> Result<HashSet<String>> {
        let mut ids = self.represented()?;
        ids.extend(
            queue
                .deployment_snapshot()?
                .events
                .iter()
                .map(|event| event.event.id.to_hex()),
        );
        Ok(ids)
    }

    pub(crate) fn restore(&self, identity: &str, queue: &mut EventQueue) -> Result<()> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let path = root.join("deployment/backlog.json");
        if !path.try_exists()? {
            return Ok(());
        }
        let mut saved: Backlog = serde_json::from_slice(&Self::read(&path)?)?;
        ensure!(
            saved.identity == identity,
            "deployment backlog identity changed"
        );
        let represented = self.represented()?;
        saved
            .queue
            .events
            .retain(|event| !represented.contains(&event.event.id.to_hex()));
        queue.restore_deployment(saved.queue)
    }

    pub(crate) fn retire_delivered(&self, identity: &str) -> Result<()> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let path = root.join("deployment/backlog.json");
        if !path.try_exists()? {
            return Ok(());
        }
        let mut saved: Backlog = serde_json::from_slice(&Self::read(&path)?)?;
        ensure!(
            saved.identity == identity,
            "deployment backlog identity changed"
        );
        let represented = self.represented()?;
        let before = saved.queue.events.len();
        saved
            .queue
            .events
            .retain(|event| !represented.contains(&event.event.id.to_hex()));
        if saved.queue.events.is_empty() {
            fs::remove_file(path)?;
            #[cfg(unix)]
            fs::File::open(root.join("deployment"))?.sync_all()?;
        } else if saved.queue.events.len() != before {
            self.write("backlog.json", &saved)?;
        }
        Ok(())
    }

    pub(crate) fn poll(&self, identity: &str, queue: &EventQueue, idle: bool) -> bool {
        if !self.requested() || !idle {
            return false;
        }
        match self.ready(identity, queue) {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(%error, "deployment drain blocked; process and queued work retained");
                false
            }
        }
    }

    pub(crate) fn ready(&self, identity: &str, queue: &EventQueue) -> Result<()> {
        self.write(
            "backlog.json",
            &Backlog {
                identity: identity.to_owned(),
                queue: queue.deployment_snapshot()?,
            },
        )?;
        let Some(root) = &self.root else {
            anyhow::bail!("drain requires a session store")
        };
        self.write(
            "drain.json",
            &serde_json::json!({
                "state": "drained", "pid": std::process::id(), "identity": identity,
                "request": String::from_utf8(Self::read(&root.join("deployment/request"))?)?,
            }),
        )
    }
}

#[cfg(test)]
#[path = "deployment_drain_tests.rs"]
mod tests;
