use crate::{
    compensation::ProposalId,
    compensation_journal::CompensationJournalV1,
    journal_store::{JournalError, RepositoryLock},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

pub const MAX_COMPENSATION_JOURNAL_BYTES: usize = 64 * 1024 * 1024;

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum StoreFault {
    TempWrite,
    TempFileSync,
    InitialPublish,
    InitialParentSync,
    PostPublishCleanup,
    UpdateRename,
    UpdateParentSync,
}
#[cfg(not(test))]
#[derive(Clone, Copy)]
#[allow(dead_code)]
enum StoreFault {
    TempWrite,
    TempFileSync,
    InitialPublish,
    InitialParentSync,
    PostPublishCleanup,
    UpdateRename,
    UpdateParentSync,
}
#[cfg(test)]
thread_local! { static STORE_FAULT: std::cell::Cell<Option<StoreFault>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
pub(crate) fn inject_fault(fault: StoreFault) {
    STORE_FAULT.with(|slot| slot.set(Some(fault)));
}
#[cfg(test)]
pub(crate) fn inject_fail_before_publish() {
    inject_fault(StoreFault::InitialPublish);
}
#[cfg(test)]
fn take_fault(wanted: StoreFault) -> bool {
    STORE_FAULT.with(|slot| {
        if slot.get().is_some_and(|fault| fault as u8 == wanted as u8) {
            slot.take();
            true
        } else {
            false
        }
    })
}
#[cfg(not(test))]
fn take_fault(_wanted: StoreFault) -> bool {
    false
}

#[derive(Debug)]
pub enum CompensationStoreError {
    AlreadyUsed,
    NotFound,
    Corrupt(String),
    RevisionConflict,
    Io(io::Error),
    CommitUncertain,
    TooLarge,
}
impl From<io::Error> for CompensationStoreError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
pub struct CompensationStore {
    dir: PathBuf,
    max_bytes: usize,
}
impl CompensationStore {
    pub fn new(common: &Path) -> Self {
        Self {
            dir: common.join("ewtm/compensation/v1/operations"),
            max_bytes: MAX_COMPENSATION_JOURNAL_BYTES,
        }
    }
    #[cfg(test)]
    fn with_max_bytes(common: &Path, max_bytes: usize) -> Self {
        Self {
            dir: common.join("ewtm/compensation/v1/operations"),
            max_bytes,
        }
    }
    #[allow(dead_code)]
    fn prepare(common: &Path) -> io::Result<()> {
        RepositoryLock::prepare_ewtm(common)
    }
    fn path(&self, id: &ProposalId) -> PathBuf {
        self.dir
            .join(crate::compensation_authority::proposal_filename(id))
    }
    pub fn read(&self, id: &ProposalId) -> Result<CompensationJournalV1, CompensationStoreError> {
        validate_namespace(&self.dir)?;
        let file = open_held(&self.path(id))?;
        let mut b = Vec::new();
        file.take(self.max_bytes as u64 + 1).read_to_end(&mut b)?;
        if b.len() > self.max_bytes {
            return Err(CompensationStoreError::TooLarge);
        };
        if crate::compensation_authority::has_duplicate_keys(&b) {
            return Err(CompensationStoreError::Corrupt("duplicate keys".into()));
        }
        let original: serde_json::Value = serde_json::from_slice(&b)
            .map_err(|e| CompensationStoreError::Corrupt(e.to_string()))?;
        if !original.is_object() {
            return Err(CompensationStoreError::Corrupt(
                "journal is not an object".into(),
            ));
        };
        let value: CompensationJournalV1 = serde_json::from_slice(&b)
            .map_err(|e| CompensationStoreError::Corrupt(e.to_string()))?;
        value.validate().map_err(CompensationStoreError::Corrupt)?;
        if serde_json::to_value(&value)
            .map_err(|e| CompensationStoreError::Corrupt(e.to_string()))?
            != original
            || value.proposal_id() != id
        {
            return Err(CompensationStoreError::Corrupt(
                "noncanonical journal".into(),
            ));
        }
        Ok(value)
    }
    pub fn list(&self) -> Result<Vec<CompensationJournalV1>, CompensationStoreError> {
        match fs::symlink_metadata(&self.dir) {
            Ok(_) => validate_namespace(&self.dir)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let entries = fs::read_dir(&self.dir)?;
        let mut out = Vec::new();
        for e in entries {
            let p = e?.path();
            let name = p
                .file_name()
                .and_then(|x| x.to_str())
                .ok_or_else(|| CompensationStoreError::Corrupt("invalid filename".into()))?;
            if is_internal_temp(name) {
                continue;
            }
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                return Err(CompensationStoreError::Corrupt("invalid filename".into()));
            };
            let stem = p
                .file_stem()
                .and_then(|x| x.to_str())
                .ok_or_else(|| CompensationStoreError::Corrupt("invalid filename".into()))?;
            let id: ProposalId = stem
                .parse()
                .map_err(|_| CompensationStoreError::Corrupt("invalid filename".into()))?;
            if stem != id.to_string() {
                return Err(CompensationStoreError::Corrupt(
                    "noncanonical filename".into(),
                ));
            };
            out.push(self.read(&id)?)
        }
        out.sort_by_key(|x| x.proposal_id().to_string());
        Ok(out)
    }
    fn create_initial(
        &self,
        loaded: &crate::compensation_authority::LoadedProposal,
    ) -> Result<CompensationJournalV1, CompensationStoreError> {
        let journal =
            CompensationJournalV1::from_loaded(loaded).map_err(CompensationStoreError::Corrupt)?;
        let bytes = serialize_bounded(&journal, self.max_bytes)?;
        self.create_bytes(&journal, &bytes)?;
        Ok(journal)
    }

    #[cfg(test)]
    fn create(&self, j: &CompensationJournalV1) -> Result<(), CompensationStoreError> {
        j.validate().map_err(CompensationStoreError::Corrupt)?;
        if !j.is_canonical_initial() {
            return Err(CompensationStoreError::Corrupt(
                "initial journal is not canonical".into(),
            ));
        }
        let bytes = serialize_bounded(j, self.max_bytes)?;
        self.create_bytes(j, &bytes)
    }

    fn create_bytes(
        &self,
        j: &CompensationJournalV1,
        bytes: &[u8],
    ) -> Result<(), CompensationStoreError> {
        let path = self.path(j.proposal_id());
        let temp = self
            .dir
            .join(format!(".{}.{}.tmp", j.proposal_id(), uuid::Uuid::new_v4()));
        let result = (|| {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            private_file(&f)?;
            write_durable(&mut f, bytes)?;
            if take_fault(StoreFault::InitialPublish) {
                return Err(CompensationStoreError::Io(io::Error::other(
                    "injected publish fault",
                )));
            }
            fs::hard_link(&temp, &path).map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    CompensationStoreError::AlreadyUsed
                } else {
                    e.into()
                }
            })?;
            if take_fault(StoreFault::InitialParentSync) {
                return Err(CompensationStoreError::CommitUncertain);
            }
            sync_parent(&self.dir).map_err(|_| CompensationStoreError::CommitUncertain)?;
            if take_fault(StoreFault::PostPublishCleanup) {
                return Ok(());
            }
            let _ = fs::remove_file(&temp);
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
    fn update(
        &self,
        previous: &CompensationJournalV1,
        next: CompensationJournalV1,
    ) -> Result<(), CompensationStoreError> {
        let checked = previous
            .successor(next)
            .map_err(CompensationStoreError::Corrupt)?;
        let current = self.read(previous.proposal_id())?;
        if current != *previous {
            return Err(CompensationStoreError::RevisionConflict);
        };
        let bytes = serialize_bounded(&checked, self.max_bytes)?;
        self.atomic_replace(&checked, &bytes)
    }
    fn atomic_replace(
        &self,
        j: &CompensationJournalV1,
        bytes: &[u8],
    ) -> Result<(), CompensationStoreError> {
        let temp = self
            .dir
            .join(format!(".{}.{}.tmp", j.proposal_id(), uuid::Uuid::new_v4()));
        let result = (|| {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            private_file(&f)?;
            write_durable(&mut f, bytes)?;
            if take_fault(StoreFault::UpdateRename) {
                return Err(CompensationStoreError::Io(io::Error::other(
                    "injected rename fault",
                )));
            }
            fs::rename(&temp, self.path(j.proposal_id()))?;
            if take_fault(StoreFault::UpdateParentSync) {
                return Err(CompensationStoreError::CommitUncertain);
            }
            sync_parent(&self.dir).map_err(|_| CompensationStoreError::CommitUncertain)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}
pub struct LockedCompensationStore {
    _lock: RepositoryLock,
    store: CompensationStore,
}
impl LockedCompensationStore {
    pub fn acquire(common: &Path) -> Result<Self, JournalError> {
        let lock = RepositoryLock::acquire(common)?;
        prepare_namespace(common).map_err(JournalError::Io)?;
        Ok(Self {
            _lock: lock,
            store: CompensationStore::new(common),
        })
    }
    #[cfg(test)]
    fn acquire_with_max_bytes(common: &Path, max_bytes: usize) -> Result<Self, JournalError> {
        let lock = RepositoryLock::acquire(common)?;
        prepare_namespace(common).map_err(JournalError::Io)?;
        Ok(Self {
            _lock: lock,
            store: CompensationStore::with_max_bytes(common, max_bytes),
        })
    }
    pub fn create_initial(
        &self,
        loaded: &crate::compensation_authority::LoadedProposal,
    ) -> Result<CompensationJournalV1, CompensationStoreError> {
        self.store.create_initial(loaded)
    }
    #[cfg(test)]
    pub(crate) fn create(
        &self,
        journal: &CompensationJournalV1,
    ) -> Result<(), CompensationStoreError> {
        self.store.create(journal)
    }
    pub fn update(
        &self,
        previous: &CompensationJournalV1,
        next: CompensationJournalV1,
    ) -> Result<(), CompensationStoreError> {
        self.store.update(previous, next)
    }
    pub fn read(&self, id: &ProposalId) -> Result<CompensationJournalV1, CompensationStoreError> {
        self.store.read(id)
    }
    pub fn list(&self) -> Result<Vec<CompensationJournalV1>, CompensationStoreError> {
        self.store.list()
    }
}
fn serialize_bounded(
    j: &CompensationJournalV1,
    max_bytes: usize,
) -> Result<Vec<u8>, CompensationStoreError> {
    let bytes =
        serde_json::to_vec_pretty(j).map_err(|e| CompensationStoreError::Corrupt(e.to_string()))?;
    if bytes.len() > max_bytes {
        return Err(CompensationStoreError::TooLarge);
    }
    Ok(bytes)
}

fn write_durable(f: &mut File, bytes: &[u8]) -> Result<(), CompensationStoreError> {
    #[cfg(test)]
    if take_fault(StoreFault::TempWrite) {
        return Err(CompensationStoreError::Io(io::Error::other(
            "injected temp write fault",
        )));
    }
    f.write_all(bytes)?;
    f.flush()?;
    #[cfg(test)]
    if take_fault(StoreFault::TempFileSync) {
        return Err(CompensationStoreError::Io(io::Error::other(
            "injected temp sync fault",
        )));
    }
    f.sync_all()?;
    Ok(())
}
fn validate_namespace(p: &Path) -> Result<(), CompensationStoreError> {
    let mut current = Some(p);
    for _ in 0..4 {
        if let Some(path) = current {
            match fs::symlink_metadata(path) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(CompensationStoreError::Corrupt("symlink namespace".into()));
                }
                Ok(m) if !m.file_type().is_dir() => {
                    return Err(CompensationStoreError::Corrupt(
                        "non-directory namespace".into(),
                    ));
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            current = path.parent();
        }
    }
    Ok(())
}
#[cfg(unix)]
fn prepare_namespace(common: &Path) -> io::Result<()> {
    use rustix::fs::{Mode, OFlags, fchmod, fsync, mkdirat, open, openat};
    let flags = OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::RDONLY;
    let mut parent = open(common, flags, Mode::empty()).map_err(io::Error::from)?;
    for name in ["ewtm", "compensation", "v1", "operations"] {
        let child = match openat(&parent, name, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(error) if error == rustix::io::Errno::NOENT => {
                mkdirat(&parent, name, Mode::from_raw_mode(0o700)).map_err(io::Error::from)?;
                fsync(&parent).map_err(io::Error::from)?;
                openat(&parent, name, flags, Mode::empty()).map_err(io::Error::from)?
            }
            Err(error) => return Err(io::Error::from(error)),
        };
        fchmod(&child, Mode::from_raw_mode(0o700)).map_err(io::Error::from)?;
        fsync(&child).map_err(io::Error::from)?;
        parent = child;
    }
    Ok(())
}
#[cfg(not(unix))]
fn prepare_namespace(_common: &Path) -> io::Result<()> {
    Err(io::Error::other("safe namespace creation unavailable"))
}
fn private_file(f: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = f;
        Ok(())
    }
}
fn sync_parent(p: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(p)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        Ok(())
    }
}
fn is_internal_temp(name: &str) -> bool {
    let Some(body) = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    let mut parts = body.split('.');
    let Some(proposal) = parts.next() else {
        return false;
    };
    let Some(nonce) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    if proposal
        .parse::<ProposalId>()
        .map_or(true, |id| id.to_string() != proposal)
    {
        return false;
    }
    let Ok(uuid) = nonce.parse::<uuid::Uuid>() else {
        return false;
    };
    uuid.get_version_num() == 4 && uuid.hyphenated().to_string() == nonce
}
fn open_held(path: &Path) -> Result<File, CompensationStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let flags = (rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC)
            .bits() as i32;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(path)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    CompensationStoreError::NotFound
                } else {
                    CompensationStoreError::Corrupt(e.to_string())
                }
            })?;
        if !file
            .metadata()
            .map_err(|e| CompensationStoreError::Corrupt(e.to_string()))?
            .is_file()
        {
            return Err(CompensationStoreError::Corrupt("not regular".into()));
        };
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(CompensationStoreError::Corrupt(
            "platform lacks no-follow reads".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compensation::Sha256Digest, compensation_journal::CompensationJournalV1};
    use sha2::Digest;
    use tempfile::tempdir;

    fn journal(id: &str) -> CompensationJournalV1 {
        let mut proposal = crate::compensation_journal::test_sample();
        proposal.proposal_id = id.parse().unwrap();
        CompensationJournalV1::new(proposal, Sha256Digest::new("b".repeat(64)).unwrap()).unwrap()
    }

    fn loaded() -> crate::compensation_authority::LoadedProposal {
        let proposal = crate::compensation_journal::test_sample();
        let raw = serde_json::to_vec(&proposal).unwrap();
        let confirmation = format!("{:x}", sha2::Sha256::digest(&raw));
        crate::compensation_authority::load_bytes(raw, &confirmation).unwrap()
    }

    #[test]
    fn missing_read_and_list_do_not_create_namespace() {
        let dir = tempdir().unwrap();
        let store = CompensationStore::new(dir.path());
        let id = *journal("00000000-0000-4000-8000-000000000000").proposal_id();
        assert!(matches!(
            store.read(&id),
            Err(CompensationStoreError::NotFound)
        ));
        assert!(store.list().unwrap().is_empty());
        assert!(!dir.path().join("ewtm").exists());
    }

    #[test]
    fn initial_store_consumes_one_paired_loaded_proposal_and_digest() {
        let dir = tempdir().unwrap();
        let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
        let first = loaded();
        let journal = locked.create_initial(&first).unwrap();
        assert_eq!(journal.proposal(), first.proposal());
        assert_eq!(journal.proposal_sha256(), first.raw_sha256());
        assert_eq!(locked.read(journal.proposal_id()).unwrap(), journal);

        let second_raw = [first.raw_bytes(), b" "].concat();
        let second_confirmation = format!("{:x}", sha2::Sha256::digest(&second_raw));
        let second =
            crate::compensation_authority::load_bytes(second_raw, &second_confirmation).unwrap();
        assert_eq!(second.proposal(), first.proposal());
        assert_ne!(second.raw_sha256(), first.raw_sha256());
        assert!(matches!(
            locked.create_initial(&second),
            Err(CompensationStoreError::AlreadyUsed)
        ));
    }

    #[test]
    fn bounded_store_closes_serialized_write_and_read_size() {
        let proposal = loaded();
        let journal = CompensationJournalV1::from_loaded(&proposal).unwrap();
        let serialized_len = serde_json::to_vec_pretty(&journal).unwrap().len();

        let dir = tempdir().unwrap();
        let locked =
            LockedCompensationStore::acquire_with_max_bytes(dir.path(), serialized_len - 1)
                .unwrap();
        assert!(matches!(
            locked.create_initial(&proposal),
            Err(CompensationStoreError::TooLarge)
        ));
        assert_eq!(
            fs::read_dir(dir.path().join("ewtm/compensation/v1/operations"))
                .unwrap()
                .count(),
            0
        );

        let dir = tempdir().unwrap();
        let locked =
            LockedCompensationStore::acquire_with_max_bytes(dir.path(), serialized_len).unwrap();
        let initial = locked.create_initial(&proposal).unwrap();
        assert_eq!(locked.read(initial.proposal_id()).unwrap(), initial);

        let dir = tempdir().unwrap();
        let mut locked =
            LockedCompensationStore::acquire_with_max_bytes(dir.path(), serialized_len).unwrap();
        let initial = locked.create_initial(&proposal).unwrap();
        let next = initial.start_next().unwrap();
        let next_len = serde_json::to_vec_pretty(&next).unwrap().len();
        locked.store.max_bytes = next_len - 1;
        let path = dir.path().join(format!(
            "ewtm/compensation/v1/operations/{}.json",
            initial.proposal_id()
        ));
        let before = fs::read(&path).unwrap();
        assert!(matches!(
            locked.update(&initial, next),
            Err(CompensationStoreError::TooLarge)
        ));
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn locked_create_read_update_and_sorted_list() {
        let dir = tempdir().unwrap();
        let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
        let first = journal("00000000-0000-4000-8000-000000000001");
        let second = journal("00000000-0000-4000-8000-000000000000");
        locked.create(&first).unwrap();
        locked.create(&second).unwrap();
        assert_eq!(
            locked
                .list()
                .unwrap()
                .iter()
                .map(|j| j.proposal_id().to_string())
                .collect::<Vec<_>>(),
            vec![
                second.proposal_id().to_string(),
                first.proposal_id().to_string()
            ]
        );
        let started = first.start_next().unwrap();
        locked.update(&first, started.clone()).unwrap();
        assert_eq!(locked.read(first.proposal_id()).unwrap(), started);
        assert!(matches!(
            locked.update(&first, started.clone()),
            Err(CompensationStoreError::RevisionConflict)
        ));
    }

    #[test]
    fn locked_store_persists_full_three_step_lifecycle() {
        let dir = tempdir().unwrap();
        let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
        let initial = CompensationJournalV1::new(
            crate::compensation_journal::test_three_step(),
            Sha256Digest::new("b".repeat(64)).unwrap(),
        )
        .unwrap();
        locked.create(&initial).unwrap();
        let mut current = initial;
        for expected_revision in 1..=6 {
            let next =
                if current.status() == crate::compensation_journal::CompensationStatus::Pending {
                    current.start_next().unwrap()
                } else {
                    current.apply_started().unwrap()
                };
            locked.update(&current, next.clone()).unwrap();
            assert_eq!(
                locked.read(current.proposal_id()).unwrap().revision(),
                expected_revision
            );
            current = next;
        }
        assert_eq!(
            current.status(),
            crate::compensation_journal::CompensationStatus::Applied
        );
    }

    #[test]
    fn create_is_single_use_for_existing_corrupt_symlink_and_directory() {
        let cases = ["corrupt", "symlink", "directory"];
        for case in cases {
            let dir = tempdir().unwrap();
            let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
            let value = journal("00000000-0000-4000-8000-000000000000");
            let target = dir
                .path()
                .join("ewtm/compensation/v1/operations/00000000-0000-4000-8000-000000000000.json");
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            match case {
                "corrupt" => {
                    fs::write(&target, b"broken").unwrap();
                }
                "directory" => {
                    fs::create_dir(&target).unwrap();
                }
                "symlink" => {
                    let other = dir.path().join("other");
                    fs::write(&other, b"broken").unwrap();
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(other, &target).unwrap();
                    #[cfg(not(unix))]
                    continue;
                }
                _ => unreachable!(),
            }
            assert!(matches!(
                locked.create(&value),
                Err(CompensationStoreError::AlreadyUsed)
            ));
        }
    }

    #[test]
    fn create_accepts_only_canonical_pending_revision_zero() {
        let mut states = Vec::new();
        let initial = journal("00000000-0000-4000-8000-000000000000");
        states.push(initial.start_next().unwrap());
        let pending = initial.start_next().unwrap().apply_started().unwrap();
        states.push(pending);
        let applied = initial.start_next().unwrap().apply_started().unwrap();
        states.push(applied);
        states.push(
            initial
                .attention(
                    crate::compensation_journal::AttentionKind::PreStartedAbsent,
                    false,
                    None,
                )
                .unwrap(),
        );
        states.push(
            initial
                .start_next()
                .unwrap()
                .attention(
                    crate::compensation_journal::AttentionKind::EffectUnknown,
                    true,
                    None,
                )
                .unwrap(),
        );

        for state in states {
            let dir = tempdir().unwrap();
            let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
            assert!(matches!(
                locked.create(&state),
                Err(CompensationStoreError::Corrupt(_))
            ));
            assert_eq!(
                fs::read_dir(dir.path().join("ewtm/compensation/v1/operations"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }

    #[test]
    fn prepublication_fault_leaves_no_final_target_or_temp() {
        let dir = tempdir().unwrap();
        let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
        let value = journal("00000000-0000-4000-8000-000000000000");
        inject_fail_before_publish();
        assert!(matches!(
            locked.create(&value),
            Err(CompensationStoreError::Io(_))
        ));
        let operations = dir.path().join("ewtm/compensation/v1/operations");
        assert!(
            !operations
                .join("00000000-0000-4000-8000-000000000000.json")
                .exists()
        );
        assert_eq!(fs::read_dir(operations).unwrap().count(), 0);
    }

    #[test]
    fn initial_fault_matrix_has_single_use_and_cleanup_semantics() {
        let prepublication = [
            StoreFault::TempWrite,
            StoreFault::TempFileSync,
            StoreFault::InitialPublish,
        ];
        for fault in prepublication {
            let dir = tempdir().unwrap();
            let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
            let value = journal("00000000-0000-4000-8000-000000000000");
            inject_fault(fault);
            assert!(matches!(
                locked.create(&value),
                Err(CompensationStoreError::Io(_))
            ));
            let operations = dir.path().join("ewtm/compensation/v1/operations");
            assert_eq!(fs::read_dir(operations).unwrap().count(), 0);
        }

        let dir = tempdir().unwrap();
        let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
        let value = journal("00000000-0000-4000-8000-000000000000");
        inject_fault(StoreFault::InitialParentSync);
        assert!(matches!(
            locked.create(&value),
            Err(CompensationStoreError::CommitUncertain)
        ));
        assert_eq!(locked.read(value.proposal_id()).unwrap(), value);
        assert!(matches!(
            locked.create(&value),
            Err(CompensationStoreError::AlreadyUsed)
        ));

        let dir = tempdir().unwrap();
        let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
        let value = journal("00000000-0000-4000-8000-000000000000");
        inject_fault(StoreFault::PostPublishCleanup);
        assert!(locked.create(&value).is_ok());
        assert_eq!(locked.list().unwrap().len(), 1);
        assert!(
            fs::read_dir(dir.path().join("ewtm/compensation/v1/operations"))
                .unwrap()
                .count()
                > 1
        );
    }

    #[test]
    fn update_fault_matrix_preserves_previous_or_reports_actual_next() {
        for fault in [
            StoreFault::TempWrite,
            StoreFault::TempFileSync,
            StoreFault::UpdateRename,
        ] {
            let dir = tempdir().unwrap();
            let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
            let initial = journal("00000000-0000-4000-8000-000000000000");
            locked.create(&initial).unwrap();
            let next = initial.start_next().unwrap();
            let path = dir
                .path()
                .join("ewtm/compensation/v1/operations/00000000-0000-4000-8000-000000000000.json");
            let before = fs::read(&path).unwrap();
            inject_fault(fault);
            assert!(matches!(
                locked.update(&initial, next),
                Err(CompensationStoreError::Io(_))
            ));
            assert_eq!(fs::read(path).unwrap(), before);
        }

        let dir = tempdir().unwrap();
        let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
        let initial = journal("00000000-0000-4000-8000-000000000000");
        locked.create(&initial).unwrap();
        let next = initial.start_next().unwrap();
        inject_fault(StoreFault::UpdateParentSync);
        assert!(matches!(
            locked.update(&initial, next.clone()),
            Err(CompensationStoreError::CommitUncertain)
        ));
        assert_eq!(locked.read(initial.proposal_id()).unwrap(), next);
        assert!(matches!(
            locked.update(&initial, next),
            Err(CompensationStoreError::RevisionConflict)
        ));
    }

    #[test]
    fn bounded_corrupt_reads_and_canonical_names_fail_closed() {
        let dir = tempdir().unwrap();
        let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
        let value = journal("00000000-0000-4000-8000-000000000000");
        let operations = dir.path().join("ewtm/compensation/v1/operations");
        let path = operations.join("00000000-0000-4000-8000-000000000000.json");
        fs::write(&path, vec![b'x'; MAX_COMPENSATION_JOURNAL_BYTES + 1]).unwrap();
        assert!(matches!(
            locked.read(value.proposal_id()),
            Err(CompensationStoreError::TooLarge)
        ));
        fs::remove_file(path).unwrap();
        fs::write(operations.join("not-an-id.json"), b"{}").unwrap();
        assert!(matches!(
            locked.list(),
            Err(CompensationStoreError::Corrupt(_))
        ));
        fs::remove_file(operations.join("not-an-id.json")).unwrap();
        fs::write(
            operations.join(
                ".00000000-0000-4000-8000-000000000000.00000000-0000-1000-8000-000000000000.tmp",
            ),
            b"stale",
        )
        .unwrap();
        assert!(matches!(
            locked.list(),
            Err(CompensationStoreError::Corrupt(_))
        ));
    }

    #[test]
    fn strict_reads_bind_filename_and_reject_nested_duplicate_unknown_trailing_and_envelope() {
        let dir = tempdir().unwrap();
        let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
        let value = journal("00000000-0000-4000-8000-000000000000");
        let operations = dir.path().join("ewtm/compensation/v1/operations");
        let canonical = serde_json::to_string(&value).unwrap();
        let cases = [
            ("00000000-0000-4000-8000-000000000001.json", canonical.clone()),
            ("00000000-0000-4000-8000-000000000000.json", canonical.replacen("\"proposal_id\":\"00000000-0000-4000-8000-000000000000\"", "\"proposal_id\":\"00000000-0000-4000-8000-000000000000\",\"proposal_id\":\"00000000-0000-4000-8000-000000000000\"", 1)),
            ("00000000-0000-4000-8000-000000000000.json", canonical.replace("\"branch_was_created\":false", "\"branch_was_created\":false,\"unknown\":true")),
            ("00000000-0000-4000-8000-000000000000.json", format!("{canonical} {{}}")),
            ("00000000-0000-4000-8000-000000000000.json", "[]".to_owned()),
        ];
        for (filename, bytes) in cases {
            let path = operations.join(filename);
            fs::write(&path, bytes).unwrap();
            let requested: ProposalId = filename.trim_end_matches(".json").parse().unwrap();
            assert!(matches!(
                locked.read(&requested),
                Err(CompensationStoreError::Corrupt(_))
            ));
            let _ = fs::remove_file(path);
        }
    }

    #[cfg(unix)]
    #[test]
    fn fifo_read_is_nonblocking_and_fail_closed() {
        let dir = tempdir().unwrap();
        let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
        let id = *journal("00000000-0000-4000-8000-000000000000").proposal_id();
        let path = dir
            .path()
            .join("ewtm/compensation/v1/operations/00000000-0000-4000-8000-000000000000.json");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert!(matches!(
            locked.read(&id),
            Err(CompensationStoreError::Corrupt(_))
        ));
    }

    #[test]
    fn compensation_and_forward_stores_share_repository_lock() {
        let dir = tempdir().unwrap();
        let forward = crate::journal_store::RepositoryLock::acquire(dir.path()).unwrap();
        assert!(matches!(
            LockedCompensationStore::acquire(dir.path()),
            Err(JournalError::RepositoryBusy)
        ));
        assert!(!dir.path().join("ewtm/compensation").exists());
        drop(forward);
        assert!(LockedCompensationStore::acquire(dir.path()).is_ok());
    }

    #[test]
    fn compensation_operations_leave_forward_journal_bytes_identical() {
        let dir = tempdir().unwrap();
        let forward = crate::journal::Journal::new(crate::lifecycle::test_plan(1));
        {
            let mut store = crate::journal_store::LockedJournalStore::acquire(dir.path()).unwrap();
            store.write_new(&forward).unwrap();
        }
        let forward_path = dir
            .path()
            .join(format!("ewtm/journal/{}.json", forward.operation_id()));
        let before = fs::read(&forward_path).unwrap();
        let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
        let initial = journal("00000000-0000-4000-8000-000000000000");
        locked.create(&initial).unwrap();
        let next = initial.start_next().unwrap();
        locked.update(&initial, next).unwrap();
        assert_eq!(fs::read(forward_path).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let locked = LockedCompensationStore::acquire(dir.path()).unwrap();
        let value = journal("00000000-0000-4000-8000-000000000000");
        locked.create(&value).unwrap();
        let root = dir.path().join("ewtm/compensation/v1/operations");
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(root.join("00000000-0000-4000-8000-000000000000.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn intermediate_symlink_is_rejected_without_touching_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let ewtm = dir.path().join("ewtm");
        symlink(outside.path(), &ewtm).unwrap();
        let before = fs::metadata(outside.path()).unwrap().permissions().mode();
        assert!(LockedCompensationStore::acquire(dir.path()).is_err());
        assert_eq!(
            fs::metadata(outside.path()).unwrap().permissions().mode(),
            before
        );
        assert!(!outside.path().join("compensation").exists());
    }
}
