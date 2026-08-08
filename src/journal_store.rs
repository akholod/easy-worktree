use crate::{journal::Journal, lifecycle::OperationId};
use fs4::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;
use uuid::Uuid;

#[cfg(test)]
static FAIL_BEFORE_RENAME: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static FAIL_DIRECTORY_SYNC: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static FAULT_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("repository is busy")]
    RepositoryBusy,
    #[error("journal not found")]
    NotFound,
    #[error("invalid operation id")]
    InvalidId,
    #[error("corrupt journal: {0}")]
    Corrupt(String),
    #[error("journal revision is not the next revision")]
    RevisionConflict,
    #[error("journal immutable plan or identity changed")]
    ImmutableMismatch,
    #[error("journal transition is not a direct legal successor")]
    InvalidTransition,
    #[error("journal I/O failed: {0}")]
    Io(#[from] io::Error),
}

pub struct RepositoryLock {
    file: File,
}
impl RepositoryLock {
    pub fn acquire(common_dir: &Path) -> Result<Self, JournalError> {
        let dir = common_dir.join("ewtm");
        fs::create_dir_all(&dir)?;
        let path = dir.join("repository.lock");
        let file = open_private(
            OpenOptions::new().create(true).read(true).write(true),
            &path,
        )?;
        set_private(&file)?;
        match FileExt::try_lock(&file) {
            Ok(()) => Ok(Self { file }),
            Err(fs4::TryLockError::WouldBlock) => Err(JournalError::RepositoryBusy),
            Err(fs4::TryLockError::Error(error)) => Err(JournalError::Io(error)),
        }
    }
}
impl Drop for RepositoryLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

pub struct JournalStore {
    dir: PathBuf,
}
impl JournalStore {
    pub fn new(common_dir: &Path) -> Self {
        Self {
            dir: common_dir.join("ewtm").join("journal"),
        }
    }
    pub fn read(&self, id: &OperationId) -> Result<Journal, JournalError> {
        let path = self.path(id)?;
        self.read_path(&path, id)
    }
    fn read_path(&self, path: &Path, id: &OperationId) -> Result<Journal, JournalError> {
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    JournalError::NotFound
                } else {
                    JournalError::Io(e)
                }
            })?
            .read_to_end(&mut bytes)?;
        let journal: Journal =
            serde_json::from_slice(&bytes).map_err(|e| JournalError::Corrupt(e.to_string()))?;
        if journal.operation_id() != id {
            return Err(JournalError::Corrupt(
                "journal operation id does not match filename".into(),
            ));
        }
        Ok(journal)
    }
    pub fn list(&self) -> Result<Vec<Journal>, JournalError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut result = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let value = path
                .file_stem()
                .and_then(|v| v.to_str())
                .ok_or_else(|| JournalError::Corrupt("invalid journal filename".into()))?;
            let id: OperationId = value
                .parse()
                .map_err(|_| JournalError::Corrupt("invalid journal filename".into()))?;
            if value != id.to_string() {
                return Err(JournalError::Corrupt(
                    "non-canonical journal filename".into(),
                ));
            }
            result.push(self.read_path(&path, &id)?);
        }
        result.sort_by_key(|journal| journal.operation_id().to_string());
        Ok(result)
    }
    fn path(&self, id: &OperationId) -> Result<PathBuf, JournalError> {
        let value = id.to_string();
        if value != id.as_uuid().to_string() {
            return Err(JournalError::InvalidId);
        }
        Ok(self.dir.join(format!("{value}.json")))
    }
}

pub struct LockedJournalStore {
    _lock: RepositoryLock,
    store: JournalStore,
}
impl LockedJournalStore {
    pub fn acquire(common_dir: &Path) -> Result<Self, JournalError> {
        Ok(Self {
            _lock: RepositoryLock::acquire(common_dir)?,
            store: JournalStore::new(common_dir),
        })
    }
    pub fn write_new(&mut self, journal: &Journal) -> Result<(), JournalError> {
        journal.validate().map_err(JournalError::Corrupt)?;
        if journal.revision() != 0
            || journal.status() != crate::journal::OperationStatus::Pending
            || journal
                .steps()
                .iter()
                .any(|step| step.status() != crate::journal::StepStatus::Pending)
        {
            return Err(JournalError::Corrupt(
                "new journal is not in canonical pending state".into(),
            ));
        }
        let path = self.store.path(journal.operation_id())?;
        if path.exists() {
            return Err(JournalError::RevisionConflict);
        }
        self.atomic_write(journal, &path)
    }
    pub fn update(&mut self, previous: &Journal, next: &Journal) -> Result<(), JournalError> {
        previous.validate().map_err(JournalError::Corrupt)?;
        next.validate().map_err(JournalError::Corrupt)?;
        if previous.schema_version() != next.schema_version()
            || previous.operation_id() != next.operation_id()
            || previous.plan() != next.plan()
            || next.revision()
                != previous
                    .revision()
                    .checked_add(1)
                    .ok_or(JournalError::RevisionConflict)?
        {
            return Err(JournalError::ImmutableMismatch);
        }
        let current = self.store.read(previous.operation_id())?;
        if current != *previous {
            return Err(JournalError::RevisionConflict);
        }
        previous
            .validate_successor(next)
            .map_err(|_| JournalError::InvalidTransition)?;
        self.atomic_write(next, &self.store.path(next.operation_id())?)
    }
    fn atomic_write(&self, journal: &Journal, path: &Path) -> Result<(), JournalError> {
        fs::create_dir_all(&self.store.dir)?;
        let temp = self.store.dir.join(format!(".{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut file = open_private(OpenOptions::new().write(true).create_new(true), &temp)?;
            let bytes = serde_json::to_vec_pretty(journal)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()?;
            #[cfg(test)]
            if FAIL_BEFORE_RENAME.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err(io::Error::other("injected pre-rename failure"));
            }
            fs::rename(&temp, path)?;
            let _ = sync_dir(&self.store.dir);
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.map_err(JournalError::Io)
    }
}

fn open_private(options: &mut OpenOptions, path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn set_private(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(())
    }
}
fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_DIRECTORY_SYNC.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return Err(io::Error::other("injected directory sync failure"));
    }
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{OperationStatus, StepStatus};
    use std::{
        io::Write,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };
    use tempfile::TempDir;
    #[test]
    fn lock_is_persistent_and_contention_is_typed() {
        let temp = TempDir::new().unwrap();
        let first = RepositoryLock::acquire(temp.path()).unwrap();
        assert!(matches!(
            RepositoryLock::acquire(temp.path()),
            Err(JournalError::RepositoryBusy)
        ));
        drop(first);
        assert!(temp.path().join("ewtm/repository.lock").exists());
        let _second = RepositoryLock::acquire(temp.path()).unwrap();
    }

    #[test]
    fn cross_process_contention_uses_the_same_persistent_lock() {
        if std::env::var_os("EWTM_LOCK_HELPER").is_some() {
            let root = std::env::var_os("EWTM_LOCK_ROOT").unwrap();
            let lock = RepositoryLock::acquire(Path::new(&root)).unwrap();
            std::fs::write(std::env::var_os("EWTM_LOCK_READY").unwrap(), b"ready").unwrap();
            let mut input = std::io::stdin();
            let mut byte = [0; 1];
            let _ = std::io::Read::read(&mut input, &mut byte);
            drop(lock);
            let _again = RepositoryLock::acquire(Path::new(&root)).unwrap();
            std::fs::write(
                std::env::var_os("EWTM_LOCK_REACQUIRED").unwrap(),
                b"reacquired",
            )
            .unwrap();
            return;
        }
        let temp = TempDir::new().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("journal_store::tests::cross_process_contention_uses_the_same_persistent_lock")
            .env("EWTM_LOCK_HELPER", "1")
            .env("EWTM_LOCK_ROOT", temp.path())
            .env("EWTM_LOCK_READY", temp.path().join("ready"))
            .env("EWTM_LOCK_REACQUIRED", temp.path().join("reacquired"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !temp.path().join("ready").exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            temp.path().join("ready").exists(),
            "lock helper did not become ready"
        );
        assert!(matches!(
            RepositoryLock::acquire(temp.path()),
            Err(JournalError::RepositoryBusy)
        ));
        let _ = child.stdin.take().unwrap().write_all(b"x");
        assert!(child.wait().unwrap().success());
        assert!(temp.path().join("reacquired").exists());
        let _reacquired = RepositoryLock::acquire(temp.path()).unwrap();
    }

    #[test]
    fn write_new_read_update_and_revision_guards() {
        let _guard = FAULT_MUTEX.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let mut store = LockedJournalStore::acquire(temp.path()).unwrap();
        let mut original = Journal::new(crate::lifecycle::test_plan(2));
        store.write_new(&original).unwrap();
        let disk_before = JournalStore::new(temp.path())
            .read(original.operation_id())
            .unwrap();
        assert_eq!(disk_before, original);
        let mut forged_wire = serde_json::to_value(&disk_before).unwrap();
        forged_wire["revision"] = serde_json::Value::from(1);
        forged_wire["status"] = serde_json::Value::from("applied");
        for step in forged_wire["steps"].as_array_mut().unwrap() {
            step["status"] = serde_json::Value::from("applied");
        }
        let forged: Journal = serde_json::from_value(forged_wire).unwrap();
        assert!(matches!(
            store.update(&disk_before, &forged),
            Err(JournalError::InvalidTransition)
        ));
        assert_eq!(
            JournalStore::new(temp.path())
                .read(original.operation_id())
                .unwrap(),
            disk_before
        );
        let first = original.steps()[0].id().clone();
        original.start_step(&first).unwrap();
        store.update(&disk_before, &original).unwrap();
        let files = std::fs::read_dir(temp.path().join("ewtm/journal"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].extension().and_then(|ext| ext.to_str()),
            Some("json")
        );
        let current = JournalStore::new(temp.path())
            .read(original.operation_id())
            .unwrap();
        assert_eq!(current, original);
        let mut stale_wire = serde_json::to_value(&current).unwrap();
        stale_wire["steps"][0]["status"] = serde_json::Value::from("applied");
        stale_wire["status"] = serde_json::Value::from("pending");
        stale_wire["revision"] = serde_json::Value::from(current.revision());
        let stale: Journal = serde_json::from_value(stale_wire).unwrap();
        let mut stale_next_wire = serde_json::to_value(&stale).unwrap();
        stale_next_wire["revision"] = serde_json::Value::from(stale.revision() + 1);
        let stale_next: Journal = serde_json::from_value(stale_next_wire).unwrap();
        assert!(matches!(
            store.update(&stale, &stale_next),
            Err(JournalError::RevisionConflict)
        ));
        assert_eq!(
            JournalStore::new(temp.path())
                .read(current.operation_id())
                .unwrap(),
            current
        );
        let mut non_new = Journal::new(crate::lifecycle::test_plan(1));
        let non_new_id = non_new.steps()[0].id().clone();
        non_new.start_step(&non_new_id).unwrap();
        assert!(matches!(
            store.write_new(&non_new),
            Err(JournalError::Corrupt(_))
        ));
        let mut skipped_wire = serde_json::to_value(&disk_before).unwrap();
        skipped_wire["revision"] = serde_json::Value::from(2);
        let skipped: Journal = serde_json::from_value(skipped_wire).unwrap();
        assert!(matches!(
            store.update(&disk_before, &skipped),
            Err(JournalError::ImmutableMismatch)
        ));
        let mut replacement_wire = serde_json::to_value(&current).unwrap();
        replacement_wire["revision"] = serde_json::Value::from(current.revision() + 1);
        replacement_wire["plan"]["steps"][0]["name"] = serde_json::Value::from("replacement");
        let replacement: Journal = serde_json::from_value(replacement_wire).unwrap();
        assert!(matches!(
            store.update(&current, &replacement),
            Err(JournalError::ImmutableMismatch)
        ));
        assert_eq!(
            JournalStore::new(temp.path())
                .read(current.operation_id())
                .unwrap(),
            current
        );
        assert_eq!(current.status(), OperationStatus::Running);
        assert_eq!(current.steps()[0].status(), StepStatus::Started);
        let files = std::fs::read_dir(temp.path().join("ewtm/journal"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 1);
        assert!(
            files
                .iter()
                .all(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        );
    }

    #[test]
    fn read_and_list_fail_closed_for_corruption_and_sort_valid_ids() {
        let _guard = FAULT_MUTEX.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let mut store = LockedJournalStore::acquire(temp.path()).unwrap();
        let first = Journal::new(crate::lifecycle::test_plan(1));
        store.write_new(&first).unwrap();
        let second = Journal::new(crate::lifecycle::test_plan(1));
        store.write_new(&second).unwrap();
        let mut expected = vec![
            first.operation_id().to_string(),
            second.operation_id().to_string(),
        ];
        expected.sort();
        let listed = JournalStore::new(temp.path()).list().unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|item| item.operation_id().to_string())
                .collect::<Vec<_>>(),
            expected
        );
        let id = first.operation_id().to_string();
        let dir = temp.path().join("ewtm/journal");
        let noncanonical = id.replace('-', "");
        std::fs::write(
            dir.join(format!("{noncanonical}.json")),
            serde_json::to_vec(&first).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            JournalStore::new(temp.path()).list(),
            Err(JournalError::Corrupt(_))
        ));
        std::fs::remove_file(dir.join(format!("{noncanonical}.json"))).unwrap();
        std::fs::write(dir.join("not-an-id.json"), b"{}").unwrap();
        assert!(matches!(
            JournalStore::new(temp.path()).list(),
            Err(JournalError::Corrupt(_))
        ));
        let json_files = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert!(
            json_files
                .iter()
                .all(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        );
        std::fs::remove_file(dir.join("not-an-id.json")).unwrap();
        std::fs::write(dir.join("00000000-0000-0000-0000-000000000001.json"), b"{}").unwrap();
        assert!(
            JournalStore::new(temp.path())
                .read(&id.parse().unwrap())
                .is_ok()
        );
        assert!(matches!(
            JournalStore::new(temp.path())
                .read(&"00000000-0000-0000-0000-000000000001".parse().unwrap()),
            Err(JournalError::Corrupt(_))
        ));
        std::fs::write(dir.join(format!("{id}.json")), b"{").unwrap();
        assert!(matches!(
            JournalStore::new(temp.path()).read(&id.parse().unwrap()),
            Err(JournalError::Corrupt(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn lock_and_journal_files_are_private_and_lock_path_persists() {
        use std::{os::unix::fs::MetadataExt, os::unix::fs::PermissionsExt};
        let _guard = FAULT_MUTEX.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let lock = RepositoryLock::acquire(temp.path()).unwrap();
        let path = temp.path().join("ewtm/repository.lock");
        let inode = std::fs::metadata(&path).unwrap().ino();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(lock);
        let again = RepositoryLock::acquire(temp.path()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        drop(again);
        let mut store = LockedJournalStore::acquire(temp.path()).unwrap();
        store
            .write_new(&Journal::new(crate::lifecycle::test_plan(1)))
            .unwrap();
        let journal = std::fs::read_dir(temp.path().join("ewtm/journal"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::metadata(journal).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn atomic_faults_clean_temps_and_keep_post_rename_success() {
        use std::sync::atomic::Ordering;
        let _guard = FAULT_MUTEX.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let mut store = LockedJournalStore::acquire(temp.path()).unwrap();
        FAIL_BEFORE_RENAME.store(true, Ordering::SeqCst);
        assert!(matches!(
            store.write_new(&Journal::new(crate::lifecycle::test_plan(1))),
            Err(JournalError::Io(_))
        ));
        assert!(
            std::fs::read_dir(temp.path().join("ewtm/journal"))
                .map(|entries| entries.count())
                .unwrap_or(0)
                == 0
        );
        let journal = Journal::new(crate::lifecycle::test_plan(1));
        FAIL_DIRECTORY_SYNC.store(true, Ordering::SeqCst);
        store.write_new(&journal).unwrap();
        assert_eq!(
            JournalStore::new(temp.path())
                .read(journal.operation_id())
                .unwrap(),
            journal
        );
    }
}
