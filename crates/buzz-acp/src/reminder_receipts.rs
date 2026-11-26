use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use buzz_sdk::reminders::Reminder;
use sha2::{Digest, Sha256};

pub(super) struct Receipts {
    directory: PathBuf,
    delivered: Option<PathBuf>,
    _lock: File,
}

impl Drop for Receipts {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self._lock);
    }
}

impl Receipts {
    pub(super) fn open(
        base: &Path,
        delivered: Option<&Path>,
        relay: &str,
        author: &str,
    ) -> Result<Self> {
        let scope = hex::encode(Sha256::digest(format!("{relay}\n{author}")));
        let delivered = delivered.map(|root| root.join(&scope));
        let directory = base.join(scope);
        fs::create_dir_all(&directory).context("create reminder receipt directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        }
        let lock = private_file(&directory.join("lock"), false)?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .context("another harness owns reminder delivery for this identity")?;
        Ok(Self {
            directory,
            delivered,
            _lock: lock,
        })
    }

    fn path(&self, reminder: &Reminder) -> PathBuf {
        self.directory
            .join(hex::encode(Sha256::digest(&reminder.id)))
    }

    pub(super) fn contains(&self, reminder: &Reminder) -> Result<bool> {
        let mut value = String::new();
        match File::open(self.path(reminder)) {
            Ok(mut file) => {
                file.read_to_string(&mut value)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        let value = value.trim();
        anyhow::ensure!(
            value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid reminder delivery receipt"
        );
        Ok(value == reminder.event_id)
    }

    pub(super) fn record(&self, reminder: &Reminder) -> Result<()> {
        self.record_parts(&reminder.id, &reminder.event_id)
    }

    pub(super) fn record_parts(&self, id: &str, event_id: &str) -> Result<()> {
        anyhow::ensure!(
            event_id.len() == 64 && event_id.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid recovered reminder event"
        );
        let temporary = self
            .directory
            .join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let mut file = private_file(&temporary, true)?;
        writeln!(file, "{event_id}")?;
        file.sync_all()?;
        fs::rename(
            &temporary,
            self.directory.join(hex::encode(Sha256::digest(id))),
        )?;
        #[cfg(unix)]
        File::open(&self.directory)?.sync_all()?;
        if let Some(directory) = &self.delivered {
            if let Err(error) = project(directory, id, event_id) {
                tracing::warn!(%error, "reminder delivery projection failed; private receipt retained");
            }
        }
        Ok(())
    }
}

fn project(directory: &Path, id: &str, event_id: &str) -> Result<()> {
    let root = directory
        .parent()
        .context("reminder delivery projection has no parent")?;
    shared_directory(root)?;
    shared_directory(directory)?;
    let record = serde_json::json!({
        "event_id": event_id,
        "recorded_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
    });
    let temporary = directory.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o640);
    }
    let mut file = options.open(&temporary)?;
    writeln!(file, "{record}")?;
    file.sync_all()?;
    fs::rename(&temporary, directory.join(hex::encode(Sha256::digest(id))))?;
    Ok(())
}

fn shared_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
        Err(error) => return Err(error.into()),
    }
    anyhow::ensure!(
        fs::symlink_metadata(path)?.is_dir(),
        "reminder delivery path is not a real directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let parent = path
            .parent()
            .context("reminder delivery directory has no parent")?;
        std::os::unix::fs::chown(path, None, Some(fs::metadata(parent)?.gid()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o2750))?;
    }
    Ok(())
}

fn private_file(path: &Path, exclusive: bool) -> Result<File> {
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
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retiring_receipt_owner_releases_a_lock_even_with_an_inherited_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let first = Receipts::open(directory.path(), None, "relay", "owner").unwrap();
        let inherited = first._lock.try_clone().unwrap();
        assert!(Receipts::open(directory.path(), None, "relay", "owner").is_err());
        drop(first);
        let _next = Receipts::open(directory.path(), None, "relay", "owner").unwrap();
        drop(inherited);
    }

    const FIRST: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const SECOND: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn private(receipts: &Receipts, id: &str) -> String {
        fs::read_to_string(receipts.directory.join(hex::encode(Sha256::digest(id))))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn projected(receipts: &Receipts, id: &str) -> serde_json::Value {
        let directory = receipts.delivered.as_ref().unwrap();
        let text = fs::read_to_string(directory.join(hex::encode(Sha256::digest(id)))).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn delivery_projection_records_the_latest_event_and_its_time() {
        let directory = tempfile::tempdir().unwrap();
        let delivered = directory.path().join("delivered");
        let receipts = Receipts::open(
            &directory.path().join("reminders"),
            Some(&delivered),
            "relay",
            "owner",
        )
        .unwrap();
        receipts.record_parts("reminder", FIRST).unwrap();
        receipts.record_parts("reminder", SECOND).unwrap();
        assert_eq!(private(&receipts, "reminder"), SECOND);
        let record = projected(&receipts, "reminder");
        assert_eq!(record["event_id"], SECOND);
        assert!(
            chrono::DateTime::parse_from_rfc3339(record["recorded_at"].as_str().unwrap()).is_ok()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let scope = receipts.delivered.as_ref().unwrap();
            let file = scope.join(hex::encode(Sha256::digest("reminder")));
            assert_eq!(
                fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o640
            );
            for path in [&delivered, scope] {
                let metadata = fs::metadata(path).unwrap();
                assert_eq!(metadata.permissions().mode() & 0o7777, 0o2750);
                assert_eq!(
                    metadata.gid(),
                    fs::metadata(directory.path()).unwrap().gid()
                );
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn delivery_projection_takes_the_parent_group_rather_than_the_process_group() {
        use std::os::unix::fs::MetadataExt;
        let own = nix::unistd::getegid();
        let Some(foreign) = nix::unistd::getgroups()
            .unwrap()
            .into_iter()
            .find(|group| *group != own)
        else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let role = directory.path().join("role");
        fs::create_dir(&role).unwrap();
        std::os::unix::fs::chown(&role, None, Some(foreign.as_raw())).unwrap();
        let delivered = role.join("delivered");
        let receipts = Receipts::open(
            &directory.path().join("reminders"),
            Some(&delivered),
            "relay",
            "owner",
        )
        .unwrap();
        receipts.record_parts("reminder", FIRST).unwrap();
        let scope = receipts.delivered.clone().unwrap();
        let file = scope.join(hex::encode(Sha256::digest("reminder")));
        for path in [delivered, scope, file] {
            assert_eq!(fs::metadata(&path).unwrap().gid(), foreign.as_raw());
        }
    }

    #[test]
    fn unavailable_projection_still_commits_the_private_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let blocker = directory.path().join("not-a-directory");
        fs::write(&blocker, b"").unwrap();
        let receipts = Receipts::open(
            &directory.path().join("reminders"),
            Some(&blocker.join("delivered")),
            "relay",
            "owner",
        )
        .unwrap();
        receipts.record_parts("reminder", FIRST).unwrap();
        assert_eq!(private(&receipts, "reminder"), FIRST);
        assert!(!blocker.join("delivered").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_projection_is_refused_without_touching_its_target() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("elsewhere");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let delivered = directory.path().join("delivered");
        std::os::unix::fs::symlink(&target, &delivered).unwrap();
        let receipts = Receipts::open(
            &directory.path().join("reminders"),
            Some(&delivered),
            "relay",
            "owner",
        )
        .unwrap();
        receipts.record_parts("reminder", FIRST).unwrap();
        assert_eq!(private(&receipts, "reminder"), FIRST);
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o755
        );
    }

    #[test]
    fn unset_projection_writes_only_the_private_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let receipts =
            Receipts::open(&directory.path().join("reminders"), None, "relay", "owner").unwrap();
        receipts.record_parts("reminder", FIRST).unwrap();
        assert_eq!(private(&receipts, "reminder"), FIRST);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
