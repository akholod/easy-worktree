//! The production compensation effect boundary.  This is intentionally not
//! shared with the forward mutation backend: compensation has narrower leases.

use crate::{
    compensation::{CompensationActionV1, ObservedArtifactState},
    compensation_authority::LoadedProposal,
    compensation_backend::{
        ArtifactObservation, BranchObservation, CapabilityRefusal, CompensationBackend,
        WorktreeObservation,
    },
    infrastructure::{self, GitError},
    lifecycle::{
        ArtifactStateV3, CreatedArtifactV3, CreatedLocalBranch, CreatedWorktree, RepositoryIdentity,
    },
};
#[cfg(unix)]
use sha2::Digest;
#[cfg(all(unix, test))]
use std::cell::RefCell;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::{
    io,
    path::{Path, PathBuf},
    process::Command,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProductionCompensationBackendError {
    #[error("compensation path is invalid")]
    InvalidPath,
    #[error("compensation observation failed")]
    Observation,
    #[error("compensation effect failed")]
    Effect,
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone)]
pub struct ProductionCompensationBackend {
    request_anchor: PathBuf,
    git_anchor: PathBuf,
    actions: Option<Vec<CompensationActionV1>>,
    branch_policy: Option<BranchCheckoutPolicy>,
}

#[derive(Debug, Clone)]
enum BranchCheckoutPolicy {
    DerivedPredecessor {
        worktree: CreatedWorktree,
        branch: CreatedLocalBranch,
    },
}

impl ProductionCompensationBackend {
    pub fn new(anchor: PathBuf) -> Self {
        Self {
            request_anchor: anchor.clone(),
            git_anchor: anchor,
            actions: None,
            branch_policy: None,
        }
    }

    pub fn for_authority(
        anchor: PathBuf,
        authority: &LoadedProposal,
    ) -> Result<Self, ProductionCompensationBackendError> {
        authority
            .proposal()
            .validate()
            .map_err(|_| ProductionCompensationBackendError::InvalidPath)?;
        let discovered = infrastructure::readonly_repository_identity(&anchor)?;
        if discovered != authority.proposal().repository {
            return Err(ProductionCompensationBackendError::InvalidPath);
        }
        let actions = authority
            .proposal()
            .steps
            .iter()
            .map(|step| step.action.clone())
            .collect::<Vec<_>>();
        let mut policy = None;
        for (index, step) in authority.proposal().steps.iter().enumerate() {
            match &step.action {
                CompensationActionV1::RemoveCreatedWorktree(worktree)
                    if worktree.branch_was_created =>
                {
                    let Some(next) = authority.proposal().steps.get(index + 1) else {
                        return Err(ProductionCompensationBackendError::InvalidPath);
                    };
                    let CompensationActionV1::DeleteCreatedLocalBranch(branch) = &next.action
                    else {
                        return Err(ProductionCompensationBackendError::InvalidPath);
                    };
                    if next.forward_step_id != step.forward_step_id
                        || branch.branch != worktree.branch
                        || branch.expected_oid != worktree.expected_oid
                    {
                        return Err(ProductionCompensationBackendError::InvalidPath);
                    }
                    if policy.is_some() {
                        return Err(ProductionCompensationBackendError::InvalidPath);
                    }
                    policy = Some(BranchCheckoutPolicy::DerivedPredecessor {
                        worktree: worktree.clone(),
                        branch: branch.clone(),
                    });
                }
                CompensationActionV1::RemoveCreatedWorktree(_) => {}
                CompensationActionV1::DeleteCreatedLocalBranch(branch) => {
                    let valid = index > 0
                        && matches!(&authority.proposal().steps[index - 1].action, CompensationActionV1::RemoveCreatedWorktree(worktree) if worktree.branch_was_created && authority.proposal().steps[index - 1].forward_step_id == step.forward_step_id && worktree.branch == branch.branch && worktree.expected_oid == branch.expected_oid);
                    if !valid {
                        return Err(ProductionCompensationBackendError::InvalidPath);
                    }
                }
                CompensationActionV1::RemoveCreatedArtifactV3(_) => {}
            }
        }
        Ok(Self {
            request_anchor: anchor,
            git_anchor: authority
                .proposal()
                .repository
                .primary_root
                .as_path()
                .to_owned(),
            actions: Some(actions),
            branch_policy: policy,
        })
    }

    fn trusted(&self, anchor: &Path) -> Result<(), ProductionCompensationBackendError> {
        if !same_path(&self.request_anchor, anchor) {
            return Err(ProductionCompensationBackendError::InvalidPath);
        }
        Ok(())
    }

    fn observe_branch_strict(
        &self,
        value: &CreatedLocalBranch,
    ) -> Result<BranchObservation, ProductionCompensationBackendError> {
        #[cfg(not(unix))]
        {
            let _ = value;
            return Err(ProductionCompensationBackendError::Observation);
        }
        #[cfg(unix)]
        {
            let valid = Command::new("git")
                .current_dir(&self.git_anchor)
                .args(["check-ref-format", "--branch", value.branch.as_str()])
                .output()?;
            if !valid.status.success() {
                return Ok(BranchObservation::Drift);
            }
            if infrastructure::readonly_ref_is_symbolic(
                &self.git_anchor,
                &format!("refs/heads/{}", value.branch),
            )? {
                return Ok(BranchObservation::Drift);
            }
            let listed = infrastructure::readonly_list(&self.git_anchor)?;
            if listed
                .data
                .worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some(value.branch.as_str()))
            {
                return Ok(BranchObservation::Drift);
            }
            Ok(
                match infrastructure::readonly_direct_ref_oid(
                    &self.git_anchor,
                    &format!("refs/heads/{}", value.branch),
                )? {
                    Some(oid) if oid == value.expected_oid => BranchObservation::BeforeExact,
                    None => BranchObservation::AfterExact,
                    _ => BranchObservation::Drift,
                },
            )
        }
    }

    fn observe_branch_contextual(
        &self,
        value: &CreatedLocalBranch,
        policy: &BranchCheckoutPolicy,
    ) -> Result<BranchObservation, ProductionCompensationBackendError> {
        #[cfg(not(unix))]
        {
            let _ = (value, policy);
            return Err(ProductionCompensationBackendError::Observation);
        }
        #[cfg(unix)]
        {
            let BranchCheckoutPolicy::DerivedPredecessor { worktree, branch } = policy;
            if branch != value {
                return self.observe_branch_strict(value);
            }
            let valid = Command::new("git")
                .current_dir(&self.git_anchor)
                .args(["check-ref-format", "--branch", value.branch.as_str()])
                .output()?;
            if !valid.status.success() {
                return Ok(BranchObservation::Drift);
            }
            let reference = format!("refs/heads/{}", value.branch);
            if infrastructure::readonly_ref_is_symbolic(&self.git_anchor, &reference)? {
                return Ok(BranchObservation::Drift);
            }
            let oid = infrastructure::readonly_direct_ref_oid(&self.git_anchor, &reference)?;
            let listed = infrastructure::readonly_list(&self.git_anchor)?;
            let checkouts: Vec<_> = listed
                .data
                .worktrees
                .iter()
                .filter(|w| w.branch.as_deref() == Some(value.branch.as_str()))
                .collect();
            let Some(oid) = oid else {
                return Ok(if checkouts.is_empty() {
                    BranchObservation::AfterExact
                } else {
                    BranchObservation::Drift
                });
            };
            if oid != value.expected_oid {
                return Ok(BranchObservation::Drift);
            }
            if checkouts.is_empty() {
                return Ok(BranchObservation::BeforeExact);
            }
            if checkouts.len() != 1 || !same_path(&checkouts[0].path, worktree.path.as_path()) {
                return Ok(BranchObservation::Drift);
            }
            if !same_path(&checkouts[0].path, worktree.path.as_path())
                || same_path(
                    &checkouts[0].path,
                    infrastructure::readonly_repository_identity(&self.git_anchor)?
                        .primary_root
                        .as_path(),
                )
            {
                return Ok(BranchObservation::Drift);
            }
            Ok(
                if self.observe_created_worktree(worktree)? == WorktreeObservation::BeforeExact {
                    BranchObservation::BeforeExact
                } else {
                    BranchObservation::Drift
                },
            )
        }
    }
}

#[cfg(all(unix, test))]
thread_local! { static COMPENSATION_RACE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) }; }
#[cfg(all(unix, test))]
thread_local! { static COMPENSATION_INNER_RACES: RefCell<Vec<Box<dyn FnOnce()>>> = const { RefCell::new(Vec::new()) }; }

#[cfg(all(unix, test))]
pub(crate) struct CompensationRaceGuard;

#[cfg(all(unix, test))]
pub(crate) fn arm_compensation_race(f: impl FnOnce() + 'static) -> CompensationRaceGuard {
    COMPENSATION_RACE.with(|slot| *slot.borrow_mut() = Some(Box::new(f)));
    CompensationRaceGuard
}
#[cfg(all(unix, test))]
pub(crate) fn arm_compensation_inner_race(f: impl FnOnce() + 'static) -> CompensationRaceGuard {
    COMPENSATION_INNER_RACES.with(|slot| slot.borrow_mut().push(Box::new(f)));
    CompensationRaceGuard
}

#[cfg(all(unix, test))]
impl Drop for CompensationRaceGuard {
    fn drop(&mut self) {
        COMPENSATION_RACE.with(|slot| {
            slot.borrow_mut().take();
        });
        COMPENSATION_INNER_RACES.with(|slot| slot.borrow_mut().clear());
    }
}

#[cfg(all(unix, test))]
fn compensation_race_hook() {
    COMPENSATION_RACE.with(|slot| {
        if let Some(f) = slot.borrow_mut().take() {
            f();
        }
    });
}
#[cfg(all(unix, test))]
fn compensation_inner_race_hook() {
    COMPENSATION_INNER_RACES.with(|slot| {
        if !slot.borrow().is_empty() {
            slot.borrow_mut().remove(0)();
        }
    });
}
#[cfg(all(unix, not(test)))]
fn compensation_race_hook() {}
#[cfg(all(unix, not(test)))]
fn compensation_inner_race_hook() {}

fn same_path(a: &Path, b: &Path) -> bool {
    infrastructure::readonly_same_path(a, b)
}

impl CompensationBackend for ProductionCompensationBackend {
    type Error = ProductionCompensationBackendError;

    fn discover_repository(&self, anchor: &Path) -> Result<RepositoryIdentity, Self::Error> {
        self.trusted(anchor)?;
        Ok(infrastructure::readonly_repository_identity(
            &self.git_anchor,
        )?)
    }

    fn check_capability(&self, action: &CompensationActionV1) -> Result<(), CapabilityRefusal> {
        #[cfg(not(unix))]
        {
            let _ = action;
            return Err(CapabilityRefusal::PlatformUnsupported);
        }
        #[cfg(unix)]
        {
            if let Some(actions) = &self.actions
                && !actions.iter().any(|candidate| candidate == action)
            {
                return Err(CapabilityRefusal::Unsupported);
            }
            match action {
                CompensationActionV1::RemoveCreatedArtifactV3(v) => match &v.expected {
                    ArtifactStateV3::Regular(_) | ArtifactStateV3::Symlink(_) => Ok(()),
                },
                CompensationActionV1::RemoveCreatedWorktree(_)
                | CompensationActionV1::DeleteCreatedLocalBranch(_) => Ok(()),
            }
        }
    }

    fn observe_created_artifact(
        &self,
        value: &CreatedArtifactV3,
    ) -> Result<ArtifactObservation, Self::Error> {
        #[cfg(not(unix))]
        {
            let _ = value;
            return Err(ProductionCompensationBackendError::Observation);
        }
        #[cfg(unix)]
        {
            let final_state = observe_artifact(value.path.as_path(), &value.expected)?;
            let staging_absent = value
                .staging
                .as_ref()
                .map_or(Ok(true), |s| absent(s.path.as_path()))?;
            if !staging_absent {
                return Ok(ArtifactObservation::Drift);
            }
            Ok(match final_state {
                ObservedArtifactState::Regular {
                    bytes,
                    digest,
                    mode,
                } => match &value.expected {
                    ArtifactStateV3::Regular(w)
                        if bytes == w.bytes && digest == w.digest && mode == w.mode =>
                    {
                        ArtifactObservation::BeforeExact
                    }
                    _ => ArtifactObservation::Drift,
                },
                ObservedArtifactState::Symlink {
                    target,
                    target_digest,
                } => match &value.expected {
                    ArtifactStateV3::Symlink(w)
                        if target == w.target && target_digest == w.target_digest =>
                    {
                        ArtifactObservation::BeforeExact
                    }
                    _ => ArtifactObservation::Drift,
                },
                ObservedArtifactState::Absent if staging_absent => ArtifactObservation::AfterExact,
                _ => ArtifactObservation::Drift,
            })
        }
    }

    fn invoke_remove_created_artifact(&self, value: &CreatedArtifactV3) -> Result<(), Self::Error> {
        #[cfg(not(unix))]
        {
            let _ = value;
            return Err(ProductionCompensationBackendError::Effect);
        }
        #[cfg(unix)]
        {
            if self.observe_created_artifact(value)? != ArtifactObservation::BeforeExact {
                return Err(ProductionCompensationBackendError::Effect);
            }
            let lease = ArtifactRemovalLease::prepare(value)?;
            compensation_race_hook();
            lease.remove(value)
        }
    }

    fn observe_created_worktree(
        &self,
        value: &CreatedWorktree,
    ) -> Result<WorktreeObservation, Self::Error> {
        #[cfg(not(unix))]
        {
            let _ = value;
            return Err(ProductionCompensationBackendError::Observation);
        }
        #[cfg(unix)]
        {
            let identity = infrastructure::readonly_repository_identity(&self.git_anchor)?;
            if same_path(identity.primary_root.as_path(), value.path.as_path()) {
                return Ok(WorktreeObservation::Drift);
            }
            let listed = infrastructure::readonly_list(&self.git_anchor)?;
            let matches: Vec<_> = listed
                .data
                .worktrees
                .iter()
                .filter(|w| same_path(&w.path, value.path.as_path()))
                .collect();
            if matches.len() > 1 {
                return Err(ProductionCompensationBackendError::Observation);
            }
            let filesystem = infrastructure::readonly_observe_absolute_node(value.path.as_path())?;
            let Some(w) = matches.first() else {
                if listed.data.worktrees.iter().any(|w| {
                    w.branch.as_deref() == Some(value.branch.as_str())
                        && w.head_oid.as_deref() == Some(value.expected_oid.as_str())
                }) {
                    return Ok(WorktreeObservation::Drift);
                }
                return Ok(if filesystem.is_none() {
                    WorktreeObservation::AfterExact
                } else {
                    WorktreeObservation::Drift
                });
            };
            let exact = w.classification == crate::domain::WorktreeClass::Linked
                && w.branch.as_deref() == Some(value.branch.as_str())
                && w.head_oid.as_deref() == Some(value.expected_oid.as_str())
                && w.locked.is_none()
                && w.prunable.is_none()
                && w.status == crate::domain::CheckoutStatus::Clean
                && !w.detached
                && !infrastructure::readonly_ongoing(value.path.as_path())?
                && matches!(filesystem, Some(infrastructure::ObservedNode::Directory));
            Ok(if exact {
                WorktreeObservation::BeforeExact
            } else {
                WorktreeObservation::Drift
            })
        }
    }

    fn invoke_remove_created_worktree(&self, value: &CreatedWorktree) -> Result<(), Self::Error> {
        #[cfg(not(unix))]
        {
            let _ = value;
            return Err(ProductionCompensationBackendError::Effect);
        }
        #[cfg(unix)]
        {
            if self.observe_created_worktree(value)? != WorktreeObservation::BeforeExact {
                return Err(ProductionCompensationBackendError::Effect);
            }
            compensation_race_hook();
            if self.observe_created_worktree(value)? != WorktreeObservation::BeforeExact {
                return Err(ProductionCompensationBackendError::Effect);
            }
            let output = Command::new("git")
                .current_dir(&self.git_anchor)
                .args(["worktree", "remove", "--"])
                .arg(value.path.as_path())
                .output()?;
            if output.status.success() {
                Ok(())
            } else {
                Err(ProductionCompensationBackendError::Effect)
            }
        }
    }

    fn observe_created_local_branch(
        &self,
        value: &CreatedLocalBranch,
    ) -> Result<BranchObservation, Self::Error> {
        match &self.branch_policy {
            Some(policy) => self.observe_branch_contextual(value, policy),
            None => self.observe_branch_strict(value),
        }
    }

    fn invoke_delete_created_local_branch(
        &self,
        value: &CreatedLocalBranch,
    ) -> Result<(), Self::Error> {
        #[cfg(not(unix))]
        {
            let _ = value;
            return Err(ProductionCompensationBackendError::Effect);
        }
        #[cfg(unix)]
        {
            if self.observe_branch_strict(value)? != BranchObservation::BeforeExact {
                return Err(ProductionCompensationBackendError::Effect);
            }
            compensation_race_hook();
            if self.observe_branch_strict(value)? != BranchObservation::BeforeExact {
                return Err(ProductionCompensationBackendError::Effect);
            }
            let name = format!("refs/heads/{}", value.branch);
            let output = Command::new("git")
                .current_dir(&self.git_anchor)
                .args([
                    "update-ref",
                    "--no-deref",
                    "-d",
                    &name,
                    value.expected_oid.as_str(),
                ])
                .output()?;
            if output.status.success() {
                Ok(())
            } else {
                Err(ProductionCompensationBackendError::Effect)
            }
        }
    }
}

#[cfg(unix)]
struct ArtifactRemovalLease {
    parent: rustix::fd::OwnedFd,
    parent_identity: infrastructure::FileIdentity,
    final_name: std::ffi::OsString,
    final_identity: infrastructure::FileIdentity,
    final_kind: FinalKind,
    final_nlink: u64,
    regular: Option<rustix::fd::OwnedFd>,
    symlink_target: Option<Vec<u8>>,
    staging: Option<StagingLease>,
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum FinalKind {
    Regular,
    Symlink,
}

#[cfg(unix)]
fn absent(path: &Path) -> Result<bool, ProductionCompensationBackendError> {
    Ok(infrastructure::readonly_final_absent(path)?)
}

#[cfg(unix)]
struct StagingLease {
    parent: rustix::fd::OwnedFd,
    identity: infrastructure::FileIdentity,
    name: std::ffi::OsString,
}

#[cfg(unix)]
fn map_effect(error: impl std::fmt::Display) -> ProductionCompensationBackendError {
    let _ = error;
    ProductionCompensationBackendError::Effect
}

#[cfg(unix)]
impl ArtifactRemovalLease {
    fn prepare(value: &CreatedArtifactV3) -> Result<Self, ProductionCompensationBackendError> {
        use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, openat, readlinkat, statat};
        let (parent, name) =
            infrastructure::compensation_parent(value.path.as_path()).map_err(map_effect)?;
        let parent_identity = infrastructure::file_identity(&fstat(&parent).map_err(map_effect)?);
        let stat = statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW).map_err(map_effect)?;
        let final_identity = infrastructure::file_identity(&stat);
        let final_nlink = stat.st_nlink as u64;
        if final_nlink != 1 {
            return Err(ProductionCompensationBackendError::Effect);
        }
        let kind = FileType::from_raw_mode(stat.st_mode);
        let (regular, symlink_target, final_kind) = match &value.expected {
            ArtifactStateV3::Regular(want) => {
                if !kind.is_file() || stat.st_size < 0 || stat.st_size as u64 != want.bytes {
                    return Err(ProductionCompensationBackendError::Effect);
                }
                compensation_inner_race_hook();
                let fd = openat(
                    &parent,
                    &name,
                    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
                .map_err(map_effect)?;
                if infrastructure::file_identity(&fstat(&fd).map_err(map_effect)?) != final_identity
                {
                    return Err(ProductionCompensationBackendError::Effect);
                }
                verify_regular(&fd, want)?;
                (Some(fd), None, FinalKind::Regular)
            }
            ArtifactStateV3::Symlink(want) => {
                if !kind.is_symlink() {
                    return Err(ProductionCompensationBackendError::Effect);
                }
                let target = readlinkat(&parent, &name, Vec::new())
                    .map_err(map_effect)?
                    .into_bytes();
                compensation_inner_race_hook();
                let restat =
                    statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW).map_err(map_effect)?;
                if infrastructure::file_identity(&restat) != final_identity
                    || restat.st_nlink as u64 != 1
                {
                    return Err(ProductionCompensationBackendError::Effect);
                }
                if target != want.target.as_path().as_os_str().as_encoded_bytes()
                    || crate::planner::artifact_digest(&target) != want.target_digest
                {
                    return Err(ProductionCompensationBackendError::Effect);
                }
                (None, Some(target), FinalKind::Symlink)
            }
        };
        let staging = if let Some(staging) = &value.staging {
            let (staging_parent, staging_name) =
                infrastructure::compensation_parent(staging.path.as_path()).map_err(map_effect)?;
            let identity =
                infrastructure::file_identity(&fstat(&staging_parent).map_err(map_effect)?);
            match statat(&staging_parent, &staging_name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) => return Err(ProductionCompensationBackendError::Effect),
                Err(error) if error != rustix::io::Errno::NOENT => {
                    return Err(ProductionCompensationBackendError::Effect);
                }
                Err(_) => {}
            }
            Some(StagingLease {
                parent: staging_parent,
                identity,
                name: staging_name,
            })
        } else {
            None
        };
        Ok(Self {
            parent,
            parent_identity,
            final_name: name,
            final_identity,
            final_kind,
            final_nlink,
            regular,
            symlink_target,
            staging,
        })
    }

    fn remove(self, value: &CreatedArtifactV3) -> Result<(), ProductionCompensationBackendError> {
        use rustix::fs::{AtFlags, FileType, fstat, readlinkat, statat, unlinkat};
        let (resolved_parent, resolved_name) =
            infrastructure::compensation_parent(value.path.as_path()).map_err(map_effect)?;
        let resolved_identity =
            infrastructure::file_identity(&fstat(&resolved_parent).map_err(map_effect)?);
        if resolved_identity != self.parent_identity || resolved_name != self.final_name {
            return Err(ProductionCompensationBackendError::Effect);
        }
        if let Some(staging) = &self.staging {
            let (resolved, name) = infrastructure::compensation_parent(
                value
                    .staging
                    .as_ref()
                    .ok_or(ProductionCompensationBackendError::Effect)?
                    .path
                    .as_path(),
            )
            .map_err(map_effect)?;
            if infrastructure::file_identity(&fstat(&resolved).map_err(map_effect)?)
                != staging.identity
                || name != staging.name
            {
                return Err(ProductionCompensationBackendError::Effect);
            }
            match statat(&staging.parent, &staging.name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) => return Err(ProductionCompensationBackendError::Effect),
                Err(error) if error != rustix::io::Errno::NOENT => {
                    return Err(ProductionCompensationBackendError::Effect);
                }
                Err(_) => {}
            }
        }
        let stat = statat(&self.parent, &self.final_name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(map_effect)?;
        let identity = infrastructure::file_identity(&stat);
        if identity != self.final_identity
            || stat.st_nlink as u64 != self.final_nlink
            || stat.st_nlink as u64 != 1
        {
            return Err(ProductionCompensationBackendError::Effect);
        }
        match &value.expected {
            ArtifactStateV3::Regular(want) => {
                let held = self
                    .regular
                    .as_ref()
                    .ok_or(ProductionCompensationBackendError::Effect)?;
                if infrastructure::file_identity(&fstat(held).map_err(map_effect)?)
                    != self.final_identity
                {
                    return Err(ProductionCompensationBackendError::Effect);
                }
                verify_regular(held, want)?;
            }
            ArtifactStateV3::Symlink(want) => {
                let target = readlinkat(&self.parent, &self.final_name, Vec::new())
                    .map_err(map_effect)?
                    .into_bytes();
                let restat = statat(&self.parent, &self.final_name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(map_effect)?;
                if infrastructure::file_identity(&restat) != self.final_identity
                    || !FileType::from_raw_mode(restat.st_mode).is_symlink()
                    || restat.st_nlink as u64 != 1
                    || target != want.target.as_path().as_os_str().as_encoded_bytes()
                    || crate::planner::artifact_digest(&target) != want.target_digest
                    || Some(target) != self.symlink_target
                {
                    return Err(ProductionCompensationBackendError::Effect);
                }
            }
        }
        let kind = FileType::from_raw_mode(stat.st_mode);
        let actual_kind = if kind.is_file() {
            FinalKind::Regular
        } else if kind.is_symlink() {
            FinalKind::Symlink
        } else {
            return Err(ProductionCompensationBackendError::Effect);
        };
        if actual_kind != self.final_kind {
            return Err(ProductionCompensationBackendError::Effect);
        }
        unlinkat(&self.parent, &self.final_name, AtFlags::empty()).map_err(map_effect)
    }
}

#[cfg(unix)]
fn verify_regular(
    fd: &rustix::fd::OwnedFd,
    want: &crate::lifecycle::RegularFileStateV3,
) -> Result<(), ProductionCompensationBackendError> {
    use rustix::fs::fstat;
    use std::{
        fs::File,
        io::{Read, Seek, SeekFrom},
    };
    let stat = fstat(fd).map_err(map_effect)?;
    if infrastructure::permission_mode(&stat) != want.mode
        || stat.st_nlink as u64 != 1
        || stat.st_size < 0
        || stat.st_size as u64 != want.bytes
    {
        return Err(ProductionCompensationBackendError::Effect);
    }
    let mut file = File::from(fd.try_clone().map_err(map_effect)?);
    file.seek(SeekFrom::Start(0)).map_err(map_effect)?;
    let mut hash = sha2::Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 8192];
    loop {
        let count = file.read(&mut buffer).map_err(map_effect)?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > want.bytes {
            return Err(ProductionCompensationBackendError::Effect);
        }
        hash.update(&buffer[..count]);
    }
    let digest =
        crate::lifecycle::ObjectId::new(format!("{:x}", hash.finalize())).map_err(map_effect)?;
    if total != want.bytes || digest != want.digest {
        return Err(ProductionCompensationBackendError::Effect);
    }
    Ok(())
}

#[cfg(unix)]
fn observe_artifact(
    path: &Path,
    expected: &ArtifactStateV3,
) -> Result<ObservedArtifactState, ProductionCompensationBackendError> {
    let lease = match ArtifactRemovalLease::prepare(&CreatedArtifactV3 {
        path: crate::domain::StoredPath::from(path.to_owned()),
        expected: expected.clone(),
        staging: None,
    }) {
        Ok(lease) => lease,
        Err(ProductionCompensationBackendError::Effect)
            if infrastructure::readonly_final_absent(path)? =>
        {
            return Ok(ObservedArtifactState::Absent);
        }
        Err(ProductionCompensationBackendError::Effect) => return Ok(ObservedArtifactState::Other),
        Err(error) => return Err(error),
    };
    if let Some(fd) = lease.regular {
        let want = match expected {
            ArtifactStateV3::Regular(want) => want,
            _ => unreachable!(),
        };
        verify_regular(&fd, want)?;
        return Ok(ObservedArtifactState::Regular {
            bytes: want.bytes,
            digest: want.digest.clone(),
            mode: want.mode,
        });
    }
    if let Some(target) = lease.symlink_target {
        let target = PathBuf::from(std::ffi::OsString::from_vec(target));
        let stored = crate::domain::StoredPath::from(target);
        return Ok(ObservedArtifactState::Symlink {
            target: stored.clone(),
            target_digest: crate::planner::artifact_digest(
                stored.as_path().as_os_str().as_encoded_bytes(),
            ),
        });
    }
    Ok(ObservedArtifactState::Absent)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        compensation::{
            CompensationActionV1, CompensationAllowanceV1, CompensationProposalSourceV1,
            CompensationProposalStepV1, CompensationProposalV1, forward_journal_digest,
            forward_plan_digest,
        },
        compensation_authority::load_bytes,
        compensation_backend::CompensationBackend,
        compensation_execution::{
            CompensationCancellation, CompensationExecutionEngine, CompensationExecutionMode,
            CompensationExecutionOutcome, CompensationExecutionRequest,
        },
        compensation_journal::CompensationStatus,
        compensation_store::LockedCompensationStore,
        domain::StoredPath,
        journal::Journal,
        journal_store::LockedJournalStore,
        lifecycle::{
            ArtifactStateV3, BranchName, CreatedArtifactV3, CreatedLocalBranch, CreatedWorktree,
            ObjectId, OperationPlan, OwnedStagingV3, RegularFileStateV3, SymlinkStateV3,
        },
    };
    use sha2::{Digest, Sha256};
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::PathBuf,
        process::Command,
        str::FromStr,
    };
    use tempfile::TempDir;

    fn regular(path: &Path, bytes: &[u8], mode: u32) -> CreatedArtifactV3 {
        if path.is_file() {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }
        let digest = crate::planner::artifact_digest(bytes);
        CreatedArtifactV3 {
            path: StoredPath::from(path.to_owned()),
            expected: ArtifactStateV3::Regular(RegularFileStateV3 {
                bytes: bytes.len() as u64,
                digest,
                mode,
            }),
            staging: None,
        }
    }
    fn symlink_artifact(path: &Path, target: &Path) -> CreatedArtifactV3 {
        CreatedArtifactV3 {
            path: StoredPath::from(path.to_owned()),
            expected: ArtifactStateV3::Symlink(SymlinkStateV3 {
                target: StoredPath::from(target.to_owned()),
                target_digest: crate::planner::artifact_digest(
                    target.as_os_str().as_encoded_bytes(),
                ),
            }),
            staging: None,
        }
    }
    fn staged(artifact: &CreatedArtifactV3, staging: &Path) -> CreatedArtifactV3 {
        let mut value = artifact.clone();
        value.staging = Some(OwnedStagingV3 {
            path: StoredPath::from(staging.to_owned()),
            ownership_token: ObjectId::new("1111111111111111111111111111111111111111").unwrap(),
        });
        value
    }
    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    struct GitFixture {
        dir: TempDir,
        primary: PathBuf,
        linked: PathBuf,
        oid: ObjectId,
    }
    fn git_fixture() -> GitFixture {
        let dir = TempDir::new().unwrap();
        let primary = dir.path().join("primary");
        fs::create_dir_all(&primary).unwrap();
        git(
            dir.path(),
            &["init", "-b", "main", primary.to_str().unwrap()],
        );
        git(&primary, &["config", "user.email", "test@example.invalid"]);
        git(&primary, &["config", "user.name", "Test"]);
        fs::write(primary.join("tracked"), b"clean").unwrap();
        git(&primary, &["add", "tracked"]);
        git(&primary, &["commit", "-m", "initial"]);
        let oid = ObjectId::new(git(&primary, &["rev-parse", "HEAD"])).unwrap();
        let linked = primary.join("w");
        git(
            &primary,
            &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
        );
        GitFixture {
            dir,
            primary,
            linked,
            oid,
        }
    }
    fn real_authority_fixture() -> (
        GitFixture,
        crate::compensation_authority::LoadedProposal,
        Vec<u8>,
    ) {
        let fixture = git_fixture();
        let mut value = serde_json::to_value(crate::lifecycle::v3_test_plan(1)).unwrap();
        fn rewrite(value: &mut serde_json::Value, primary: &str, linked: &str, oid: &str) {
            match value {
                serde_json::Value::String(s) if s == "0000000000000000000000000000000000000000" => {
                    *s = oid.to_owned()
                }
                serde_json::Value::String(s) if s.starts_with("/r/w") => {
                    *s = format!("{linked}{}", &s[4..])
                }
                serde_json::Value::String(s) if s.starts_with("/r") => {
                    *s = format!("{primary}{}", &s[2..])
                }
                serde_json::Value::Array(items) => items
                    .iter_mut()
                    .for_each(|item| rewrite(item, primary, linked, oid)),
                serde_json::Value::Object(items) => items
                    .values_mut()
                    .for_each(|item| rewrite(item, primary, linked, oid)),
                _ => {}
            }
        }
        rewrite(
            &mut value,
            fixture.primary.to_str().unwrap(),
            fixture.linked.to_str().unwrap(),
            fixture.oid.as_str(),
        );
        let plan: OperationPlan = serde_json::from_value(value).unwrap();
        plan.validate_executable_plan().unwrap();
        let mut forward = Journal::new(plan.clone());
        let mut store =
            LockedJournalStore::acquire(plan.repository().common_dir.as_path()).unwrap();
        store.write_new(&forward).unwrap();
        for step in plan.steps() {
            let started = {
                let mut next = forward.clone();
                next.start_step(step.id()).unwrap();
                next
            };
            store.update(&forward, &started).unwrap();
            let applied = {
                let mut next = started.clone();
                next.apply_step(step.id()).unwrap();
                next
            };
            store.update(&started, &applied).unwrap();
            forward = applied;
        }
        let raw = fs::read(
            plan.repository()
                .common_dir
                .as_path()
                .join("ewtm/journal")
                .join(format!("{}.json", forward.operation_id())),
        )
        .unwrap();
        let worktree = match plan.steps()[0].compensation().as_ref().unwrap() {
            crate::lifecycle::Compensation::RemoveCreatedWorktree(value) => value.clone(),
            _ => panic!(),
        };
        let steps = vec![
            CompensationProposalStepV1 {
                forward_step_id: plan.steps()[0].id().clone(),
                action: CompensationActionV1::RemoveCreatedWorktree(worktree.clone()),
            },
            CompensationProposalStepV1 {
                forward_step_id: plan.steps()[0].id().clone(),
                action: CompensationActionV1::DeleteCreatedLocalBranch(CreatedLocalBranch {
                    branch: worktree.branch.clone(),
                    expected_oid: worktree.expected_oid.clone(),
                }),
            },
        ];
        let proposal = CompensationProposalV1 {
            proposal_schema_version: 1,
            proposal_id: crate::compensation::ProposalId::from_str(
                "00000000-0000-4000-8000-000000000001",
            )
            .unwrap(),
            executable: false,
            repository: plan.repository().clone(),
            source: CompensationProposalSourceV1 {
                operation_id: *forward.operation_id(),
                plan_schema_version: plan.plan_schema_version(),
                journal_schema_version: forward.schema_version(),
                journal_revision: forward.revision(),
                forward_plan_digest: forward_plan_digest(&plan).unwrap(),
                forward_journal_digest: forward_journal_digest(&raw),
            },
            allowed_categories: vec![
                CompensationAllowanceV1::Worktree,
                CompensationAllowanceV1::LocalBranch,
            ],
            steps,
        };
        proposal.validate().unwrap();
        let proposal_raw = serde_json::to_vec(&proposal).unwrap();
        let confirmation = format!("{:x}", Sha256::digest(&proposal_raw));
        let authority = load_bytes(proposal_raw, &confirmation).unwrap();
        (fixture, authority, raw)
    }

    #[test]
    fn regular_artifact_exact_invoke_after_uses_descriptor_lease() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("value");
        fs::write(&path, b"payload").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let artifact = regular(&path, b"payload", 0o640);
        let backend = ProductionCompensationBackend::new(dir.path().to_owned());
        assert_eq!(
            backend.observe_created_artifact(&artifact).unwrap(),
            ArtifactObservation::BeforeExact
        );
        backend.invoke_remove_created_artifact(&artifact).unwrap();
        assert_eq!(
            backend.observe_created_artifact(&artifact).unwrap(),
            ArtifactObservation::AfterExact
        );
    }

    #[test]
    fn regular_artifact_foreign_replacement_race_survives() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("value");
        fs::write(&path, b"payload").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let artifact = regular(&path, b"payload", 0o640);
        let foreign = dir.path().join("foreign");
        fs::write(&foreign, b"foreign").unwrap();
        let replacement = path.clone();
        let guard = arm_compensation_race(move || {
            fs::rename(&foreign, &replacement).unwrap();
        });
        let backend = ProductionCompensationBackend::new(dir.path().to_owned());
        assert!(backend.invoke_remove_created_artifact(&artifact).is_err());
        drop(guard);
        assert_eq!(fs::read(&path).unwrap(), b"foreign");
    }

    #[test]
    fn symlink_artifact_uses_raw_target_and_never_follows_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("link");
        let target = PathBuf::from("../raw-target");
        symlink(&target, &path).unwrap();
        let digest = crate::planner::artifact_digest(target.as_os_str().as_encoded_bytes());
        let artifact = CreatedArtifactV3 {
            path: StoredPath::from(path),
            expected: ArtifactStateV3::Symlink(SymlinkStateV3 {
                target: StoredPath::from(target),
                target_digest: digest,
            }),
            staging: None,
        };
        let backend = ProductionCompensationBackend::new(dir.path().to_owned());
        assert_eq!(
            backend.observe_created_artifact(&artifact).unwrap(),
            ArtifactObservation::BeforeExact
        );
        backend.invoke_remove_created_artifact(&artifact).unwrap();
        assert_eq!(
            backend.observe_created_artifact(&artifact).unwrap(),
            ArtifactObservation::AfterExact
        );
    }
    #[test]
    fn artifact_bounded_short_and_extra_content_are_drift_and_unchanged() {
        for actual in [b"short".as_slice(), b"payload-extra".as_slice()] {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("value");
            fs::write(&path, actual).unwrap();
            let artifact = regular(&path, b"payload", 0o640);
            let before = fs::read(&path).unwrap();
            let backend = ProductionCompensationBackend::new(dir.path().to_owned());
            assert_eq!(
                backend.observe_created_artifact(&artifact).unwrap(),
                ArtifactObservation::Drift
            );
            assert!(backend.invoke_remove_created_artifact(&artifact).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }
    #[test]
    fn artifact_same_shape_regular_and_symlink_replacements_survive_lease() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("value");
        let foreign = dir.path().join("foreign");
        fs::write(&path, b"payload").unwrap();
        fs::write(&foreign, b"payload").unwrap();
        let artifact = regular(&path, b"payload", 0o640);
        let guard = arm_compensation_race({
            let path = path.clone();
            let foreign = foreign.clone();
            move || {
                fs::rename(foreign, path).unwrap();
            }
        });
        assert!(
            ProductionCompensationBackend::new(dir.path().to_owned())
                .invoke_remove_created_artifact(&artifact)
                .is_err()
        );
        drop(guard);
        assert_eq!(fs::read(&path).unwrap(), b"payload");

        let link = dir.path().join("link");
        let other = dir.path().join("other");
        symlink("target", &link).unwrap();
        symlink("target", &other).unwrap();
        let artifact = symlink_artifact(&link, Path::new("target"));
        let guard = arm_compensation_race({
            let link = link.clone();
            let other = other.clone();
            move || {
                fs::rename(other, link).unwrap();
            }
        });
        assert!(
            ProductionCompensationBackend::new(dir.path().to_owned())
                .invoke_remove_created_artifact(&artifact)
                .is_err()
        );
        drop(guard);
        assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("target"));
    }
    #[test]
    fn artifact_inner_regular_and_symlink_replacements_survive_before_open_or_restat() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("value");
        let foreign = dir.path().join("foreign");
        fs::write(&path, b"payload").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        fs::write(&foreign, b"payload").unwrap();
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o640)).unwrap();
        let artifact = regular(&path, b"payload", 0o640);
        let guard = arm_compensation_inner_race({
            let path = path.clone();
            let foreign = foreign.clone();
            move || {
                fs::rename(foreign, path).unwrap();
            }
        });
        assert!(
            ProductionCompensationBackend::new(dir.path().to_owned())
                .invoke_remove_created_artifact(&artifact)
                .is_err()
        );
        drop(guard);
        assert_eq!(fs::read(&path).unwrap(), b"payload");
        let link = dir.path().join("link");
        let other = dir.path().join("other");
        symlink("target", &link).unwrap();
        symlink("target", &other).unwrap();
        let artifact = symlink_artifact(&link, Path::new("target"));
        let guard = arm_compensation_inner_race({
            let link = link.clone();
            let other = other.clone();
            move || {
                fs::rename(other, link).unwrap();
            }
        });
        assert!(
            ProductionCompensationBackend::new(dir.path().to_owned())
                .invoke_remove_created_artifact(&artifact)
                .is_err()
        );
        drop(guard);
        assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("target"));
    }
    #[test]
    fn artifact_same_size_wrong_content_is_digest_drift_and_never_removed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("value");
        fs::write(&path, b"payload").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let artifact = regular(&path, b"payloAd", 0o640);
        let backend = ProductionCompensationBackend::new(dir.path().to_owned());
        assert_eq!(
            backend.observe_created_artifact(&artifact).unwrap(),
            ArtifactObservation::Drift
        );
        assert!(backend.invoke_remove_created_artifact(&artifact).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"payload");
    }
    #[test]
    fn artifact_hardlinks_initial_and_race_are_refused_without_deletion() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("value");
        let alias = dir.path().join("alias");
        fs::write(&path, b"payload").unwrap();
        fs::hard_link(&path, &alias).unwrap();
        let artifact = regular(&path, b"payload", 0o640);
        let backend = ProductionCompensationBackend::new(dir.path().to_owned());
        assert_eq!(
            backend.observe_created_artifact(&artifact).unwrap(),
            ArtifactObservation::Drift
        );
        assert!(backend.invoke_remove_created_artifact(&artifact).is_err());
        assert!(path.exists() && alias.exists());

        fs::remove_file(&alias).unwrap();
        let guard = arm_compensation_race({
            let path = path.clone();
            let alias = alias.clone();
            move || {
                fs::hard_link(&path, &alias).unwrap();
            }
        });
        assert!(backend.invoke_remove_created_artifact(&artifact).is_err());
        drop(guard);
        assert!(path.exists() && alias.exists());
    }
    #[test]
    fn artifact_symlink_hardlink_is_drift_and_target_survives() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("link");
        let alias = dir.path().join("alias");
        let target = PathBuf::from("target");
        symlink(&target, &path).unwrap();
        fs::hard_link(&path, &alias).unwrap();
        let artifact = symlink_artifact(&path, &target);
        let backend = ProductionCompensationBackend::new(dir.path().to_owned());
        assert_eq!(
            backend.observe_created_artifact(&artifact).unwrap(),
            ArtifactObservation::Drift
        );
        assert!(backend.invoke_remove_created_artifact(&artifact).is_err());
        assert!(fs::symlink_metadata(&path).is_ok() && fs::symlink_metadata(&alias).is_ok());
    }
    #[test]
    fn artifact_parent_swap_and_ancestor_swap_refuse_and_preserve_trees() {
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("parent");
        fs::create_dir(&parent).unwrap();
        let path = parent.join("value");
        fs::write(&path, b"payload").unwrap();
        let replacement = dir.path().join("replacement");
        fs::create_dir(&replacement).unwrap();
        let root = dir.path().to_owned();
        let artifact = regular(&path, b"payload", 0o640);
        let guard = arm_compensation_race({
            let parent = parent.clone();
            let replacement = replacement.clone();
            move || {
                fs::rename(&parent, root.join("old")).unwrap();
                fs::rename(replacement, parent).unwrap();
            }
        });
        assert!(
            ProductionCompensationBackend::new(dir.path().to_owned())
                .invoke_remove_created_artifact(&artifact)
                .is_err()
        );
        drop(guard);
        assert!(dir.path().join("old/value").exists() && parent.exists());
    }
    #[test]
    fn artifact_staging_absent_allows_remove_but_present_and_appearance_refuse() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("value");
        let staging = dir.path().join("staging");
        fs::write(&path, b"payload").unwrap();
        let artifact = staged(&regular(&path, b"payload", 0o640), &staging);
        let backend = ProductionCompensationBackend::new(dir.path().to_owned());
        assert_eq!(
            backend.observe_created_artifact(&artifact).unwrap(),
            ArtifactObservation::BeforeExact
        );
        backend.invoke_remove_created_artifact(&artifact).unwrap();
        assert!(!path.exists() && !staging.exists());
        fs::write(&path, b"payload").unwrap();
        fs::write(&staging, b"owned").unwrap();
        assert_eq!(
            backend.observe_created_artifact(&artifact).unwrap(),
            ArtifactObservation::Drift
        );
        assert!(backend.invoke_remove_created_artifact(&artifact).is_err());
        assert!(path.exists() && staging.exists());
        fs::remove_file(&staging).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let guard = arm_compensation_race({
            let staging = staging.clone();
            move || {
                fs::write(staging, b"appeared").unwrap();
            }
        });
        assert!(backend.invoke_remove_created_artifact(&artifact).is_err());
        drop(guard);
        assert!(path.exists(), "final artifact was removed");
        assert!(staging.exists(), "staging did not appear");
    }
    #[test]
    fn artifact_symlink_ancestor_directory_and_special_final_are_never_removed() {
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = dir.path().join("alias");
        symlink(&real, &alias).unwrap();
        let path = alias.join("value");
        fs::write(&path, b"payload").unwrap();
        let artifact = regular(&path, b"payload", 0o640);
        let backend = ProductionCompensationBackend::new(dir.path().to_owned());
        assert!(backend.invoke_remove_created_artifact(&artifact).is_err());
        assert!(path.exists());
        let directory = real.join("directory");
        fs::create_dir(&directory).unwrap();
        let directory_artifact = regular(&directory, b"payload", 0o640);
        assert_eq!(
            backend
                .observe_created_artifact(&directory_artifact)
                .unwrap(),
            ArtifactObservation::Drift
        );
        assert!(directory.exists());
    }
    #[test]
    fn fifo_and_special_final_nodes_are_drift_without_blocking_reads() {
        let dir = TempDir::new().unwrap();
        let fifo = dir.path().join("fifo");
        let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success());
        let regular_artifact = regular(&fifo, b"payload", 0o640);
        let backend = ProductionCompensationBackend::new(dir.path().to_owned());
        assert_eq!(
            backend.observe_created_artifact(&regular_artifact).unwrap(),
            ArtifactObservation::Drift
        );
        assert!(
            backend
                .invoke_remove_created_artifact(&regular_artifact)
                .is_err()
        );
        assert!(fifo.exists());
        let symlink_artifact = symlink_artifact(&fifo, Path::new("target"));
        assert_eq!(
            backend.observe_created_artifact(&symlink_artifact).unwrap(),
            ArtifactObservation::Drift
        );
        assert!(
            backend
                .invoke_remove_created_artifact(&symlink_artifact)
                .is_err()
        );
        assert!(fifo.exists());
    }
    #[test]
    fn real_clean_linked_worktree_is_removed_without_force_and_registration() {
        let fixture = git_fixture();
        let backend = ProductionCompensationBackend::new(fixture.primary.clone());
        let value = CreatedWorktree {
            path: StoredPath::from(fixture.linked.clone()),
            branch: BranchName::new("feature").unwrap(),
            expected_oid: fixture.oid.clone(),
            branch_was_created: true,
        };
        assert_eq!(
            backend.observe_created_worktree(&value).unwrap(),
            WorktreeObservation::BeforeExact
        );
        backend.invoke_remove_created_worktree(&value).unwrap();
        assert_eq!(
            backend.observe_created_worktree(&value).unwrap(),
            WorktreeObservation::AfterExact
        );
        assert!(!fixture.linked.exists());
        assert!(!git(&fixture.primary, &["worktree", "list", "--porcelain"]).contains("feature"));
    }
    #[test]
    fn worktree_primary_dirty_and_untracked_refusals_preserve_filesystem_and_registration() {
        let fixture = git_fixture();
        let backend = ProductionCompensationBackend::new(fixture.primary.clone());
        let primary = CreatedWorktree {
            path: StoredPath::from(fixture.primary.clone()),
            branch: BranchName::new("main").unwrap(),
            expected_oid: fixture.oid.clone(),
            branch_was_created: false,
        };
        assert_eq!(
            backend.observe_created_worktree(&primary).unwrap(),
            WorktreeObservation::Drift
        );
        assert!(backend.invoke_remove_created_worktree(&primary).is_err());
        assert!(fixture.primary.exists());
        let value = CreatedWorktree {
            path: StoredPath::from(fixture.linked.clone()),
            branch: BranchName::new("feature").unwrap(),
            expected_oid: fixture.oid.clone(),
            branch_was_created: true,
        };
        fs::write(fixture.linked.join("tracked"), b"dirty").unwrap();
        let snapshot = fs::read(fixture.linked.join("tracked")).unwrap();
        assert_eq!(
            backend.observe_created_worktree(&value).unwrap(),
            WorktreeObservation::Drift
        );
        assert!(backend.invoke_remove_created_worktree(&value).is_err());
        assert_eq!(fs::read(fixture.linked.join("tracked")).unwrap(), snapshot);
        assert!(fixture.linked.exists());
        fs::write(fixture.linked.join("tracked"), b"clean").unwrap();
        fs::write(fixture.linked.join("untracked"), b"untracked").unwrap();
        assert_eq!(
            backend.observe_created_worktree(&value).unwrap(),
            WorktreeObservation::Drift
        );
        assert!(backend.invoke_remove_created_worktree(&value).is_err());
        assert!(fixture.linked.join("untracked").exists());
    }
    #[test]
    fn worktree_race_dirty_after_first_check_refuses_without_force() {
        let fixture = git_fixture();
        let backend = ProductionCompensationBackend::new(fixture.primary.clone());
        let value = CreatedWorktree {
            path: StoredPath::from(fixture.linked.clone()),
            branch: BranchName::new("feature").unwrap(),
            expected_oid: fixture.oid.clone(),
            branch_was_created: true,
        };
        let guard = arm_compensation_race({
            let linked = fixture.linked.clone();
            move || {
                fs::write(linked.join("race"), b"dirty").unwrap();
            }
        });
        assert!(backend.invoke_remove_created_worktree(&value).is_err());
        drop(guard);
        assert!(fixture.linked.exists() && fixture.linked.join("race").exists());
    }
    #[test]
    fn worktree_wrong_tuple_and_detached_states_are_drift() {
        let fixture = git_fixture();
        let backend = ProductionCompensationBackend::new(fixture.primary.clone());
        let wrong_path = CreatedWorktree {
            path: StoredPath::from(fixture.dir.path().join("wrong")),
            branch: BranchName::new("feature").unwrap(),
            expected_oid: fixture.oid.clone(),
            branch_was_created: true,
        };
        assert_eq!(
            backend.observe_created_worktree(&wrong_path).unwrap(),
            WorktreeObservation::Drift
        );
        git(&fixture.linked, &["checkout", "--detach"]);
        let detached = CreatedWorktree {
            path: StoredPath::from(fixture.linked.clone()),
            branch: BranchName::new("feature").unwrap(),
            expected_oid: fixture.oid.clone(),
            branch_was_created: true,
        };
        assert_eq!(
            backend.observe_created_worktree(&detached).unwrap(),
            WorktreeObservation::Drift
        );
        assert!(backend.invoke_remove_created_worktree(&detached).is_err());
        assert!(fixture.linked.exists());
    }
    #[test]
    fn locked_worktree_is_drift_and_preserves_registration_and_path() {
        let fixture = git_fixture();
        let backend = ProductionCompensationBackend::new(fixture.primary.clone());
        let value = CreatedWorktree {
            path: StoredPath::from(fixture.linked.clone()),
            branch: BranchName::new("feature").unwrap(),
            expected_oid: fixture.oid.clone(),
            branch_was_created: true,
        };
        git(
            &fixture.primary,
            &["worktree", "lock", fixture.linked.to_str().unwrap()],
        );
        assert_eq!(
            backend.observe_created_worktree(&value).unwrap(),
            WorktreeObservation::Drift
        );
        assert!(backend.invoke_remove_created_worktree(&value).is_err());
        assert!(fixture.linked.exists());
        assert!(
            git(&fixture.primary, &["worktree", "list", "--porcelain"])
                .contains(fixture.linked.to_str().unwrap())
        );
    }
    #[test]
    fn prunable_registered_missing_path_is_drift_not_after() {
        let fixture = git_fixture();
        let backend = ProductionCompensationBackend::new(fixture.primary.clone());
        let value = CreatedWorktree {
            path: StoredPath::from(fixture.linked.clone()),
            branch: BranchName::new("feature").unwrap(),
            expected_oid: fixture.oid.clone(),
            branch_was_created: true,
        };
        fs::remove_dir_all(&fixture.linked).unwrap();
        assert_eq!(
            backend.observe_created_worktree(&value).unwrap(),
            WorktreeObservation::Drift
        );
        assert!(backend.invoke_remove_created_worktree(&value).is_err());
        assert!(!fixture.linked.exists());
        assert!(git(&fixture.primary, &["worktree", "list", "--porcelain"]).contains("prunable"));
    }
    #[test]
    fn ongoing_operation_marker_is_drift_and_preserves_worktree() {
        let fixture = git_fixture();
        let backend = ProductionCompensationBackend::new(fixture.primary.clone());
        let value = CreatedWorktree {
            path: StoredPath::from(fixture.linked.clone()),
            branch: BranchName::new("feature").unwrap(),
            expected_oid: fixture.oid.clone(),
            branch_was_created: true,
        };
        let merge_head = PathBuf::from(git(
            &fixture.linked,
            &["rev-parse", "--git-path", "MERGE_HEAD"],
        ));
        fs::write(&merge_head, format!("{}\n", fixture.oid.as_str())).unwrap();
        assert_eq!(
            backend.observe_created_worktree(&value).unwrap(),
            WorktreeObservation::Drift
        );
        assert!(backend.invoke_remove_created_worktree(&value).is_err());
        assert!(fixture.linked.exists());
        assert!(
            git(&fixture.primary, &["worktree", "list", "--porcelain"])
                .contains(fixture.linked.to_str().unwrap())
        );
    }
    #[test]
    fn exact_branch_lease_removes_direct_ref_and_wrong_or_checked_out_refs_survive() {
        let fixture = git_fixture();
        let backend = ProductionCompensationBackend::new(fixture.primary.clone());
        let value = CreatedLocalBranch {
            branch: BranchName::new("feature").unwrap(),
            expected_oid: fixture.oid.clone(),
        };
        assert_eq!(
            backend.observe_created_local_branch(&value).unwrap(),
            BranchObservation::Drift
        );
        git(
            &fixture.primary,
            &["worktree", "remove", fixture.linked.to_str().unwrap()],
        );
        git(
            &fixture.primary,
            &["branch", "direct", fixture.oid.as_str()],
        );
        let value = CreatedLocalBranch {
            branch: BranchName::new("direct").unwrap(),
            expected_oid: fixture.oid.clone(),
        };
        assert_eq!(
            backend.observe_created_local_branch(&value).unwrap(),
            BranchObservation::BeforeExact
        );
        backend.invoke_delete_created_local_branch(&value).unwrap();
        assert_eq!(
            backend.observe_created_local_branch(&value).unwrap(),
            BranchObservation::AfterExact
        );

        fs::write(fixture.primary.join("second"), b"second").unwrap();
        git(&fixture.primary, &["add", "second"]);
        git(&fixture.primary, &["commit", "-m", "second"]);
        let other = ObjectId::new(git(&fixture.primary, &["rev-parse", "HEAD"])).unwrap();
        git(&fixture.primary, &["branch", "wrong", fixture.oid.as_str()]);
        let wrong = CreatedLocalBranch {
            branch: BranchName::new("wrong").unwrap(),
            expected_oid: other,
        };
        assert_eq!(
            backend.observe_created_local_branch(&wrong).unwrap(),
            BranchObservation::Drift
        );
        assert!(backend.invoke_delete_created_local_branch(&wrong).is_err());
        assert!(
            git(
                &fixture.primary,
                &["show-ref", "--verify", "refs/heads/wrong"]
            )
            .contains("wrong")
        );
    }
    #[test]
    fn branch_symbolic_invalid_and_oid_race_are_refused_without_deletion() {
        let fixture = git_fixture();
        let backend = ProductionCompensationBackend::new(fixture.primary.clone());
        git(&fixture.primary, &["branch", "race"]);
        fs::write(fixture.primary.join("race-commit"), b"race").unwrap();
        git(&fixture.primary, &["add", "race-commit"]);
        git(&fixture.primary, &["commit", "-m", "race"]);
        let other = ObjectId::new(git(&fixture.primary, &["rev-parse", "HEAD"])).unwrap();
        let race = CreatedLocalBranch {
            branch: BranchName::new("race").unwrap(),
            expected_oid: fixture.oid.clone(),
        };
        let guard = arm_compensation_race({
            let primary = fixture.primary.clone();
            let other = other.clone();
            move || {
                git(&primary, &["update-ref", "refs/heads/race", other.as_str()]);
            }
        });
        assert!(backend.invoke_delete_created_local_branch(&race).is_err());
        drop(guard);
        assert_eq!(
            git(&fixture.primary, &["rev-parse", "refs/heads/race"]),
            other.as_str()
        );
        git(
            &fixture.primary,
            &["symbolic-ref", "refs/heads/symbolic", "refs/heads/main"],
        );
        let symbolic = CreatedLocalBranch {
            branch: BranchName::new("symbolic").unwrap(),
            expected_oid: fixture.oid.clone(),
        };
        assert_eq!(
            backend.observe_created_local_branch(&symbolic).unwrap(),
            BranchObservation::Drift
        );
        assert!(
            backend
                .invoke_delete_created_local_branch(&symbolic)
                .is_err()
        );
        let invalid = CreatedLocalBranch {
            branch: BranchName::new("bad..name").unwrap(),
            expected_oid: fixture.oid.clone(),
        };
        assert_eq!(
            backend.observe_created_local_branch(&invalid).unwrap(),
            BranchObservation::Drift
        );
        assert!(
            backend
                .invoke_delete_created_local_branch(&invalid)
                .is_err()
        );
    }
    #[test]
    fn branch_ref_pointing_to_tag_object_is_not_exact_commit_lease() {
        let fixture = git_fixture();
        let backend = ProductionCompensationBackend::new(fixture.primary.clone());
        git(&fixture.primary, &["tag", "annotated", "-m", "annotated"]);
        let tag_object = git(
            &fixture.primary,
            &["rev-parse", "refs/tags/annotated^{tag}"],
        );
        fs::write(
            fixture.primary.join(".git/refs/heads/tag-object"),
            format!("{tag_object}\n"),
        )
        .unwrap();
        let value = CreatedLocalBranch {
            branch: BranchName::new("tag-object").unwrap(),
            expected_oid: fixture.oid.clone(),
        };
        assert_eq!(
            backend.observe_created_local_branch(&value).unwrap(),
            BranchObservation::Drift
        );
        assert!(backend.invoke_delete_created_local_branch(&value).is_err());
        assert_eq!(
            git(&fixture.primary, &["rev-parse", "refs/heads/tag-object"]),
            tag_object
        );
    }
    #[test]
    fn unix_capabilities_cover_artifact_worktree_and_branch_actions() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("value");
        fs::write(&path, b"payload").unwrap();
        let backend = ProductionCompensationBackend::new(dir.path().to_owned());
        let artifact = regular(&path, b"payload", 0o640);
        assert!(
            backend
                .check_capability(&CompensationActionV1::RemoveCreatedArtifactV3(artifact))
                .is_ok()
        );
        assert!(
            backend
                .check_capability(&CompensationActionV1::RemoveCreatedWorktree(
                    CreatedWorktree {
                        path: StoredPath::from(dir.path().join("worktree")),
                        branch: BranchName::new("feature").unwrap(),
                        expected_oid: ObjectId::new("1111111111111111111111111111111111111111")
                            .unwrap(),
                        branch_was_created: true
                    }
                ))
                .is_ok()
        );
        assert!(
            backend
                .check_capability(&CompensationActionV1::DeleteCreatedLocalBranch(
                    CreatedLocalBranch {
                        branch: BranchName::new("feature").unwrap(),
                        expected_oid: ObjectId::new("1111111111111111111111111111111111111111")
                            .unwrap()
                    }
                ))
                .is_ok()
        );
    }
    #[test]
    fn strict_new_refuses_checked_out_created_branch_but_authority_context_allows_predecessor() {
        let (fixture, authority, _) = real_authority_fixture();
        let strict = ProductionCompensationBackend::new(fixture.primary.clone());
        let branch = match &authority.proposal().steps[1].action {
            CompensationActionV1::DeleteCreatedLocalBranch(value) => value.clone(),
            _ => unreachable!(),
        };
        assert_eq!(
            strict.observe_created_local_branch(&branch).unwrap(),
            BranchObservation::Drift
        );
        assert!(strict.invoke_delete_created_local_branch(&branch).is_err());
        let contextual =
            ProductionCompensationBackend::for_authority(fixture.primary.clone(), &authority)
                .unwrap();
        assert_eq!(
            contextual.observe_created_local_branch(&branch).unwrap(),
            BranchObservation::BeforeExact
        );
        assert!(
            contextual
                .invoke_delete_created_local_branch(&branch)
                .is_err()
        );
        let worktree = match &authority.proposal().steps[0].action {
            CompensationActionV1::RemoveCreatedWorktree(value) => value.clone(),
            _ => unreachable!(),
        };
        strict.invoke_remove_created_worktree(&worktree).unwrap();
        assert_eq!(
            contextual.observe_created_local_branch(&branch).unwrap(),
            BranchObservation::BeforeExact
        );
        contextual
            .invoke_delete_created_local_branch(&branch)
            .unwrap();
        assert_eq!(
            contextual.observe_created_local_branch(&branch).unwrap(),
            BranchObservation::AfterExact
        );
    }
    #[test]
    fn real_engine_authority_applies_worktree_then_created_branch_and_preserves_forward_bytes() {
        let (fixture, authority, forward_before) = real_authority_fixture();
        let backend =
            ProductionCompensationBackend::for_authority(fixture.linked.clone(), &authority)
                .unwrap();
        let outcome = CompensationExecutionEngine::new(backend)
            .execute(CompensationExecutionRequest {
                anchor: &fixture.linked,
                mode: CompensationExecutionMode::Fresh {
                    authority: &authority,
                },
                cancellation: CompensationCancellation::default(),
            })
            .unwrap();
        assert!(matches!(outcome, CompensationExecutionOutcome::Applied(_)));
        assert!(!fixture.linked.exists());
        assert!(
            !Command::new("git")
                .current_dir(&fixture.primary)
                .args(["show-ref", "--verify", "refs/heads/feature"])
                .status()
                .unwrap()
                .success()
        );
        let compensation = LockedCompensationStore::acquire(fixture.primary.join(".git").as_path())
            .unwrap()
            .read(&authority.proposal().proposal_id)
            .unwrap();
        assert_eq!(compensation.status(), CompensationStatus::Applied);
        assert!(compensation.steps().iter().all(
            |step| step.status == crate::compensation_journal::CompensationStepStatus::Applied
        ));
        let forward_after = fs::read(
            fixture
                .primary
                .join(".git/ewtm/journal")
                .join(format!("{}.json", authority.proposal().source.operation_id)),
        )
        .unwrap();
        assert_eq!(forward_after, forward_before);
    }
    #[test]
    fn real_engine_dirty_worktree_stops_pre_started_without_invocation_or_forward_mutation() {
        let (fixture, authority, forward_before) = real_authority_fixture();
        fs::write(fixture.linked.join("tracked"), b"dirty").unwrap();
        let backend =
            ProductionCompensationBackend::for_authority(fixture.linked.clone(), &authority)
                .unwrap();
        let outcome = CompensationExecutionEngine::new(backend)
            .execute(CompensationExecutionRequest {
                anchor: &fixture.linked,
                mode: CompensationExecutionMode::Fresh {
                    authority: &authority,
                },
                cancellation: CompensationCancellation::default(),
            })
            .unwrap();
        assert!(matches!(
            outcome,
            CompensationExecutionOutcome::NeedsAttention(_)
        ));
        let compensation = LockedCompensationStore::acquire(fixture.primary.join(".git").as_path())
            .unwrap()
            .read(&authority.proposal().proposal_id)
            .unwrap();
        assert_eq!(compensation.status(), CompensationStatus::NeedsAttention);
        assert_eq!(compensation.revision(), 1);
        assert!(compensation.started_step().is_none());
        assert!(fixture.linked.exists());
        assert!(
            Command::new("git")
                .current_dir(&fixture.primary)
                .args(["show-ref", "--verify", "refs/heads/feature"])
                .status()
                .unwrap()
                .success()
        );
        let forward_after = fs::read(
            fixture
                .primary
                .join(".git/ewtm/journal")
                .join(format!("{}.json", authority.proposal().source.operation_id)),
        )
        .unwrap();
        assert_eq!(forward_after, forward_before);
    }
    #[test]
    fn authority_without_created_branch_has_no_contextual_checkout_exemption() {
        let (fixture, authority, _) = real_authority_fixture();
        let mut proposal = authority.proposal().clone();
        if let CompensationActionV1::RemoveCreatedWorktree(worktree) = &mut proposal.steps[0].action
        {
            worktree.branch_was_created = false;
        } else {
            unreachable!()
        }
        proposal.steps.pop();
        proposal
            .allowed_categories
            .retain(|category| *category != CompensationAllowanceV1::LocalBranch);
        let raw = serde_json::to_vec(&proposal).unwrap();
        let loaded = load_bytes(raw.clone(), &format!("{:x}", Sha256::digest(&raw))).unwrap();
        let backend =
            ProductionCompensationBackend::for_authority(fixture.primary.clone(), &loaded).unwrap();
        let branch = CreatedLocalBranch {
            branch: BranchName::new("feature").unwrap(),
            expected_oid: fixture.oid.clone(),
        };
        assert_eq!(
            backend.observe_created_local_branch(&branch).unwrap(),
            BranchObservation::Drift
        );
        assert_eq!(
            backend.check_capability(&CompensationActionV1::DeleteCreatedLocalBranch(branch)),
            Err(CapabilityRefusal::Unsupported)
        );
    }
    #[test]
    fn contextual_branch_changed_oid_is_drift_even_with_mapped_checkout() {
        let (fixture, authority, _) = real_authority_fixture();
        let backend =
            ProductionCompensationBackend::for_authority(fixture.primary.clone(), &authority)
                .unwrap();
        fs::write(fixture.primary.join("changed"), b"changed").unwrap();
        git(&fixture.primary, &["add", "changed"]);
        git(&fixture.primary, &["commit", "-m", "changed"]);
        let changed = git(&fixture.primary, &["rev-parse", "HEAD"]);
        git(
            &fixture.primary,
            &["update-ref", "refs/heads/feature", &changed],
        );
        let branch = match &authority.proposal().steps[1].action {
            CompensationActionV1::DeleteCreatedLocalBranch(value) => value,
            _ => unreachable!(),
        };
        assert_eq!(
            backend.observe_created_local_branch(branch).unwrap(),
            BranchObservation::Drift
        );
    }
    #[test]
    fn contextual_symbolic_branch_is_drift_even_with_mapped_checkout() {
        let (fixture, authority, _) = real_authority_fixture();
        let backend =
            ProductionCompensationBackend::for_authority(fixture.primary.clone(), &authority)
                .unwrap();
        git(
            &fixture.primary,
            &["symbolic-ref", "refs/heads/feature", "refs/heads/main"],
        );
        let branch = match &authority.proposal().steps[1].action {
            CompensationActionV1::DeleteCreatedLocalBranch(value) => value,
            _ => unreachable!(),
        };
        assert_eq!(
            backend.observe_created_local_branch(branch).unwrap(),
            BranchObservation::Drift
        );
    }
    #[test]
    fn contextual_exact_checkout_with_extra_checkout_is_drift() {
        let (fixture, authority, _) = real_authority_fixture();
        let backend =
            ProductionCompensationBackend::for_authority(fixture.primary.clone(), &authority)
                .unwrap();
        let extra = fixture.dir.path().join("extra");
        git(
            &fixture.primary,
            &[
                "worktree",
                "add",
                "--force",
                extra.to_str().unwrap(),
                "feature",
            ],
        );
        let branch = match &authority.proposal().steps[1].action {
            CompensationActionV1::DeleteCreatedLocalBranch(value) => value,
            _ => unreachable!(),
        };
        assert_eq!(
            backend.observe_created_local_branch(branch).unwrap(),
            BranchObservation::Drift
        );
        assert!(extra.exists());
        assert!(fixture.linked.exists());
    }
}

#[cfg(all(test, not(unix)))]
mod non_unix_tests {
    use super::*;
    use crate::{
        compensation::CompensationActionV1,
        lifecycle::{BranchName, CreatedLocalBranch, ObjectId},
    };
    #[test]
    fn non_unix_capability_and_effects_refuse_all_destructive_actions() {
        let backend = ProductionCompensationBackend::new(PathBuf::from("."));
        let action = CompensationActionV1::DeleteCreatedLocalBranch(CreatedLocalBranch {
            branch: BranchName::new("feature").unwrap(),
            expected_oid: ObjectId::new("1111111111111111111111111111111111111111").unwrap(),
        });
        assert_eq!(
            backend.check_capability(&action),
            Err(CapabilityRefusal::PlatformUnsupported)
        );
        assert!(
            backend
                .invoke_delete_created_local_branch(match &action {
                    CompensationActionV1::DeleteCreatedLocalBranch(value) => value,
                    _ => unreachable!(),
                })
                .is_err()
        );
    }
}
