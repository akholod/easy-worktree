use crate::{journal::Journal, lifecycle::OperationId};
use fs4::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;
use uuid::Uuid;

#[cfg(unix)]
fn has_duplicate_keys(raw: &[u8]) -> bool {
    use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
    struct V;
    impl<'de> DeserializeSeed<'de> for V {
        type Value = bool;
        fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<bool, D::Error> {
            deserializer.deserialize_any(self)
        }
    }
    impl<'de> Visitor<'de> for V {
        type Value = bool;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("json")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<bool, A::Error> {
            let mut keys = std::collections::BTreeSet::new();
            let mut duplicate = false;
            while let Some(key) = access.next_key::<String>()? {
                if !keys.insert(key) {
                    duplicate = true;
                }
                if access.next_value_seed(V)? {
                    duplicate = true;
                }
            }
            Ok(duplicate)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<bool, A::Error> {
            let mut duplicate = false;
            while let Some(value) = access.next_element_seed(V)? {
                if value {
                    duplicate = true;
                }
            }
            Ok(duplicate)
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_borrowed_str<E: serde::de::Error>(self, _: &'de str) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<bool, E> {
            Ok(false)
        }
    }
    serde_json::Deserializer::from_slice(raw)
        .deserialize_any(V)
        .unwrap_or(true)
}

/// The read-only lock/evidence boundary used by compensation.  Unlike
/// `LockedJournalStore`, this type has no write methods and never creates
/// repository state.
#[derive(Debug)]
pub struct LockedForwardEvidence {
    _lock: RepositoryLock,
    journal: Journal,
    raw: Vec<u8>,
}
impl crate::compensation::LockedForwardEvidence for LockedForwardEvidence {
    fn journal(&self) -> &Journal {
        &self.journal
    }
    fn raw_bytes(&self) -> &[u8] {
        &self.raw
    }
}

pub struct JournalEvidencePort;
impl crate::compensation::ForwardEvidencePort for JournalEvidencePort {
    type Guard = LockedForwardEvidence;
    fn acquire(
        &self,
        common_dir: &Path,
        id: &OperationId,
    ) -> Result<Self::Guard, crate::compensation::CompensationError> {
        #[cfg(not(unix))]
        {
            let _ = (common_dir, id);
            return Err(crate::compensation::CompensationError::PlatformUnsupported);
        }
        #[cfg(unix)]
        {
            let lock = RepositoryLock::acquire_read_only(common_dir).map_err(map_evidence_error)?;
            let path = common_dir
                .join("ewtm")
                .join("journal")
                .join(format!("{id}.json"));
            let file = open_no_follow(&path).map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    crate::compensation::CompensationError::ForwardOperationNotFound
                } else {
                    crate::compensation::CompensationError::JournalCorrupt
                }
            })?;
            let (journal, raw) = read_bounded_held_journal(file, id)?;
            Ok(LockedForwardEvidence {
                _lock: lock,
                journal,
                raw,
            })
        }
    }
}

#[cfg(unix)]
fn read_bounded_held_journal(
    file: File,
    id: &OperationId,
) -> Result<(Journal, Vec<u8>), crate::compensation::CompensationError> {
    let meta = file
        .metadata()
        .map_err(|_| crate::compensation::CompensationError::JournalCorrupt)?;
    if !meta.file_type().is_file() {
        return Err(crate::compensation::CompensationError::JournalCorrupt);
    }
    let mut raw = Vec::new();
    file.take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut raw)
        .map_err(|_| crate::compensation::CompensationError::JournalCorrupt)?;
    if raw.len() > 64 * 1024 * 1024 {
        return Err(crate::compensation::CompensationError::JournalCorrupt);
    }
    let value: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|_| crate::compensation::CompensationError::JournalCorrupt)?;
    if has_duplicate_keys(&raw) {
        return Err(crate::compensation::CompensationError::JournalCorrupt);
    }
    let journal: Journal = serde_json::from_slice(&raw)
        .map_err(|_| crate::compensation::CompensationError::JournalCorrupt)?;
    journal
        .validate()
        .map_err(|_| crate::compensation::CompensationError::JournalCorrupt)?;
    if journal.operation_id() != id
        || value
            != serde_json::to_value(&journal)
                .map_err(|_| crate::compensation::CompensationError::JournalCorrupt)?
    {
        return Err(crate::compensation::CompensationError::JournalCorrupt);
    }
    Ok((journal, raw))
}
fn map_evidence_error(error: JournalError) -> crate::compensation::CompensationError {
    match error {
        JournalError::RepositoryBusy => crate::compensation::CompensationError::RepositoryBusy,
        JournalError::NotFound => crate::compensation::CompensationError::ForwardOperationNotFound,
        _ => crate::compensation::CompensationError::JournalCorrupt,
    }
}

#[cfg(test)]
thread_local! {
    static FAIL_BEFORE_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_ON_WRITE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FAIL_DIRECTORY_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(test)]
static FAULT_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn inject_fail_before_rename() {
    FAIL_BEFORE_RENAME.with(|fault| fault.set(true));
}
#[cfg(test)]
pub(crate) fn inject_fail_on_atomic_write(number: usize) {
    FAIL_ON_WRITE.with(|fault| fault.set(number));
}
#[cfg(test)]
fn inject_fail_directory_sync() {
    FAIL_DIRECTORY_SYNC.with(|fault| fault.set(true));
}
#[cfg(test)]
pub(crate) struct TestFaultGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}
#[cfg(test)]
pub(crate) fn test_fault_guard() -> TestFaultGuard {
    let guard = TestFaultGuard {
        _lock: FAULT_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    };
    reset_faults();
    guard
}
#[cfg(test)]
impl Drop for TestFaultGuard {
    fn drop(&mut self) {
        reset_faults();
    }
}
#[cfg(test)]
fn reset_faults() {
    FAIL_BEFORE_RENAME.with(|fault| fault.set(false));
    FAIL_ON_WRITE.with(|fault| fault.set(0));
    FAIL_DIRECTORY_SYNC.with(|fault| fault.set(false));
}

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

#[derive(Debug)]
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
    #[cfg(unix)]
    fn acquire_read_only(common_dir: &Path) -> Result<Self, JournalError> {
        let path = common_dir.join("ewtm").join("repository.lock");
        let file = open_read_only(&path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                JournalError::NotFound
            } else {
                JournalError::Io(e)
            }
        })?;
        if !file.metadata()?.file_type().is_file() {
            return Err(JournalError::Io(io::Error::other("lock is not regular")));
        }
        match FileExt::try_lock(&file) {
            Ok(()) => Ok(Self { file }),
            Err(fs4::TryLockError::WouldBlock) => Err(JournalError::RepositoryBusy),
            Err(fs4::TryLockError::Error(e)) => Err(JournalError::Io(e)),
        }
    }
}

#[cfg(unix)]
fn open_no_follow(path: &Path) -> io::Result<File> {
    open_read_only(path)
}

#[cfg(unix)]
fn open_read_only(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, open};

    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let descriptor = open(path, flags, Mode::empty())
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    Ok(File::from(descriptor))
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
    pub fn read(&self, id: &OperationId) -> Result<Journal, JournalError> {
        self.store.read(id)
    }
    pub fn list(&self) -> Result<Vec<Journal>, JournalError> {
        self.store.list()
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
            if FAIL_BEFORE_RENAME.with(std::cell::Cell::take) || take_write_fault() {
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
#[cfg(test)]
fn take_write_fault() -> bool {
    FAIL_ON_WRITE.with(|fault| {
        let current = fault.get();
        if current == 0 {
            false
        } else {
            fault.set(current - 1);
            current == 1
        }
    })
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
    if FAIL_DIRECTORY_SYNC.with(std::cell::Cell::take) {
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
    use crate::compensation::{ForwardEvidencePort, LockedForwardEvidence};
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
        let _guard = test_fault_guard();
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
    fn archived_schema_one_journal_is_readable_without_mutation() {
        let temp = TempDir::new().unwrap();
        let journal = Journal::new(crate::lifecycle::test_plan(2));
        let operation_id = *journal.operation_id();
        let mut wire = serde_json::to_value(&journal).unwrap();
        wire["plan"]["plan_schema_version"] = serde_json::json!(1);
        let action = &mut wire["plan"]["steps"][1]["action"]["FileArtifact"];
        action.as_object_mut().unwrap().remove("sensitive");
        action.as_object_mut().unwrap().remove("confirm");
        action.as_object_mut().unwrap().remove("mode_policy");
        std::fs::create_dir_all(temp.path().join("ewtm/journal")).unwrap();
        std::fs::write(
            temp.path()
                .join(format!("ewtm/journal/{operation_id}.json")),
            serde_json::to_vec(&wire).unwrap(),
        )
        .unwrap();
        let before = std::fs::read_dir(temp.path().join("ewtm/journal"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        let restored = JournalStore::new(temp.path()).read(&operation_id).unwrap();
        assert_eq!(restored.operation_id(), &operation_id);
        assert!(restored.validate().is_ok());
        assert_eq!(JournalStore::new(temp.path()).list().unwrap().len(), 1);
        let after = std::fs::read_dir(temp.path().join("ewtm/journal"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(before, after);
    }

    #[test]
    fn read_and_list_fail_closed_for_corruption_and_sort_valid_ids() {
        let _guard = test_fault_guard();
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
        let _guard = test_fault_guard();
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
        let _guard = test_fault_guard();
        let temp = TempDir::new().unwrap();
        let mut store = LockedJournalStore::acquire(temp.path()).unwrap();
        inject_fail_before_rename();
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
        inject_fail_directory_sync();
        store.write_new(&journal).unwrap();
        assert_eq!(
            JournalStore::new(temp.path())
                .read(journal.operation_id())
                .unwrap(),
            journal
        );
    }

    #[cfg(unix)]
    fn evidence_fixture(temp: &TempDir) -> (crate::journal::Journal, Vec<u8>) {
        let journal = crate::journal::Journal::new(crate::lifecycle::test_plan(1));
        let raw = serde_json::to_vec(&journal).unwrap();
        std::fs::create_dir_all(temp.path().join("ewtm/journal")).unwrap();
        std::fs::write(
            temp.path()
                .join("ewtm/journal")
                .join(format!("{}.json", journal.operation_id())),
            &raw,
        )
        .unwrap();
        (journal, raw)
    }

    #[cfg(unix)]
    #[test]
    fn evidence_returns_exact_bytes_and_holds_lock_until_drop() {
        let temp = TempDir::new().unwrap();
        let (journal, raw) = evidence_fixture(&temp);
        drop(RepositoryLock::acquire(temp.path()).unwrap());
        let guard = JournalEvidencePort
            .acquire(temp.path(), journal.operation_id())
            .unwrap();
        assert_eq!(guard.raw_bytes(), raw.as_slice());
        assert_eq!(guard.journal(), &journal);
        assert!(matches!(
            RepositoryLock::acquire(temp.path()),
            Err(JournalError::RepositoryBusy)
        ));
        drop(guard);
        assert!(RepositoryLock::acquire(temp.path()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn evidence_missing_lock_or_journal_does_not_create_paths() {
        let temp = TempDir::new().unwrap();
        let missing_id = "00000000-0000-4000-8000-000000000000".parse().unwrap();
        assert_eq!(
            JournalEvidencePort
                .acquire(temp.path(), &missing_id)
                .unwrap_err()
                .code(),
            "forward_operation_not_found"
        );
        assert!(!temp.path().join("ewtm").exists());
        drop(RepositoryLock::acquire(temp.path()).unwrap());
        assert_eq!(
            JournalEvidencePort
                .acquire(temp.path(), &missing_id)
                .unwrap_err()
                .code(),
            "forward_operation_not_found"
        );
        assert!(!temp.path().join("ewtm/journal").exists());
    }

    #[cfg(unix)]
    #[test]
    fn evidence_rejects_symlink_directory_fifo_and_malformed_bytes() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let id = crate::journal::Journal::new(crate::lifecycle::test_plan(1))
            .operation_id()
            .to_owned();
        std::fs::create_dir_all(temp.path().join("ewtm/journal")).unwrap();
        drop(RepositoryLock::acquire(temp.path()).unwrap());
        let target = temp.path().join("target");
        std::fs::write(&target, b"{}").unwrap();
        let journal_path = temp.path().join("ewtm/journal").join(format!("{id}.json"));
        symlink(&target, &journal_path).unwrap();
        assert_eq!(
            JournalEvidencePort
                .acquire(temp.path(), &id)
                .unwrap_err()
                .code(),
            "journal_corrupt"
        );
        std::fs::remove_file(&journal_path).unwrap();
        std::fs::create_dir(&journal_path).unwrap();
        assert_eq!(
            JournalEvidencePort
                .acquire(temp.path(), &id)
                .unwrap_err()
                .code(),
            "journal_corrupt"
        );
        std::fs::remove_dir(&journal_path).unwrap();
        std::fs::write(&journal_path, b"{\"nested\":{\"a\":1,\"a\":2}}").unwrap();
        assert_eq!(
            JournalEvidencePort
                .acquire(temp.path(), &id)
                .unwrap_err()
                .code(),
            "journal_corrupt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn evidence_rejects_more_than_64_mib_without_unlimited_read() {
        let temp = TempDir::new().unwrap();
        let journal = crate::journal::Journal::new(crate::lifecycle::test_plan(1));
        let path = temp.path().join("ewtm/journal");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join(format!("{}.json", journal.operation_id())),
            vec![b' '; 64 * 1024 * 1024 + 1],
        )
        .unwrap();
        drop(RepositoryLock::acquire(temp.path()).unwrap());
        assert_eq!(
            JournalEvidencePort
                .acquire(temp.path(), journal.operation_id())
                .unwrap_err()
                .code(),
            "journal_corrupt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn held_fd_reads_original_after_path_replacement() {
        let temp = TempDir::new().unwrap();
        let (journal, raw) = evidence_fixture(&temp);
        let path = temp
            .path()
            .join("ewtm/journal")
            .join(format!("{}.json", journal.operation_id()));
        let file = open_no_follow(&path).unwrap();
        let replacement = path.with_extension("replacement");
        std::fs::rename(&path, &replacement).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        let (_, held_raw) = read_bounded_held_journal(file, journal.operation_id()).unwrap();
        assert_eq!(held_raw, raw);
    }
}
