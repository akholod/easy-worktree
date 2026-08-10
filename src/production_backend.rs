use crate::{
    domain::{CheckoutStatus, WorktreeClass},
    execution::{ConditionResult, ExecutionBackend, ProbeCapability, ProbeContext, ProbeVerdict},
    infrastructure::{self, GitError},
    lifecycle::{ObjectId, OperationPlan, PlanStep, Postcondition, Precondition, StepAction},
};
use std::{
    io,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProductionBackendError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("mutation is unavailable in the read-only production backend")]
    MutationUnavailable,
    #[error("unsupported persisted observation: {0}")]
    UnsupportedObservation(&'static str),
}

#[derive(Debug, Clone)]
pub struct ProductionRepository {
    pub identity: crate::lifecycle::RepositoryIdentity,
}

#[derive(Debug, Clone)]
pub struct ProductionBackend {
    anchor: PathBuf,
}

struct FileArtifactProbe<'a> {
    kind: crate::planner::FileArtifactKind,
    destination: &'a Path,
    bytes: u64,
    digest: &'a ObjectId,
    fingerprint: &'a ObjectId,
    mode_policy: crate::planner::FileModePolicy,
    link_target: Option<&'a Path>,
}

impl ProductionBackend {
    pub fn new(anchor: PathBuf) -> Self {
        Self { anchor }
    }

    fn list(&self) -> Result<crate::domain::ListResult, ProductionBackendError> {
        infrastructure::readonly_list(&self.anchor).map_err(Into::into)
    }

    fn worktree(
        &self,
        path: &Path,
    ) -> Result<Option<crate::domain::Worktree>, ProductionBackendError> {
        Ok(self
            .list()?
            .data
            .worktrees
            .into_iter()
            .find(|item| infrastructure::readonly_same_path(&item.path, path)))
    }

    fn parent_safe(path: &Path) -> Result<bool, ProductionBackendError> {
        Ok(infrastructure::readonly_safe_directory(path)?)
    }

    fn create_source_oid(&self, step: &PlanStep) -> Result<ObjectId, ProductionBackendError> {
        let StepAction::CreateWorktree { source, .. } = step.action() else {
            return Err(ProductionBackendError::UnsupportedObservation(
                "create contract requested for non-create action",
            ));
        };
        let expected_ref = match source {
            crate::lifecycle::CreateSource::NewBranch { base, .. } => base
                .as_ref()
                .map_or("HEAD", |value| value.as_str())
                .to_owned(),
            crate::lifecycle::CreateSource::ExistingLocal { branch } => branch.as_str().to_owned(),
            crate::lifecycle::CreateSource::RemoteTracking {
                remote,
                remote_branch,
                ..
            } => format!("refs/remotes/{remote}/{remote_branch}"),
        };
        let matches: Vec<_> = step
            .preconditions()
            .iter()
            .filter_map(|condition| match condition {
                Precondition::RefAt { reference, oid }
                    if reference.as_str() == expected_ref
                        || (matches!(
                            source,
                            crate::lifecycle::CreateSource::ExistingLocal { .. }
                        ) && reference.as_str() == format!("refs/heads/{expected_ref}")) =>
                {
                    Some(oid.clone())
                }
                _ => None,
            })
            .collect();
        match matches.as_slice() {
            [oid] => Ok(oid.clone()),
            [] => Err(ProductionBackendError::UnsupportedObservation(
                "create source lacks matching RefAt OID",
            )),
            _ => Err(ProductionBackendError::UnsupportedObservation(
                "create source has duplicate matching RefAt OIDs",
            )),
        }
    }

    fn ref_at(&self, reference: &str) -> Result<Option<ObjectId>, ProductionBackendError> {
        infrastructure::readonly_ref_oid(&self.anchor, reference).map_err(Into::into)
    }

    fn authoritative_ref(
        &self,
        plan: &OperationPlan,
        reference: &str,
        provenance: Option<&crate::lifecycle::MergeTargetProvenance>,
    ) -> Result<Option<ObjectId>, ProductionBackendError> {
        if reference != "HEAD" {
            return self.ref_at(reference);
        }
        let root = match plan.intent() {
            crate::lifecycle::OperationIntent::Create(intent) => intent
                .current_worktree_root
                .as_ref()
                .map(|path| path.as_path())
                .ok_or(ProductionBackendError::UnsupportedObservation(
                    "CreateIntent HEAD authority is missing",
                ))?,
            crate::lifecycle::OperationIntent::Remove(_) => match provenance {
                None | Some(crate::lifecycle::MergeTargetProvenance::Primary) => {
                    plan.repository().primary_root.as_path()
                }
                _ => {
                    return Err(ProductionBackendError::UnsupportedObservation(
                        "RemoveIntent HEAD authority is not primary",
                    ));
                }
            },
        };
        infrastructure::readonly_ref_oid(root, "HEAD").map_err(Into::into)
    }

    fn remote_ref(
        &self,
        remote: &str,
        branch: &str,
    ) -> Result<Option<ObjectId>, ProductionBackendError> {
        infrastructure::readonly_remote_ref(&self.anchor, remote, branch).map_err(Into::into)
    }

    fn remote_default(
        &self,
        target: &crate::lifecycle::RemoteBranch,
    ) -> Result<bool, ProductionBackendError> {
        let expected = format!("refs/heads/{}", target.branch);
        Ok(
            infrastructure::readonly_remote_default(&self.anchor, target.remote.as_str())?
                == expected,
        )
    }

    fn branch_upstream(&self, branch: &str) -> Result<Option<String>, ProductionBackendError> {
        infrastructure::readonly_branch_upstream(&self.anchor, branch).map_err(Into::into)
    }

    fn artifact_source(
        &self,
        root: &Path,
        source: &Path,
        kind: crate::planner::FileArtifactKind,
        expected_bytes: u64,
        digest: &ObjectId,
    ) -> Result<bool, ProductionBackendError> {
        let root = infrastructure::readonly_normalize(root.to_owned());
        let source = infrastructure::readonly_normalize(source.to_owned());
        if !source.starts_with(&root) || !infrastructure::readonly_safe_parent_of(&source)? {
            return Ok(false);
        }
        let Some(node) = infrastructure::readonly_observe_node(&root, &source)? else {
            return Ok(false);
        };
        let data = match (kind, node) {
            (
                crate::planner::FileArtifactKind::CopyFile,
                infrastructure::ObservedNode::Regular { bytes, .. },
            ) => {
                return Ok(bytes.len() as u64 == expected_bytes
                    && crate::planner::artifact_digest(&bytes) == *digest);
            }
            (
                crate::planner::FileArtifactKind::CreateSymlink,
                infrastructure::ObservedNode::Regular { .. },
            )
            | (
                crate::planner::FileArtifactKind::CreateSymlink,
                infrastructure::ObservedNode::Directory,
            ) => source.as_os_str().as_encoded_bytes().to_vec(),
            (
                crate::planner::FileArtifactKind::RelinkSymlink,
                infrastructure::ObservedNode::Symlink { target },
            ) => target.as_os_str().as_encoded_bytes().to_vec(),
            _ => return Ok(false),
        };
        Ok(
            data.len() as u64 == expected_bytes
                && crate::planner::artifact_digest(&data) == *digest,
        )
    }

    fn condition(
        &mut self,
        plan: &OperationPlan,
        step: Option<&PlanStep>,
        condition: &Precondition,
    ) -> Result<bool, ProductionBackendError> {
        let listing = || self.list();
        Ok(match condition {
            Precondition::CommonDirectory(path) => infrastructure::readonly_same_path(path.as_path(), &self.discover_repository()?.identity.common_dir.into_path()),
            Precondition::ExactlyOnePrimary => listing()?.data.worktrees.iter().filter(|w| w.classification == WorktreeClass::Primary).count() == 1,
            Precondition::BareRepositoryFalse => !listing()?.data.repository.bare,
            Precondition::PathAbsent(path) => {
                infrastructure::readonly_final_absent(path.as_path())?
            }
            Precondition::ParentSafe(path) => Self::parent_safe(path.as_path())?,
            Precondition::RefAbsent(reference) => self.authoritative_ref(plan, reference.as_str(), None)?.is_none(),
            Precondition::RefAt { reference, oid } => self.authoritative_ref(plan, reference.as_str(), None)? == Some(oid.clone()),
            Precondition::RefMergedInto { reference, target_ref, target_oid, provenance } => {
                let source = match self.authoritative_ref(plan, reference.as_str(), Some(provenance))? { Some(v) => v, None => return Ok(false) };
                let target = match target_ref { Some(r) => match self.authoritative_ref(plan, r.as_str(), Some(provenance))? { Some(value) => value, None => return Ok(false) }, None => target_oid.clone() };
                target == *target_oid && infrastructure::readonly_ancestor(&self.anchor, &source, &target)?
            }
            Precondition::BranchUpstreamIs { branch, upstream_ref } => self.branch_upstream(branch.as_str())?.as_deref() == Some(upstream_ref.as_str()),
            Precondition::WorktreeAt { path, branch, oid, class } => self.worktree(path.as_path())?.is_some_and(|w| w.branch.as_deref() == Some(branch.as_str()) && w.head_oid.as_deref() == Some(oid.as_str()) && w.classification == *class),
            Precondition::SymlinkAt { path, target_digest } => {
                matches!(infrastructure::readonly_observe_absolute_node(path.as_path())?, Some(infrastructure::ObservedNode::Symlink { target }) if crate::planner::artifact_digest(target.as_os_str().as_encoded_bytes()) == *target_digest)
            }
            Precondition::RemoteRefAt { remote, branch, oid } => self.remote_ref(remote.as_str(), branch.as_str())? == Some(oid.clone()),
            Precondition::WorktreeRegistered { path, oid } => self.worktree(path.as_path())?.is_some_and(|w| w.head_oid.as_deref() == Some(oid.as_str())),
            Precondition::WorktreeClass { path, class } => self.worktree(path.as_path())?.is_some_and(|w| w.classification == *class),
            Precondition::WorktreeUnlocked { path } => self.worktree(path.as_path())?.is_some_and(|w| w.locked.is_none()),
            Precondition::WorktreeNotPrunable { path } => self.worktree(path.as_path())?.is_some_and(|w| w.prunable.is_none()),
            Precondition::WorktreeClean { path } => self.worktree(path.as_path())?.is_some_and(|w| w.status == CheckoutStatus::Clean),
            Precondition::NoOngoingGitOperation { path } => !infrastructure::readonly_ongoing(path.as_path())?,
            Precondition::BranchNotElsewhere(branch) => listing()?.data.worktrees.iter().all(|w| {
                w.branch.as_deref() != Some(branch.as_str()) || matches!(&plan.intent(), crate::lifecycle::OperationIntent::Remove(intent) if infrastructure::readonly_same_path(w.path.as_path(), intent.worktree.as_path()))
            }),
            Precondition::BranchNotCheckedOut(branch) => listing()?.data.worktrees.iter().all(|w| w.branch.as_deref() != Some(branch.as_str())),
            Precondition::RemoteBranchNotDefault(target) => !self.remote_default(target)?,
            Precondition::SourceManifest { .. } => return Err(ProductionBackendError::UnsupportedObservation("legacy SourceManifest")),
            Precondition::ArtifactSourceAt { rule, source_root, source, destination, bytes, digest, manifest_digest } => {
                let Some(StepAction::FileArtifact { rule: action_rule, kind, source: action_source, destination: action_destination, digest: action_digest, manifest_digest: action_manifest, .. }) = step.map(|value| value.action()) else { return Err(ProductionBackendError::UnsupportedObservation("artifact guard without FileArtifact step")); };
                if rule != action_rule || source != action_source || destination != action_destination || digest != action_digest || manifest_digest != action_manifest || !matches!(kind, crate::planner::FileArtifactKind::CopyFile | crate::planner::FileArtifactKind::CreateSymlink | crate::planner::FileArtifactKind::RelinkSymlink) { return Ok(false); }
                self.artifact_source(source_root.as_path(), source.as_path(), *kind, *bytes, digest)?
            }
        })
    }
}

impl ExecutionBackend for ProductionBackend {
    type Error = ProductionBackendError;
    type Repository = ProductionRepository;

    fn discover_repository(&mut self) -> Result<Self::Repository, Self::Error> {
        let data = infrastructure::readonly_list(&self.anchor)?.data;
        if data.repository.bare {
            return Err(ProductionBackendError::Git(GitError::Discovery(
                "bare repository".into(),
            )));
        }
        let primary = data
            .worktrees
            .iter()
            .find(|w| w.classification == WorktreeClass::Primary)
            .ok_or_else(|| GitError::Discovery("no primary worktree".into()))?;
        let oid = primary
            .head_oid
            .as_deref()
            .ok_or_else(|| GitError::Discovery("unborn primary".into()))
            .and_then(|v| ObjectId::new(v).map_err(GitError::Parse))?;
        let common = data.repository.common_dir.canonicalize()?;
        let root = primary.path.canonicalize()?;
        Ok(ProductionRepository {
            identity: crate::lifecycle::RepositoryIdentity {
                common_dir: common.into(),
                primary_root: root.into(),
                repository_oid: oid,
            },
        })
    }
    fn repository_common_dir<'a>(&self, repository: &'a Self::Repository) -> &'a Path {
        repository.identity.common_dir.as_path()
    }
    fn repository_matches_plan(&self, repository: &Self::Repository, plan: &OperationPlan) -> bool {
        repository.identity.repository_oid == plan.repository().repository_oid
            && infrastructure::readonly_same_path(
                repository.identity.common_dir.as_path(),
                plan.repository().common_dir.as_path(),
            )
            && infrastructure::readonly_same_path(
                repository.identity.primary_root.as_path(),
                plan.repository().primary_root.as_path(),
            )
    }
    fn supports_precondition(
        &self,
        _plan: &OperationPlan,
        step: Option<&PlanStep>,
        precondition: &Precondition,
    ) -> bool {
        if matches!(precondition, Precondition::SourceManifest { .. }) {
            return false;
        }
        match (precondition, step.map(|value| value.action())) {
            (
                Precondition::ArtifactSourceAt {
                    rule,
                    source,
                    destination,
                    manifest_digest,
                    ..
                },
                Some(StepAction::FileArtifact {
                    rule: action_rule,
                    source: action_source,
                    destination: action_destination,
                    manifest_digest: action_manifest,
                    ..
                }),
            ) => {
                rule == action_rule
                    && source == action_source
                    && destination == action_destination
                    && manifest_digest == action_manifest
            }
            (Precondition::ArtifactSourceAt { .. }, _) => false,
            _ => true,
        }
    }
    fn supports_action(&self, step: &PlanStep) -> bool {
        match step.action() {
            StepAction::CreateWorktree { .. }
            | StepAction::DeleteLocalBranch { .. }
            | StepAction::DeleteRemoteBranch {
                expected_oid: Some(_),
                ..
            } => true,
            StepAction::RemoveWorktree { path } => step.preconditions().iter().any(|condition| {
                matches!(condition, Precondition::WorktreeClean { path: guarded } if guarded == path)
            }),
            _ => false,
        }
    }
    fn probe_capability(&self, step: &PlanStep) -> ProbeCapability {
        if matches!(step.action(), StepAction::RunTask { .. }) {
            ProbeCapability::UnknownAfterCrash
        } else {
            ProbeCapability::Deterministic
        }
    }
    fn check_precondition(
        &mut self,
        plan: &OperationPlan,
        step: Option<&PlanStep>,
        precondition: &Precondition,
    ) -> Result<crate::execution::ConditionResult, Self::Error> {
        if !self.supports_precondition(plan, step, precondition) {
            return Err(ProductionBackendError::UnsupportedObservation(
                "artifact guard without FileArtifact step",
            ));
        }
        self.condition(plan, step, precondition).map(|v| {
            if v {
                ConditionResult::Satisfied
            } else {
                ConditionResult::Unsatisfied
            }
        })
    }
    fn invoke(&mut self, step: &PlanStep) -> Result<(), Self::Error> {
        match step.action() {
            StepAction::CreateWorktree {
                destination,
                source,
            } => {
                let source_oid = self.create_source_oid(step)?;
                infrastructure::mutate_create_worktree(
                    &self.anchor,
                    destination.as_path(),
                    source,
                    &source_oid,
                )?;
                Ok(())
            }
            StepAction::RemoveWorktree { path } => {
                infrastructure::mutate_remove_worktree(&self.anchor, path.as_path())?;
                Ok(())
            }
            StepAction::DeleteLocalBranch { branch } => {
                let expected = step
                    .preconditions()
                    .iter()
                    .find_map(|condition| match condition {
                        Precondition::RefAt { reference, oid }
                            if reference.as_str() == branch.as_str()
                                || reference.as_str() == format!("refs/heads/{branch}") =>
                        {
                            Some(oid)
                        }
                        _ => None,
                    })
                    .ok_or(ProductionBackendError::UnsupportedObservation(
                        "local branch deletion lacks expected OID",
                    ))?;
                infrastructure::mutate_delete_local_branch(
                    &self.anchor,
                    branch.as_str(),
                    expected,
                )?;
                Ok(())
            }
            StepAction::DeleteRemoteBranch {
                target,
                expected_oid: Some(expected),
            } => {
                infrastructure::mutate_delete_remote_branch(
                    &self.anchor,
                    target.remote.as_str(),
                    target.branch.as_str(),
                    expected,
                )?;
                Ok(())
            }
            _ => Err(ProductionBackendError::MutationUnavailable),
        }
    }
    fn probe(
        &mut self,
        step: &PlanStep,
        _context: ProbeContext,
    ) -> Result<ProbeVerdict, Self::Error> {
        if matches!(step.action(), StepAction::RunTask { .. }) {
            return Ok(ProbeVerdict::Unknown);
        }
        if let StepAction::FileArtifact {
            kind,
            destination,
            bytes,
            digest,
            fingerprint,
            link_target,
            mode_policy,
            ..
        } = step.action()
        {
            return Ok(
                if self.file_artifact_applied(FileArtifactProbe {
                    kind: *kind,
                    destination: destination.as_path(),
                    bytes: *bytes,
                    digest,
                    fingerprint,
                    mode_policy: *mode_policy,
                    link_target: link_target.as_ref().map(|v| v.as_path()),
                })? {
                    ProbeVerdict::Applied
                } else {
                    ProbeVerdict::NotApplied
                },
            );
        }
        if let StepAction::CreateWorktree {
            destination,
            source,
        } = step.action()
        {
            let intended_branch = match source {
                crate::lifecycle::CreateSource::NewBranch { branch, .. }
                | crate::lifecycle::CreateSource::ExistingLocal { branch }
                | crate::lifecycle::CreateSource::RemoteTracking {
                    local_branch: branch,
                    ..
                } => branch,
            };
            let mut applied = true;
            for post in step.postconditions() {
                let value = match post {
                    Postcondition::WorktreeCreated { path, oid } => {
                        self.worktree(path.as_path())?.is_some_and(|w| {
                            infrastructure::readonly_same_path(&w.path, destination.as_path())
                                && w.head_oid.as_deref() == Some(oid.as_str())
                                && w.branch.as_deref() == Some(intended_branch.as_str())
                                && w.classification == WorktreeClass::Linked
                        })
                    }
                    other => self.postcondition(other)?,
                };
                if !value {
                    applied = false;
                    break;
                }
            }
            return Ok(if applied {
                ProbeVerdict::Applied
            } else {
                ProbeVerdict::NotApplied
            });
        }
        let mut applied = true;
        for post in step.postconditions() {
            if !self.postcondition(post)? {
                applied = false;
                break;
            }
        }
        Ok(if applied {
            ProbeVerdict::Applied
        } else {
            ProbeVerdict::NotApplied
        })
    }
}

impl ProductionBackend {
    fn file_artifact_applied(
        &self,
        contract: FileArtifactProbe<'_>,
    ) -> Result<bool, ProductionBackendError> {
        let Some(node) = infrastructure::readonly_observe_absolute_node(contract.destination)?
        else {
            return Ok(false);
        };
        match (contract.kind, node) {
            (crate::planner::FileArtifactKind::CopyFile, node) => {
                let infrastructure::ObservedNode::Regular { bytes: data, mode } = node else {
                    return Ok(false);
                };
                #[cfg(unix)]
                {
                    if contract.mode_policy == crate::planner::FileModePolicy::Private
                        && mode & 0o7777 != 0o600
                    {
                        return Ok(false);
                    }
                    if contract.mode_policy == crate::planner::FileModePolicy::PreserveSafe
                        && mode & (0o7000 | 0o022) != 0
                    {
                        return Ok(false);
                    }
                }
                Ok(data.len() as u64 == contract.bytes
                    && crate::planner::artifact_digest(&data) == *contract.digest
                    && *contract.fingerprint == *contract.digest)
            }
            (crate::planner::FileArtifactKind::CreateSymlink, node)
            | (crate::planner::FileArtifactKind::RelinkSymlink, node) => {
                let infrastructure::ObservedNode::Symlink { target: actual } = node else {
                    return Ok(false);
                };
                let Some(expected) = contract.link_target else {
                    return Err(ProductionBackendError::UnsupportedObservation(
                        "symlink action without link target",
                    ));
                };
                let actual_bytes = actual.as_os_str().as_encoded_bytes();
                Ok(actual == expected
                    && actual_bytes.len() as u64 == contract.bytes
                    && crate::planner::artifact_digest(actual_bytes) == *contract.digest
                    && *contract.fingerprint == *contract.digest)
            }
        }
    }

    fn postcondition(&self, post: &Postcondition) -> Result<bool, ProductionBackendError> {
        match post {
            Postcondition::WorktreeCreated { path, oid } => self
                .worktree(path.as_path())
                .map(|w| w.is_some_and(|w| w.head_oid.as_deref() == Some(oid.as_str()))),
            Postcondition::WorktreeRemoved { path, .. } => {
                Ok(self.worktree(path.as_path())?.is_none()
                    && infrastructure::readonly_final_absent(path.as_path())?)
            }
            Postcondition::BranchCreated { branch, oid } => {
                Ok(self.ref_at(&format!("refs/heads/{branch}"))? == Some(oid.clone()))
            }
            Postcondition::BranchUpstreamAt {
                branch,
                remote,
                remote_branch,
            } => Ok(self.branch_upstream(branch.as_str())?
                == Some(format!("refs/remotes/{remote}/{remote_branch}"))),
            Postcondition::BranchDeleted(branch) => {
                Ok(self.ref_at(&format!("refs/heads/{branch}"))?.is_none())
            }
            Postcondition::RemoteBranchDeleted(target) => Ok(self
                .remote_ref(target.remote.as_str(), target.branch.as_str())?
                .is_none()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{ExecutionEngine, ExecutionOutcome};
    use crate::lifecycle::{BranchName, RefName, StepId};
    use std::collections::{BTreeMap, BTreeSet};
    use std::{fs, process::Command};
    use tempfile::TempDir;

    fn git(cwd: &Path, args: &[&str]) {
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
    }

    fn repository() -> TempDir {
        let temp = TempDir::new().unwrap();
        git(temp.path(), &["init", "-b", "main"]);
        git(temp.path(), &["config", "user.name", "D1 test"]);
        git(temp.path(), &["config", "user.email", "d1@example.invalid"]);
        fs::write(temp.path().join("tracked"), b"tracked").unwrap();
        git(temp.path(), &["add", "tracked"]);
        git(temp.path(), &["commit", "-m", "initial"]);
        temp
    }

    #[test]
    fn primary_and_linked_discovery_share_identity() {
        let temp = repository();
        let linked = temp.path().join("linked");
        git(
            temp.path(),
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let primary = ProductionBackend::new(temp.path().to_owned())
            .discover_repository()
            .unwrap();
        let linked_identity = ProductionBackend::new(linked)
            .discover_repository()
            .unwrap();
        assert_eq!(primary.identity, linked_identity.identity);
    }

    #[test]
    fn bare_and_unborn_discovery_are_typed_failures() {
        let bare = TempDir::new().unwrap();
        git(bare.path(), &["init", "--bare"]);
        assert!(matches!(
            ProductionBackend::new(bare.path().to_owned()).discover_repository(),
            Err(ProductionBackendError::Git(GitError::Discovery(_)))
        ));
        let unborn = TempDir::new().unwrap();
        git(unborn.path(), &["init", "-b", "main"]);
        assert!(matches!(
            ProductionBackend::new(unborn.path().to_owned()).discover_repository(),
            Err(ProductionBackendError::Git(GitError::Discovery(_)))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn parent_safe_and_observed_nodes_are_no_follow() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let directory = root.join("directory");
        fs::create_dir(&directory).unwrap();
        fs::write(root.join("file"), b"bytes").unwrap();
        std::os::unix::fs::symlink("file", root.join("link")).unwrap();
        assert!(infrastructure::readonly_safe_directory(&directory).unwrap());
        assert!(matches!(
            infrastructure::readonly_observe_node(root, &root.join("file")).unwrap(),
            Some(infrastructure::ObservedNode::Regular { ref bytes, .. }) if bytes == b"bytes"
        ));
        assert!(matches!(
            infrastructure::readonly_observe_node(root, &directory).unwrap(),
            Some(infrastructure::ObservedNode::Directory)
        ));
        assert!(matches!(
            infrastructure::readonly_observe_node(root, &root.join("link")).unwrap(),
            Some(infrastructure::ObservedNode::Symlink { .. })
        ));
        assert!(infrastructure::readonly_final_absent(&root.join("missing")).unwrap());
        assert!(!infrastructure::readonly_final_absent(&root.join("file")).unwrap());
        assert!(!infrastructure::readonly_final_absent(&root.join("link")).unwrap());
        assert!(!infrastructure::readonly_safe_directory(&root.join("file")).unwrap());
        assert!(!infrastructure::readonly_safe_directory(&root.join("missing")).unwrap());
        std::os::unix::fs::symlink("directory", root.join("ancestor")).unwrap();
        assert!(
            !infrastructure::readonly_safe_directory(&root.join("ancestor").join("child")).unwrap()
        );
        let trusted = root.join("trusted");
        fs::create_dir(&trusted).unwrap();
        fs::write(trusted.join("value"), b"alias").unwrap();
        let alias = root.join("trusted-alias");
        std::os::unix::fs::symlink("trusted", &alias).unwrap();
        assert!(matches!(
            infrastructure::readonly_observe_node(&alias, &alias.join("value")).unwrap(),
            Some(infrastructure::ObservedNode::Regular { .. })
        ));
        std::os::unix::fs::symlink("directory", trusted.join("nested-alias")).unwrap();
        assert!(
            infrastructure::readonly_observe_node(&alias, &alias.join("nested-alias/child"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unsupported_actions_remain_mutation_unavailable() {
        let temp = repository();
        let plan = crate::lifecycle::test_plan(1);
        let backend = ProductionBackend::new(temp.path().to_owned());
        for step in plan.steps() {
            if !backend.supports_action(step) {
                assert!(matches!(
                    ProductionBackend::new(temp.path().to_owned()).invoke(step),
                    Err(ProductionBackendError::MutationUnavailable)
                ));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn copy_probe_enforces_exact_private_and_safe_modes() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("mode");
        fs::write(&file, b"mode").unwrap();
        let digest = crate::planner::artifact_digest(b"mode");
        let backend = ProductionBackend::new(temp.path().to_owned());
        let probe = |mode_policy| {
            backend
                .file_artifact_applied(FileArtifactProbe {
                    kind: crate::planner::FileArtifactKind::CopyFile,
                    destination: &file,
                    bytes: 4,
                    digest: &digest,
                    fingerprint: &digest,
                    mode_policy,
                    link_target: None,
                })
                .unwrap()
        };
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(probe(crate::planner::FileModePolicy::PreserveSafe));
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(probe(crate::planner::FileModePolicy::PreserveSafe));
        assert!(probe(crate::planner::FileModePolicy::Private));
        for mode in [0o4600, 0o2600, 0o1600, 0o666] {
            fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
            assert!(!probe(crate::planner::FileModePolicy::PreserveSafe));
            assert!(!probe(crate::planner::FileModePolicy::Private));
        }
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!probe(crate::planner::FileModePolicy::Private));
    }

    #[test]
    fn git_observers_preserve_missing_false_and_fatal_error_boundaries() {
        let temp = repository();
        assert_eq!(
            infrastructure::readonly_ref_oid(temp.path(), "refs/heads/missing").unwrap(),
            None
        );
        assert!(infrastructure::readonly_ref_oid(Path::new("/"), "HEAD").is_err());
        let oid = infrastructure::readonly_ref_oid(temp.path(), "HEAD")
            .unwrap()
            .unwrap();
        fs::write(temp.path().join("second"), b"second").unwrap();
        git(temp.path(), &["add", "second"]);
        git(temp.path(), &["commit", "-m", "second"]);
        let newer = infrastructure::readonly_ref_oid(temp.path(), "HEAD")
            .unwrap()
            .unwrap();
        assert!(!infrastructure::readonly_ancestor(temp.path(), &newer, &oid).unwrap());
        let unrelated = ObjectId::new("0123456789012345678901234567890123456789").unwrap();
        assert!(infrastructure::readonly_ancestor(temp.path(), &unrelated, &oid).is_err());
        assert!(infrastructure::readonly_remote_ref(temp.path(), "-origin", "main").is_err());
    }

    #[test]
    fn unconfigured_or_symbolic_headless_remotes_are_observer_errors() {
        let temp = repository();
        assert!(infrastructure::readonly_remote_default(temp.path(), "missing").is_err());
        let remote = TempDir::new().unwrap();
        git(remote.path(), &["init", "--bare"]);
        git(
            temp.path(),
            &["remote", "add", "empty", remote.path().to_str().unwrap()],
        );
        assert!(infrastructure::readonly_remote_default(temp.path(), "empty").is_err());
    }

    fn output(cwd: &Path, args: &[&str]) -> String {
        let value = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            value.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&value.stderr)
        );
        String::from_utf8(value.stdout).unwrap().trim().to_owned()
    }

    fn oid(cwd: &Path, reference: &str) -> ObjectId {
        ObjectId::new(output(cwd, &["rev-parse", reference])).unwrap()
    }

    fn stored(path: impl Into<PathBuf>) -> crate::domain::StoredPath {
        path.into().into()
    }

    fn check(
        backend: &mut ProductionBackend,
        plan: &crate::lifecycle::OperationPlan,
        condition: Precondition,
        expected: bool,
    ) {
        let result = backend.check_precondition(plan, None, &condition);
        assert_eq!(
            result.unwrap(),
            if expected {
                ConditionResult::Satisfied
            } else {
                ConditionResult::Unsatisfied
            },
            "condition {condition:?}"
        );
    }

    fn executable_new_branch_plan(
        root: &Path,
        destination: &Path,
        branch: &str,
    ) -> crate::lifecycle::OperationPlan {
        let repository = ProductionBackend::new(root.to_owned())
            .discover_repository()
            .unwrap()
            .identity;
        let branch = BranchName::new(branch).unwrap();
        let source = crate::lifecycle::CreateSource::NewBranch {
            branch: branch.clone(),
            base: Some(RefName::new("refs/heads/main").unwrap()),
        };
        let intent = crate::lifecycle::CreateIntent {
            repository: repository.clone(),
            source: source.clone(),
            destination: Some(stored(destination.to_owned())),
            selected_tasks: BTreeSet::new(),
            skipped_rules: BTreeSet::new(),
            granted_consents: BTreeSet::new(),
            task_contracts: BTreeMap::new(),
            current_worktree_root: Some(stored(root.to_owned())),
            artifact_rule_contracts: BTreeMap::new(),
        };
        crate::planner::plan_create(crate::planner::CreatePlanInput {
            operation_id: crate::planner::new_operation_id(),
            repository: repository.clone(),
            intent,
            bare: false,
            primary_count: 1,
            invocation_cwd: stored(root.to_owned()),
            primary_root: repository.primary_root.clone(),
            current_worktree_root: repository.primary_root.clone(),
            destination: crate::planner::DestinationFacts {
                path: stored(destination.to_owned()),
                state: crate::planner::DestinationState::Absent,
                parent: stored(destination.parent().unwrap().to_owned()),
                parent_safe: true,
            },
            source_facts: crate::planner::CreateSourceFacts::NewBranch {
                branch,
                base_ref: RefName::new("refs/heads/main").unwrap(),
                base_oid: oid(root, "refs/heads/main"),
                branch_absent: true,
            },
            branch_checked_out: false,
            branch_collision: false,
            known_rules: BTreeSet::new(),
            enabled_rules: BTreeSet::new(),
            known_tasks: BTreeSet::new(),
            manifests: vec![],
            tasks: vec![],
        })
        .unwrap()
    }

    #[test]
    fn engine_applies_new_branch_after_post_success_injected_mutation_error() {
        let temp = repository();
        let root = temp.path();
        let destination = root.join("engine-linked");
        let branch = "engine-branch";
        let plan = executable_new_branch_plan(root, &destination, branch);
        assert!(
            plan.validate_executable_plan().is_ok(),
            "schema-v2 zero-artifact plan must be executable"
        );
        let expected_oid = oid(root, "refs/heads/main");
        let guard = infrastructure::arm_mutation_success_error();
        let outcome = ExecutionEngine::new(ProductionBackend::new(root.to_owned()))
            .execute(plan.clone())
            .unwrap();
        assert_eq!(
            infrastructure::mutation_invocation_count(),
            1,
            "mutation invoke count"
        );
        assert!(
            matches!(outcome, ExecutionOutcome::Applied { .. }),
            "post-success error reconciles as Applied"
        );
        assert_eq!(oid(root, &format!("refs/heads/{branch}")), expected_oid);
        assert_eq!(oid(&destination, "HEAD"), expected_oid);
        let common = root.join(".git");
        let journal = crate::journal_store::JournalStore::new(&common)
            .read(plan.operation_id())
            .unwrap();
        assert_eq!(journal.status(), crate::journal::OperationStatus::Applied);
        assert_eq!(
            journal.revision(),
            2,
            "pending -> Started -> Applied journal transitions"
        );
        assert_eq!(
            journal.steps()[0].status(),
            crate::journal::StepStatus::Applied
        );
        assert!(!journal.is_unresolved());
        assert!(
            journal.started_step().is_none(),
            "final journal has no active Started step"
        );
        drop(guard);
        assert_eq!(
            infrastructure::mutation_invocation_count(),
            0,
            "fault seam RAII reset"
        );
    }

    #[test]
    fn mutation_fault_guard_resets_on_drop_and_unwind() {
        let guard = infrastructure::arm_mutation_success_error();
        assert_eq!(infrastructure::mutation_invocation_count(), 0);
        drop(guard);
        assert_eq!(infrastructure::mutation_invocation_count(), 0);
        let unwind = std::panic::catch_unwind(|| {
            let _guard = infrastructure::arm_mutation_success_error();
            panic!("exercise RAII reset");
        });
        assert!(unwind.is_err());
        assert_eq!(
            infrastructure::mutation_invocation_count(),
            0,
            "fault state does not leak after panic"
        );
    }

    #[test]
    fn engine_rejects_unsupported_remove_before_journal_and_requires_exact_clean_guard() {
        let temp = repository();
        let root = temp.path();
        let destination = root.join("unsupported-remove");
        git(
            root,
            &[
                "worktree",
                "add",
                "-b",
                "remove-me",
                destination.to_str().unwrap(),
            ],
        );
        fs::write(destination.join("dirty"), b"dirty").unwrap();
        let discovered = ProductionBackend::new(root.to_owned())
            .discover_repository()
            .unwrap();
        let listing = infrastructure::readonly_list(root).unwrap();
        let worktree = listing
            .data
            .worktrees
            .iter()
            .find(|item| infrastructure::readonly_same_path(&item.path, &destination))
            .unwrap();
        let branch = BranchName::new(worktree.branch.clone().unwrap()).unwrap();
        let worktree_oid = ObjectId::new(worktree.head_oid.clone().unwrap()).unwrap();
        let dirty_facts = crate::planner::RemoveFacts {
            repository: discovered.identity.clone(),
            class: worktree.classification,
            locked: worktree.locked.is_some(),
            prunable: worktree.prunable.is_some(),
            ongoing: infrastructure::readonly_ongoing(&destination).unwrap(),
            oid_matches: true,
            branch_elsewhere: false,
            dirty: true,
            local_branch_safe_to_delete: false,
            safe_target_ref: RefName::new("HEAD").unwrap(),
            safe_target: oid(root, "HEAD"),
            merge_provenance: crate::lifecycle::MergeTargetProvenance::Primary,
            branch: branch.clone(),
            branch_oid: worktree_oid.clone(),
            worktree_oid: worktree_oid.clone(),
            remote_branch: None,
            remote_branch_oid: None,
            remote_is_default: false,
            path: stored(destination.clone()),
        };
        let mut grants = BTreeSet::new();
        grants.insert(crate::lifecycle::ConsentId::new("remove:worktree").unwrap());
        grants.insert(crate::lifecycle::ConsentId::new("remove:dirty").unwrap());
        let intent = crate::lifecycle::RemoveIntent::new(
            discovered.identity.clone(),
            stored(destination.clone()),
            true,
            false,
            false,
            None,
            grants,
        )
        .unwrap();
        let plan = crate::planner::plan_remove(crate::planner::RemovePlanInput {
            operation_id: crate::planner::new_operation_id(),
            intent,
            facts: dirty_facts,
        })
        .unwrap();
        assert!(
            plan.validate_executable_plan().is_ok(),
            "genuine schema-v2 dirty remove plan is valid"
        );
        assert!(
            !plan.steps()[0]
                .preconditions()
                .iter()
                .any(|guard| matches!(guard, Precondition::WorktreeClean { .. }))
        );
        let before_worktrees = output(root, &["worktree", "list", "--porcelain", "-z"]);
        let before_refs = output(root, &["show-ref"]);
        let before_dirty = fs::read(destination.join("dirty")).unwrap();
        assert!(matches!(
            ExecutionEngine::new(ProductionBackend::new(root.to_owned())).execute(plan),
            Err(crate::execution::ExecutionError::UnsupportedPlan(_))
        ));
        assert!(
            !root.join(".git/ewtm").exists(),
            "support scan precedes journal creation"
        );
        assert!(!root.join(".git/ewtm/repository.lock").exists());
        assert_eq!(
            output(root, &["worktree", "list", "--porcelain", "-z"]),
            before_worktrees
        );
        assert_eq!(output(root, &["show-ref"]), before_refs);
        assert_eq!(fs::read(destination.join("dirty")).unwrap(), before_dirty);
        assert_eq!(
            infrastructure::mutation_invocation_count(),
            0,
            "support scan performs no mutation"
        );
        assert!(destination.exists());
        assert_eq!(oid(root, "refs/heads/remove-me"), oid(&destination, "HEAD"));

        let clean = PlanStep::new(
            StepId::new("clean-remove").unwrap(),
            "clean-remove".into(),
            StepAction::RemoveWorktree {
                path: stored(destination.clone()),
            },
            vec![Precondition::WorktreeClean {
                path: stored(destination.clone()),
            }],
            vec![],
            None,
            false,
        )
        .unwrap();
        let backend = ProductionBackend::new(root.to_owned());
        assert!(backend.supports_action(&clean));
        let mut extra = clean.clone();
        extra
            .preconditions_mut()
            .push(Precondition::BareRepositoryFalse);
        assert!(
            ProductionBackend::new(root.to_owned()).supports_action(&extra),
            "dirty-removal consent does not replace exact clean guard"
        );
        let mut missing = clean.clone();
        missing.preconditions_mut().clear();
        assert!(!ProductionBackend::new(root.to_owned()).supports_action(&missing));
        let mut mismatched = clean;
        mismatched.preconditions_mut()[0] = Precondition::WorktreeClean {
            path: stored(root.join("other")),
        };
        assert!(!ProductionBackend::new(root.to_owned()).supports_action(&mismatched));
    }

    #[test]
    fn schema2_preconditions_are_observed_as_typed_satisfied_or_unsatisfied() {
        let temp = repository();
        let root = temp.path();
        let mut plan_wire = serde_json::to_value(crate::lifecycle::test_plan(1)).unwrap();
        plan_wire["intent"]["Create"]["current_worktree_root"] = serde_json::json!(root);
        let plan: crate::lifecycle::OperationPlan = serde_json::from_value(plan_wire).unwrap();
        let head = oid(root, "HEAD");
        let common = output(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );
        let primary = root.to_owned();
        let mut backend = ProductionBackend::new(root.to_owned());

        check(
            &mut backend,
            &plan,
            Precondition::CommonDirectory(stored(common)),
            true,
        );
        check(&mut backend, &plan, Precondition::ExactlyOnePrimary, true);
        check(&mut backend, &plan, Precondition::BareRepositoryFalse, true);
        check(
            &mut backend,
            &plan,
            Precondition::RefAbsent(RefName::new("refs/heads/nope").unwrap()),
            true,
        );
        check(
            &mut backend,
            &plan,
            Precondition::RefAt {
                reference: RefName::new("HEAD").unwrap(),
                oid: head.clone(),
            },
            true,
        );
        check(
            &mut backend,
            &plan,
            Precondition::RefAt {
                reference: RefName::new("HEAD").unwrap(),
                oid: ObjectId::new("0".repeat(40)).unwrap(),
            },
            false,
        );
        check(
            &mut backend,
            &plan,
            Precondition::BranchUpstreamIs {
                branch: BranchName::new("main").unwrap(),
                upstream_ref: RefName::new("refs/remotes/origin/main").unwrap(),
            },
            false,
        );
        check(
            &mut backend,
            &plan,
            Precondition::WorktreeAt {
                path: stored(primary.clone()),
                branch: BranchName::new("main").unwrap(),
                oid: head.clone(),
                class: WorktreeClass::Primary,
            },
            true,
        );
        check(
            &mut backend,
            &plan,
            Precondition::WorktreeRegistered {
                path: stored(primary.clone()),
                oid: head.clone(),
            },
            true,
        );
        check(
            &mut backend,
            &plan,
            Precondition::WorktreeClass {
                path: stored(primary.clone()),
                class: WorktreeClass::Primary,
            },
            true,
        );
        check(
            &mut backend,
            &plan,
            Precondition::WorktreeUnlocked {
                path: stored(primary.clone()),
            },
            true,
        );
        check(
            &mut backend,
            &plan,
            Precondition::WorktreeNotPrunable {
                path: stored(primary.clone()),
            },
            true,
        );
        check(
            &mut backend,
            &plan,
            Precondition::WorktreeClean {
                path: stored(primary.clone()),
            },
            true,
        );
        check(
            &mut backend,
            &plan,
            Precondition::NoOngoingGitOperation {
                path: stored(primary.clone()),
            },
            true,
        );
        check(
            &mut backend,
            &plan,
            Precondition::BranchNotElsewhere(BranchName::new("unused").unwrap()),
            true,
        );
        check(
            &mut backend,
            &plan,
            Precondition::BranchNotCheckedOut(BranchName::new("unused").unwrap()),
            true,
        );
        check(
            &mut backend,
            &plan,
            Precondition::ParentSafe(stored(root.join("missing"))),
            false,
        );
        let absent = root.join("absent");
        check(
            &mut backend,
            &plan,
            Precondition::PathAbsent(stored(absent.clone())),
            true,
        );
        fs::write(&absent, b"now present").unwrap();
        check(
            &mut backend,
            &plan,
            Precondition::PathAbsent(stored(absent)),
            false,
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let dangling = root.join("dangling");
            symlink("not-there", &dangling).unwrap();
            check(
                &mut backend,
                &plan,
                Precondition::PathAbsent(stored(dangling)),
                false,
            );
            let link = root.join("link");
            symlink("target", &link).unwrap();
            let digest = crate::planner::artifact_digest(b"target");
            check(
                &mut backend,
                &plan,
                Precondition::SymlinkAt {
                    path: stored(link.clone()),
                    target_digest: digest.clone(),
                },
                true,
            );
            check(
                &mut backend,
                &plan,
                Precondition::SymlinkAt {
                    path: stored(link),
                    target_digest: head.clone(),
                },
                false,
            );
            check(
                &mut backend,
                &plan,
                Precondition::ParentSafe(stored(root.join("file/child"))),
                false,
            );
            symlink(".", root.join("component")).unwrap();
            check(
                &mut backend,
                &plan,
                Precondition::ParentSafe(stored(root.join("component/child"))),
                false,
            );
        }

        let fatal = Precondition::RefAt {
            reference: RefName::new("refs/heads/")
                .unwrap_or_else(|_| RefName::new("refs/heads/main").unwrap()),
            oid: head,
        };
        assert!(backend.check_precondition(&plan, None, &fatal).is_ok());
        assert!(!backend.supports_precondition(
            &plan,
            None,
            &Precondition::SourceManifest {
                rule: "r".into(),
                source: stored(root.join("s")),
                destination: stored(root.join("d")),
                digest: ObjectId::new("0".repeat(40)).unwrap()
            }
        ));
    }

    #[test]
    fn head_ref_uses_create_current_worktree_authority() {
        let temp = repository();
        let primary = temp.path().to_owned();
        let linked = temp.path().join("linked-head");
        git(
            temp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "linked-head",
                linked.to_str().unwrap(),
            ],
        );
        fs::write(linked.join("different"), b"different").unwrap();
        git(&linked, &["add", "different"]);
        git(&linked, &["commit", "-m", "different"]);
        let expected = oid(&primary, "HEAD");
        let wrong = oid(&linked, "HEAD");
        let mut wire = serde_json::to_value(crate::lifecycle::test_plan(1)).unwrap();
        wire["intent"]["Create"]["current_worktree_root"] = serde_json::json!(primary);
        let plan: crate::lifecycle::OperationPlan = serde_json::from_value(wire).unwrap();
        let mut backend = ProductionBackend::new(linked);
        check(
            &mut backend,
            &plan,
            Precondition::RefAt {
                reference: RefName::new("HEAD").unwrap(),
                oid: expected,
            },
            true,
        );
        check(
            &mut backend,
            &plan,
            Precondition::RefAt {
                reference: RefName::new("HEAD").unwrap(),
                oid: wrong,
            },
            false,
        );
    }

    #[test]
    fn remove_primary_merge_head_uses_persisted_primary_root() {
        let temp = repository();
        let primary = temp.path().to_owned();
        let primary_oid = oid(&primary, "HEAD");
        git(&primary, &["branch", "old"]);
        let linked = temp.path().join("linked-merge");
        git(
            &primary,
            &[
                "worktree",
                "add",
                "-b",
                "linked-merge",
                linked.to_str().unwrap(),
            ],
        );
        fs::write(linked.join("different"), b"different").unwrap();
        git(&linked, &["add", "different"]);
        git(&linked, &["commit", "-m", "different"]);
        let linked_oid = oid(&linked, "HEAD");
        let base = crate::lifecycle::test_plan(1);
        let remove = crate::lifecycle::RemoveIntent::new(
            base.repository().clone(),
            stored(linked.clone()),
            false,
            false,
            false,
            None,
            Default::default(),
        )
        .unwrap();
        let mut wire = serde_json::to_value(base).unwrap();
        wire["intent"] =
            serde_json::to_value(crate::lifecycle::OperationIntent::Remove(remove)).unwrap();
        wire["repository"]["primary_root"] = serde_json::json!(primary);
        wire["repository"]["common_dir"] = serde_json::json!(primary.join(".git"));
        wire["intent"]["Remove"]["repository"]["primary_root"] = serde_json::json!(primary);
        wire["intent"]["Remove"]["repository"]["common_dir"] =
            serde_json::json!(primary.join(".git"));
        let plan: crate::lifecycle::OperationPlan = serde_json::from_value(wire).unwrap();
        check(
            &mut ProductionBackend::new(linked.clone()),
            &plan,
            Precondition::RefAt {
                reference: RefName::new("HEAD").unwrap(),
                oid: primary_oid.clone(),
            },
            true,
        );
        check(
            &mut ProductionBackend::new(linked.clone()),
            &plan,
            Precondition::RefAt {
                reference: RefName::new("HEAD").unwrap(),
                oid: linked_oid.clone(),
            },
            false,
        );
        let condition = Precondition::RefMergedInto {
            reference: RefName::new("refs/heads/old").unwrap(),
            target_ref: Some(RefName::new("HEAD").unwrap()),
            target_oid: primary_oid.clone(),
            provenance: crate::lifecycle::MergeTargetProvenance::Primary,
        };
        let mut backend = ProductionBackend::new(linked);
        check(&mut backend, &plan, condition, true);
        check(
            &mut backend,
            &plan,
            Precondition::RefMergedInto {
                reference: RefName::new("refs/heads/old").unwrap(),
                target_ref: Some(RefName::new("HEAD").unwrap()),
                target_oid: linked_oid,
                provenance: crate::lifecycle::MergeTargetProvenance::Primary,
            },
            false,
        );
    }

    #[test]
    fn d2_clean_remove_and_local_branch_cas_are_typed_mutations() {
        let temp = repository();
        let root = temp.path();
        git(root, &["branch", "feature"]);
        let expected = oid(root, "refs/heads/feature");
        let delete = PlanStep::new(
            StepId::new("delete").unwrap(),
            "delete".into(),
            StepAction::DeleteLocalBranch {
                branch: BranchName::new("feature").unwrap(),
            },
            vec![Precondition::RefAt {
                reference: RefName::new("feature").unwrap(),
                oid: expected.clone(),
            }],
            vec![],
            None,
            false,
        )
        .unwrap();
        ProductionBackend::new(root.to_owned())
            .invoke(&delete)
            .unwrap();
        assert!(
            infrastructure::readonly_ref_oid(root, "refs/heads/feature")
                .unwrap()
                .is_none()
        );

        git(root, &["branch", "feature"]);
        fs::write(root.join("stale-source"), b"stale-source").unwrap();
        git(root, &["add", "stale-source"]);
        git(root, &["commit", "-m", "stale-cas"]);
        let stale_oid = oid(root, "HEAD");
        let stale = PlanStep::new(
            StepId::new("stale").unwrap(),
            "stale".into(),
            delete.action().clone(),
            vec![Precondition::RefAt {
                reference: RefName::new("feature").unwrap(),
                oid: stale_oid,
            }],
            vec![],
            None,
            false,
        )
        .unwrap();
        assert!(
            ProductionBackend::new(root.to_owned())
                .invoke(&stale)
                .is_err()
        );
        assert_eq!(oid(root, "refs/heads/feature"), expected);

        git(root, &["symbolic-ref", "refs/heads/sym", "refs/heads/main"]);
        let referent = oid(root, "refs/heads/main");
        let symbolic = PlanStep::new(
            StepId::new("symbolic").unwrap(),
            "symbolic".into(),
            StepAction::DeleteLocalBranch {
                branch: BranchName::new("sym").unwrap(),
            },
            vec![Precondition::RefAt {
                reference: RefName::new("sym").unwrap(),
                oid: referent.clone(),
            }],
            vec![],
            None,
            false,
        )
        .unwrap();
        assert!(
            ProductionBackend::new(root.to_owned())
                .invoke(&symbolic)
                .is_err()
        );
        assert_eq!(oid(root, "refs/heads/main"), referent);

        let linked = root.join("remove-me");
        git(root, &["worktree", "add", linked.to_str().unwrap(), "HEAD"]);
        let remove = PlanStep::new(
            StepId::new("remove").unwrap(),
            "remove".into(),
            StepAction::RemoveWorktree {
                path: stored(linked.clone()),
            },
            vec![],
            vec![],
            None,
            false,
        )
        .unwrap();
        ProductionBackend::new(root.to_owned())
            .invoke(&remove)
            .unwrap();
        assert!(!linked.exists());
    }

    #[test]
    fn d2_create_sources_and_remote_lease_deletion_use_closed_helpers() {
        let temp = repository();
        let root = temp.path();
        let new_path = root.join("new-source");
        let main_oid = oid(root, "refs/heads/main");
        infrastructure::mutate_create_worktree(
            root,
            &new_path,
            &crate::lifecycle::CreateSource::NewBranch {
                branch: BranchName::new("new-source").unwrap(),
                base: Some(RefName::new("main").unwrap()),
            },
            &main_oid,
        )
        .unwrap();
        assert!(infrastructure::readonly_list(&new_path).is_ok());

        git(root, &["branch", "existing"]);
        let existing_path = root.join("existing-source");
        let existing_oid = oid(root, "refs/heads/existing");
        infrastructure::mutate_create_worktree(
            root,
            &existing_path,
            &crate::lifecycle::CreateSource::ExistingLocal {
                branch: BranchName::new("existing").unwrap(),
            },
            &existing_oid,
        )
        .unwrap();
        assert!(infrastructure::readonly_list(&existing_path).is_ok());

        let bare = TempDir::new().unwrap();
        git(bare.path(), &["init", "--bare"]);
        git(
            root,
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        git(root, &["push", "origin", "HEAD:refs/heads/remote-base"]);
        git(root, &["fetch", "origin"]);
        let remote_path = root.join("remote-source");
        let remote_oid = infrastructure::readonly_remote_ref(root, "origin", "remote-base")
            .unwrap()
            .unwrap();
        infrastructure::mutate_create_worktree(
            root,
            &remote_path,
            &crate::lifecycle::CreateSource::RemoteTracking {
                remote: crate::lifecycle::RemoteName::new("origin").unwrap(),
                remote_branch: BranchName::new("remote-base").unwrap(),
                local_branch: BranchName::new("remote-local").unwrap(),
            },
            &remote_oid,
        )
        .unwrap();
        assert!(infrastructure::readonly_list(&remote_path).is_ok());

        fs::write(root.join("remote-change"), b"remote-change").unwrap();
        git(root, &["add", "remote-change"]);
        git(root, &["commit", "-m", "remote-change"]);
        git(root, &["push", "origin", "HEAD:refs/heads/remote-base"]);
        let changed_remote_oid = infrastructure::readonly_remote_ref(root, "origin", "remote-base")
            .unwrap()
            .unwrap();
        assert!(
            infrastructure::mutate_delete_remote_branch(root, "origin", "remote-base", &remote_oid)
                .is_err()
        );
        assert_eq!(
            infrastructure::readonly_remote_ref(root, "origin", "remote-base").unwrap(),
            Some(changed_remote_oid.clone())
        );
        infrastructure::mutate_delete_remote_branch(
            root,
            "origin",
            "remote-base",
            &changed_remote_oid,
        )
        .unwrap();
        assert!(
            infrastructure::readonly_remote_ref(root, "origin", "remote-base")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn probe_covers_postconditions_and_file_artifact_modes_without_mutation() {
        let temp = repository();
        let root = temp.path();
        let branch = BranchName::new("probe-branch").unwrap();
        let destination = root.join("probe-worktree");
        git(
            root,
            &[
                "worktree",
                "add",
                "-b",
                "probe-branch",
                destination.to_str().unwrap(),
            ],
        );
        let head = oid(root, "refs/heads/probe-branch");
        let step = PlanStep::new(
            StepId::new("probe").unwrap(),
            "probe".into(),
            StepAction::CreateWorktree {
                destination: stored(destination.clone()),
                source: crate::lifecycle::CreateSource::NewBranch {
                    branch: branch.clone(),
                    base: None,
                },
            },
            vec![],
            vec![Postcondition::WorktreeCreated {
                path: stored(destination.clone()),
                oid: head.clone(),
            }],
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            ProductionBackend::new(root.to_owned())
                .probe(&step, ProbeContext::StartupReconciliation)
                .unwrap(),
            ProbeVerdict::Applied
        );
        let mismatch = PlanStep::new(
            step.id().clone(),
            "probe-mismatch".into(),
            step.action().clone(),
            vec![],
            vec![Postcondition::WorktreeCreated {
                path: stored(destination.clone()),
                oid: ObjectId::new("0".repeat(40)).unwrap(),
            }],
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            ProductionBackend::new(root.to_owned())
                .probe(&mismatch, ProbeContext::StartupReconciliation)
                .unwrap(),
            ProbeVerdict::NotApplied
        );

        let absent = root.join("removed");
        let removed = PlanStep::new(
            StepId::new("removed").unwrap(),
            "removed".into(),
            StepAction::RemoveWorktree {
                path: stored(absent.clone()),
            },
            vec![],
            vec![Postcondition::WorktreeRemoved {
                path: stored(absent),
                oid: head.clone(),
            }],
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            ProductionBackend::new(root.to_owned())
                .probe(&removed, ProbeContext::StartupReconciliation)
                .unwrap(),
            ProbeVerdict::Applied
        );
        let file = root.join("artifact");
        fs::write(&file, b"artifact").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let digest = crate::planner::artifact_digest(b"artifact");
        let artifact = PlanStep::new(
            StepId::new("artifact").unwrap(),
            "artifact".into(),
            StepAction::FileArtifact {
                rule: "r".into(),
                kind: crate::planner::FileArtifactKind::CopyFile,
                source: stored(file.clone()),
                destination: stored(file.clone()),
                bytes: 8,
                digest: digest.clone(),
                fingerprint: digest.clone(),
                link_target: None,
                manifest_digest: digest.clone(),
                sensitive: false,
                confirm: false,
                mode_policy: crate::planner::FileModePolicy::PreserveSafe,
            },
            vec![],
            vec![],
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            ProductionBackend::new(root.to_owned())
                .probe(
                    &artifact,
                    ProbeContext::AfterAttempt {
                        executor_succeeded: true
                    }
                )
                .unwrap(),
            ProbeVerdict::Applied
        );
        fs::write(&file, b"drift").unwrap();
        assert_eq!(
            ProductionBackend::new(root.to_owned())
                .probe(
                    &artifact,
                    ProbeContext::AfterAttempt {
                        executor_succeeded: false
                    }
                )
                .unwrap(),
            ProbeVerdict::NotApplied
        );
        let run = PlanStep::new(
            StepId::new("run").unwrap(),
            "run".into(),
            StepAction::RunTask {
                name: "x".into(),
                argv: crate::lifecycle::CommandArgv::new(vec!["true".into()]).unwrap(),
                cwd: stored(root),
                required: false,
                environment_allowlist: vec![],
            },
            vec![],
            vec![],
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            ProductionBackend::new(temp.path().to_owned()).probe_capability(&run),
            ProbeCapability::UnknownAfterCrash
        );
        assert_eq!(
            ProductionBackend::new(temp.path().to_owned())
                .probe(&run, ProbeContext::StartupReconciliation)
                .unwrap(),
            ProbeVerdict::Unknown
        );
    }
}
