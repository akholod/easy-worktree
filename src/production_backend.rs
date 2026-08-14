#[cfg(unix)]
use crate::task_runtime::RuntimeInput;
use crate::{
    application::{
        ApplyError, ExecutionOutcomeKind, ExecutionResult, ForwardExecutionPort, PreparedApply,
    },
    domain::{CheckoutStatus, WorktreeClass},
    execution::{
        ConditionResult, ExecutionBackend, ExecutionEngine, ExecutionError, ExecutionOutcome,
        ProbeCapability, ProbeContext, ProbeVerdict,
    },
    infrastructure::{self, GitError},
    lifecycle::{ObjectId, OperationPlan, PlanStep, Postcondition, Precondition, StepAction},
    task_runtime::{CancellationToken, TimingPolicy},
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
    #[error("task execution failed")]
    TaskExecutionFailed,
}

#[derive(Debug, Clone)]
pub struct ProductionRepository {
    pub identity: crate::lifecycle::RepositoryIdentity,
}

#[derive(Clone)]
pub struct ProductionBackend {
    anchor: PathBuf,
    cancellation: CancellationToken,
    timing: TimingPolicy,
}

impl std::fmt::Debug for ProductionBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProductionBackend")
            .field("anchor", &self.anchor)
            .finish_non_exhaustive()
    }
}

struct FileArtifactProbe<'a> {
    kind: crate::planner::FileArtifactKind,
    destination: &'a Path,
    bytes: u64,
    digest: &'a ObjectId,
    fingerprint: &'a ObjectId,
    #[cfg_attr(not(unix), allow(dead_code))]
    mode_policy: crate::planner::FileModePolicy,
    link_target: Option<&'a Path>,
}

impl ProductionBackend {
    pub fn new(anchor: PathBuf) -> Self {
        Self {
            anchor,
            cancellation: CancellationToken::default(),
            timing: TimingPolicy::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_runtime(
        anchor: PathBuf,
        cancellation: CancellationToken,
        timing: TimingPolicy,
    ) -> Self {
        Self {
            anchor,
            cancellation,
            timing,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
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
        phase: crate::execution::ConditionPhase,
        condition: &Precondition,
    ) -> Result<bool, ProductionBackendError> {
        let listing = || self.list();
        Ok(match condition {
            Precondition::CommonDirectory(path) => infrastructure::readonly_same_path(path.as_path(), &self.discover_repository()?.identity.common_dir.into_path()),
            Precondition::ExactlyOnePrimary => listing()?.data.worktrees.iter().filter(|w| w.classification == WorktreeClass::Primary).count() == 1,
            Precondition::BareRepositoryFalse => !listing()?.data.repository.bare,
            Precondition::PathAbsent(path) => {
                let future_artifact_path = step.is_some_and(|step| match step.action() {
                    StepAction::CopyFileV3 {
                        destination,
                        staging,
                        ..
                    } => path == destination || path == &staging.path,
                    StepAction::CreateSymlinkV3 { destination, .. } => path == destination,
                    StepAction::RelinkSymlinkV3 { replacement_staging, backup_staging, .. } => path == &replacement_staging.path || path == &backup_staging.path,
                    _ => false,
                });
                if phase == crate::execution::ConditionPhase::InitialPreflight
                    && future_artifact_path
                {
                    true
                } else {
                    infrastructure::readonly_final_absent(path.as_path())?
                }
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
            Precondition::ArtifactSourceAtV3 { rule, source_root, source, expectation, manifest_digest } => {
                let Some(action) = step.map(|s| s.action()) else { return Ok(false) };
                let matches = match action {
                    StepAction::CopyFileV3 { rule: r, source_root: root, source: s, expected_source, manifest_digest: md, .. } =>
                        r == rule && root == source_root && s == source && matches!(expectation, crate::lifecycle::ArtifactSourceExpectationV3::Regular(want) if expected_source == want) && md == manifest_digest,
                    StepAction::CreateSymlinkV3 { rule: r, source_root: root, source: s, expected_source, manifest_digest: md, .. } =>
                        r == rule && root == source_root && s == source && *expected_source == *expectation && md == manifest_digest,
                    StepAction::RelinkSymlinkV3 { rule: r, source_root: root, source: s, expected_source, manifest_digest: md, .. } =>
                        r == rule && root == source_root && s == source && matches!(expectation, crate::lifecycle::ArtifactSourceExpectationV3::Symlink(want) if expected_source == want) && md == manifest_digest,
                    _ => false,
                };
                if !matches { return Ok(false); }
                let Some(node) = infrastructure::readonly_observe_node(source_root.as_path(), source.as_path())? else { return Ok(false) };
                infrastructure::source_expectation_matches(&node, expectation)
            }
            Precondition::TreeSymlinkAtV3 { commit_oid, checkout_relative_path, expected } => {
                match infrastructure::observe_committed_tree_symlink(&self.anchor, commit_oid, checkout_relative_path.as_path()) {
                    Ok(actual) => actual == *expected,
                    Err(error) if matches!(error.code.as_str(), "tree_missing" | "tree_wrong_kind" | "tree_invalid_target") => false,
                    Err(error) => return Err(ProductionBackendError::Git(GitError::Command(format!("tree observer {}: {}", error.code, error.message)))),
                }
            }
            Precondition::SymlinkAtV3 { path, expected } => {
                if phase == crate::execution::ConditionPhase::InitialPreflight
                    && step.is_some_and(|value| matches!(value.action(), StepAction::RelinkSymlinkV3 { destination, .. } if destination == path))
                {
                    return Ok(true);
                }
                matches!(infrastructure::readonly_observe_absolute_node(path.as_path())?, Some(infrastructure::ObservedNode::Symlink { target }) if target == expected.target.as_path())
            }
        })
    }
}

pub(crate) trait SignalGuard {
    fn exit_override(&self) -> Option<u8>;
}

pub(crate) trait SignalScope {
    type Guard: SignalGuard;

    fn install(&self, token: &CancellationToken) -> Result<Self::Guard, ApplyError>;
}

pub(crate) struct NoopSignalScope;

impl SignalScope for NoopSignalScope {
    type Guard = ();

    fn install(&self, _token: &CancellationToken) -> Result<Self::Guard, ApplyError> {
        Ok(())
    }
}

impl SignalGuard for () {
    fn exit_override(&self) -> Option<u8> {
        None
    }
}

pub(crate) struct ProductionForwardExecution<S = NoopSignalScope> {
    signal_scope: S,
}

impl ProductionForwardExecution<NoopSignalScope> {
    #[allow(dead_code)]
    pub(crate) fn new() -> Self {
        Self {
            signal_scope: NoopSignalScope,
        }
    }
}

impl<S: SignalScope> ProductionForwardExecution<S> {
    pub(crate) fn with_signal_scope(signal_scope: S) -> Self {
        Self { signal_scope }
    }
}

impl<S: SignalScope> ForwardExecutionPort for ProductionForwardExecution<S> {
    fn execute(&self, prepared: PreparedApply) -> Result<ExecutionResult, ApplyError> {
        let backend = ProductionBackend::new(prepared.anchor().as_path().to_owned());
        let token = backend.cancellation_token();
        let signal_guard = self.signal_scope.install(&token)?;
        let mut engine = ExecutionEngine::new(backend);
        let execution = engine.execute(prepared.into_plan());
        let exit_override = signal_guard.exit_override();
        let result = match execution {
            Ok(outcome) => Ok(map_execution_outcome(outcome)),
            Err(error) => Err(map_execution_error(error)),
        };
        drop(signal_guard);
        result
            .map(|mut value| {
                value.exit_override = exit_override;
                value
            })
            .map_err(|mut error| {
                error.exit_override = exit_override;
                error
            })
    }
}

pub(crate) fn map_execution_outcome(outcome: ExecutionOutcome) -> ExecutionResult {
    match outcome {
        ExecutionOutcome::Applied { operation_id } => ExecutionResult {
            operation_id,
            outcome: ExecutionOutcomeKind::Applied,
            step_id: None,
            detail: None,
            exit_override: None,
        },
        ExecutionOutcome::AlreadyApplied { operation_id } => ExecutionResult {
            operation_id,
            outcome: ExecutionOutcomeKind::AlreadyApplied,
            step_id: None,
            detail: None,
            exit_override: None,
        },
        ExecutionOutcome::PreflightRefused { operation_id, .. } => ExecutionResult {
            operation_id,
            outcome: ExecutionOutcomeKind::PreflightRefused,
            step_id: None,
            detail: Some("precondition refused".into()),
            exit_override: None,
        },
        ExecutionOutcome::Paused {
            operation_id,
            step_id,
            ..
        } => ExecutionResult {
            operation_id,
            outcome: ExecutionOutcomeKind::Paused,
            step_id: Some(step_id),
            detail: Some("execution paused".into()),
            exit_override: None,
        },
        ExecutionOutcome::NeedsAttention {
            operation_id,
            step_id,
        } => ExecutionResult {
            operation_id,
            outcome: ExecutionOutcomeKind::NeedsAttention,
            step_id: Some(step_id),
            detail: Some("execution needs attention".into()),
            exit_override: None,
        },
        ExecutionOutcome::ExistingOperation {
            operation_id,
            status,
        } => ExecutionResult {
            operation_id,
            outcome: ExecutionOutcomeKind::ExistingOperation,
            step_id: None,
            detail: Some(status.as_str().into()),
            exit_override: None,
        },
    }
}

pub(crate) fn map_execution_error<E>(error: ExecutionError<E>) -> ApplyError {
    match error {
        ExecutionError::Backend(_) => ApplyError {
            code: "backend_error",
            message: "execution backend failed",
            exit_override: None,
        },
        ExecutionError::Journal(error) => match error {
            crate::journal_store::JournalError::RepositoryBusy => ApplyError {
                code: "repository_busy",
                message: "repository is busy",
                exit_override: None,
            },
            crate::journal_store::JournalError::Corrupt(_)
            | crate::journal_store::JournalError::InvalidId
            | crate::journal_store::JournalError::RevisionConflict
            | crate::journal_store::JournalError::ImmutableMismatch
            | crate::journal_store::JournalError::InvalidTransition => ApplyError {
                code: "journal_corrupt",
                message: "journal is corrupt",
                exit_override: None,
            },
            crate::journal_store::JournalError::NotFound => ApplyError {
                code: "journal_io",
                message: "journal I/O failed",
                exit_override: None,
            },
            crate::journal_store::JournalError::Io(_) => ApplyError {
                code: "journal_io",
                message: "journal I/O failed",
                exit_override: None,
            },
        },
        ExecutionError::UnsupportedPlan(_) => ApplyError {
            code: "unsupported_plan",
            message: "execution plan is unsupported",
            exit_override: None,
        },
        ExecutionError::MissingConsent(_) => ApplyError {
            code: "missing_consent",
            message: "required execution consent is missing",
            exit_override: None,
        },
        ExecutionError::RepositoryIdentityMismatch => ApplyError {
            code: "repository_identity_mismatch",
            message: "repository identity does not match plan",
            exit_override: None,
        },
        ExecutionError::ImmutableCollision => ApplyError {
            code: "immutable_collision",
            message: "operation identity collides with another plan",
            exit_override: None,
        },
    }
}

fn relink_step_shape(step: &PlanStep) -> bool {
    let StepAction::RelinkSymlinkV3 {
        destination,
        replacement_staging,
        backup_staging,
        ..
    } = step.action()
    else {
        return false;
    };
    let has_source = step
        .preconditions()
        .iter()
        .filter(|p| matches!(p, Precondition::ArtifactSourceAtV3 { .. }))
        .count()
        == 1;
    let has_tree = step
        .preconditions()
        .iter()
        .filter(|p| matches!(p, Precondition::TreeSymlinkAtV3 { .. }))
        .count()
        == 1;
    let has_destination = step
        .preconditions()
        .iter()
        .filter(|p| matches!(p, Precondition::SymlinkAtV3 { path, .. } if path == destination))
        .count()
        == 1;
    let has_replacement = step
        .preconditions()
        .iter()
        .filter(
            |p| matches!(p, Precondition::PathAbsent(path) if path == &replacement_staging.path),
        )
        .count()
        == 1;
    let has_backup = step
        .preconditions()
        .iter()
        .filter(|p| matches!(p, Precondition::PathAbsent(path) if path == &backup_staging.path))
        .count()
        == 1;
    has_source
        && has_tree
        && has_destination
        && has_replacement
        && has_backup
        && step.preconditions().len() == 5
}

#[cfg(unix)]
fn supported_task(context: &crate::execution::StepExecutionContext<'_>) -> bool {
    let StepAction::RunTask {
        name,
        argv,
        cwd,
        required,
        environment_allowlist,
    } = context.step().action()
    else {
        return false;
    };
    let crate::lifecycle::OperationIntent::Create(intent) = context.plan().intent() else {
        return false;
    };
    let Some(destination) = intent.destination.as_ref() else {
        return false;
    };
    let Some(contract) = intent.task_contracts.get(name) else {
        return false;
    };
    let normalized = |path: &Path| {
        path.is_absolute()
            && path.components().all(|component| {
                matches!(
                    component,
                    std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                        | std::path::Component::Normal(_)
                )
            })
    };
    let contained = normalized(destination.as_path())
        && normalized(cwd.as_path())
        && cwd.as_path().strip_prefix(destination.as_path()).is_ok();
    let env_unique = environment_allowlist
        .iter()
        .map(|value| value.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        == environment_allowlist.len()
        && environment_allowlist.iter().all(|value| {
            !value.as_str().is_empty()
                && !value.as_str().contains('\0')
                && !value.as_str().contains('=')
        });
    let consent_id = format!("task:{name}");
    let consent = context
        .plan()
        .required_consents()
        .iter()
        .find(|value| value.id.as_str() == consent_id);
    contract.argv == *argv
        && contract.cwd == *cwd
        && contract.required == *required
        && contract.environment_allowlist == *environment_allowlist
        && argv
            .as_slice()
            .iter()
            .all(|value| !value.is_empty() && !value.contains('\0'))
        && env_unique
        && contained
        && context.step().irreversible()
        && context.step().compensation().is_none()
        && context.step().postconditions().is_empty()
        && consent.is_some_and(|value| {
            value.risks.len() == 1
                && value.risks[0].kind == crate::lifecycle::RiskKind::ExecuteTask
                && context
                    .plan()
                    .risks()
                    .iter()
                    .any(|risk| risk.kind == crate::lifecycle::RiskKind::ExecuteTask)
        })
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
        _phase: crate::execution::ConditionPhase,
        precondition: &Precondition,
    ) -> bool {
        if matches!(precondition, Precondition::SourceManifest { .. }) {
            return false;
        }
        if matches!(
            precondition,
            Precondition::TreeSymlinkAtV3 { .. } | Precondition::SymlinkAtV3 { .. }
        ) {
            return step
                .is_some_and(|value| matches!(value.action(), StepAction::RelinkSymlinkV3 { .. }));
        }
        if let Precondition::ArtifactSourceAtV3 {
            rule,
            source_root,
            source,
            expectation,
            manifest_digest,
        } = precondition
        {
            return match step.map(|s| s.action()) {
                Some(StepAction::CopyFileV3 {
                    rule: r,
                    source_root: root,
                    source: s,
                    expected_source,
                    manifest_digest: md,
                    ..
                }) => {
                    r == rule
                        && root == source_root
                        && s == source
                        && matches!(expectation, crate::lifecycle::ArtifactSourceExpectationV3::Regular(want) if expected_source == want)
                        && md == manifest_digest
                }
                Some(StepAction::CreateSymlinkV3 {
                    rule: r,
                    source_root: root,
                    source: s,
                    expected_source,
                    manifest_digest: md,
                    ..
                }) => {
                    r == rule
                        && root == source_root
                        && s == source
                        && expected_source == expectation
                        && md == manifest_digest
                }
                Some(StepAction::RelinkSymlinkV3 {
                    rule: r,
                    source_root: root,
                    source: s,
                    expected_source,
                    manifest_digest: md,
                    ..
                }) => {
                    r == rule
                        && root == source_root
                        && s == source
                        && matches!(expectation, crate::lifecycle::ArtifactSourceExpectationV3::Symlink(want) if expected_source == want)
                        && md == manifest_digest
                }
                _ => false,
            };
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
    fn supports_action(&self, context: &crate::execution::StepExecutionContext<'_>) -> bool {
        match context.step().action() {
            StepAction::CreateWorktree { .. }
            | StepAction::DeleteLocalBranch { .. }
            | StepAction::DeleteRemoteBranch {
                expected_oid: Some(_),
                ..
            } => true,
            StepAction::CopyFileV3 { publication: crate::lifecycle::PublicationStrategyV3::AtomicNoReplaceV1, .. }
            | StepAction::CreateSymlinkV3 { expected_source: crate::lifecycle::ArtifactSourceExpectationV3::Regular(_) | crate::lifecycle::ArtifactSourceExpectationV3::Directory, .. } => true,
            StepAction::RelinkSymlinkV3 { .. } => relink_step_shape(context.step()),
            StepAction::RemoveWorktree { path } => context.step().preconditions().iter().any(|condition| {
                matches!(condition, Precondition::WorktreeClean { path: guarded } if *guarded == *path)
            }),
            #[cfg(unix)]
            StepAction::RunTask { .. } => supported_task(context),
            _ => false,
        }
    }
    fn probe_capability(
        &self,
        context: &crate::execution::StepExecutionContext<'_>,
    ) -> ProbeCapability {
        if matches!(context.step().action(), StepAction::RelinkSymlinkV3 { .. }) {
            return if relink_step_shape(context.step()) {
                ProbeCapability::Deterministic
            } else {
                ProbeCapability::Unsupported
            };
        }
        if matches!(
            context.step().action(),
            StepAction::CopyFileV3 {
                publication: crate::lifecycle::PublicationStrategyV3::AtomicNoReplaceV1,
                ..
            } | StepAction::CreateSymlinkV3 {
                expected_source: crate::lifecycle::ArtifactSourceExpectationV3::Regular(_)
                    | crate::lifecycle::ArtifactSourceExpectationV3::Directory,
                ..
            }
        ) {
            return ProbeCapability::Deterministic;
        }
        if matches!(context.step().action(), StepAction::RunTask { .. }) {
            ProbeCapability::UnknownAfterCrash
        } else {
            ProbeCapability::Deterministic
        }
    }
    fn check_precondition(
        &mut self,
        plan: &OperationPlan,
        step: Option<&PlanStep>,
        phase: crate::execution::ConditionPhase,
        precondition: &Precondition,
    ) -> Result<crate::execution::ConditionResult, Self::Error> {
        if !self.supports_precondition(plan, step, phase, precondition) {
            return Err(ProductionBackendError::UnsupportedObservation(
                "artifact guard without FileArtifact step",
            ));
        }
        self.condition(plan, step, phase, precondition).map(|v| {
            if v {
                ConditionResult::Satisfied
            } else {
                ConditionResult::Unsatisfied
            }
        })
    }
    fn invoke(
        &mut self,
        context: &crate::execution::StepExecutionContext<'_>,
    ) -> Result<(), Self::Error> {
        let step = context.step();
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
            StepAction::CopyFileV3 {
                source_root,
                source,
                expected_source,
                destination,
                desired_output,
                staging,
                ..
            } => {
                infrastructure::mutate_copy_file_v3(
                    source_root.as_path(),
                    source.as_path(),
                    expected_source,
                    destination.as_path(),
                    desired_output,
                    staging,
                )?;
                Ok(())
            }
            StepAction::CreateSymlinkV3 {
                source_root,
                source,
                expected_source,
                destination,
                desired,
                ..
            } => {
                infrastructure::mutate_create_symlink_v3(
                    source_root.as_path(),
                    source.as_path(),
                    expected_source,
                    destination.as_path(),
                    desired,
                )?;
                Ok(())
            }
            StepAction::RelinkSymlinkV3 {
                source_root,
                source,
                expected_source,
                destination,
                expected_old,
                desired_new,
                replacement_staging,
                backup_staging,
                ..
            } => {
                let expected = expected_source;
                if !relink_step_shape(step) {
                    return Err(ProductionBackendError::UnsupportedObservation(
                        "relink guard shape",
                    ));
                }
                infrastructure::mutate_relink_symlink_v3(&infrastructure::RelinkMutationSpec {
                    source_root: source_root.as_path(),
                    source: source.as_path(),
                    expected_source: expected,
                    destination: destination.as_path(),
                    expected_old,
                    desired: desired_new,
                    replacement: replacement_staging,
                    backup: backup_staging,
                })?;
                Ok(())
            }
            #[cfg(unix)]
            StepAction::RunTask {
                argv,
                cwd,
                environment_allowlist,
                ..
            } => {
                if !supported_task(context) {
                    return Err(ProductionBackendError::MutationUnavailable);
                }
                let repository = self.discover_repository()?;
                if !self.repository_matches_plan(&repository, context.plan()) {
                    return Err(ProductionBackendError::MutationUnavailable);
                }
                let argv = argv.as_slice().to_vec();
                let step_id = context.step().id().as_str();
                crate::task_runtime::run_task(&RuntimeInput {
                    common_dir: self.repository_common_dir(&repository),
                    operation_id: context.operation_id().as_uuid(),
                    step_id,
                    argv: &argv,
                    cwd: cwd.as_path(),
                    environment_allowlist: &environment_allowlist
                        .iter()
                        .map(|value| value.as_str().to_owned())
                        .collect::<Vec<_>>(),
                    token: self.cancellation.clone(),
                    timing: self.timing,
                })
                .map(|_| ())
                .map_err(|_| ProductionBackendError::TaskExecutionFailed)
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
        context: &crate::execution::StepExecutionContext<'_>,
        probe_context: ProbeContext,
    ) -> Result<ProbeVerdict, Self::Error> {
        let step = context.step();
        if let StepAction::RelinkSymlinkV3 {
            destination,
            expected_old,
            desired_new,
            replacement_staging,
            backup_staging,
            ..
        } = step.action()
        {
            return Ok(
                match infrastructure::probe_relink_symlink_v3(
                    destination.as_path(),
                    replacement_staging.path.as_path(),
                    backup_staging.path.as_path(),
                    expected_old,
                    desired_new,
                ) {
                    Err(_) => ProbeVerdict::Unknown,
                    Ok(infrastructure::RelinkProbeState::Applied) => ProbeVerdict::Applied,
                    Ok(infrastructure::RelinkProbeState::NotApplied) => ProbeVerdict::NotApplied,
                    Ok(infrastructure::RelinkProbeState::Unknown) => ProbeVerdict::Unknown,
                },
            );
        }
        if matches!(step.action(), StepAction::RunTask { .. }) {
            return Ok(match probe_context {
                ProbeContext::AfterAttempt {
                    executor_succeeded: true,
                } => ProbeVerdict::Applied,
                ProbeContext::AfterAttempt {
                    executor_succeeded: false,
                }
                | ProbeContext::StartupReconciliation => ProbeVerdict::Unknown,
            });
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
        if let StepAction::CopyFileV3 {
            destination,
            desired_output,
            staging,
            ..
        } = step.action()
        {
            let stage_absent = match infrastructure::readonly_final_absent(staging.path.as_path()) {
                Ok(absent) => absent,
                Err(_) => return Ok(ProbeVerdict::Unknown),
            };
            if !stage_absent {
                return Ok(ProbeVerdict::Unknown);
            }
            let final_absent = match infrastructure::readonly_final_absent(destination.as_path()) {
                Ok(absent) => absent,
                Err(_) => return Ok(ProbeVerdict::Unknown),
            };
            if final_absent {
                return Ok(ProbeVerdict::NotApplied);
            }
            let final_state =
                match infrastructure::readonly_observe_absolute_node(destination.as_path()) {
                    Ok(state) => state,
                    Err(_) => return Ok(ProbeVerdict::Unknown),
                };
            return match final_state {
                Some(infrastructure::ObservedNode::Regular { bytes, mode })
                    if bytes.len() as u64 == desired_output.bytes
                        && crate::planner::artifact_digest(&bytes) == desired_output.digest
                        && mode == desired_output.mode =>
                {
                    Ok(ProbeVerdict::Applied)
                }
                None => Ok(ProbeVerdict::Unknown),
                _ => Ok(ProbeVerdict::Unknown),
            };
        }
        if let StepAction::CreateSymlinkV3 {
            destination,
            desired,
            ..
        } = step.action()
        {
            let final_absent = match infrastructure::readonly_final_absent(destination.as_path()) {
                Ok(absent) => absent,
                Err(_) => return Ok(ProbeVerdict::Unknown),
            };
            if final_absent {
                return Ok(ProbeVerdict::NotApplied);
            }
            return match infrastructure::readonly_observe_absolute_node(destination.as_path()) {
                Ok(Some(infrastructure::ObservedNode::Symlink { target }))
                    if target == desired.target.as_path()
                        && crate::planner::artifact_digest(
                            target.as_os_str().as_encoded_bytes(),
                        ) == desired.target_digest =>
                {
                    Ok(ProbeVerdict::Applied)
                }
                Ok(None) => Ok(ProbeVerdict::Unknown),
                _ => Ok(ProbeVerdict::Unknown),
            };
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
                #[cfg(not(unix))]
                let _ = mode;
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
    use sha2::Digest;
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

    fn context_plan(
        base: &crate::lifecycle::OperationPlan,
        step: PlanStep,
    ) -> crate::lifecycle::OperationPlan {
        crate::lifecycle::OperationPlan::new(crate::lifecycle::OperationPlanDraft {
            operation_id: *base.operation_id(),
            kind: base.kind(),
            repository: base.repository().clone(),
            intent: base.intent().clone(),
            preconditions: step.preconditions().to_vec(),
            steps: vec![step],
            risks: base.risks().to_vec(),
            required_consents: base.required_consents().to_vec(),
            granted_consents: base.granted_consents().clone(),
        })
        .unwrap()
    }

    #[cfg(unix)]
    fn generated_task_plan(
        required: bool,
        commands: Vec<(String, String)>,
    ) -> (TempDir, crate::lifecycle::OperationPlan, PathBuf) {
        let temp = repository();
        let root = temp.path();
        let mut backend = ProductionBackend::new(root.to_owned());
        let identity = backend.discover_repository().unwrap().identity;
        let destination = root
            .parent()
            .unwrap()
            .join(format!("ewtm-b2a-{}", uuid::Uuid::new_v4()));
        let mut selected_tasks = BTreeSet::new();
        let mut granted_consents = BTreeSet::new();
        let mut tasks = Vec::new();
        for (name, body) in commands {
            selected_tasks.insert(name.clone());
            granted_consents
                .insert(crate::lifecycle::ConsentId::new(format!("task:{name}")).unwrap());
            let argv = if body == "__spawn_failure__" {
                vec!["/no/such/ewtm-command".into()]
            } else {
                vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    body,
                    "ewtm-task".into(),
                    "marker".into(),
                ]
            };
            tasks.push(crate::planner::TaskSpec {
                name,
                argv: crate::lifecycle::CommandArgv::new(argv).unwrap(),
                cwd: stored(&destination),
                enabled: true,
                post_create: true,
                required,
                environment_allowlist: vec![],
            });
        }
        let branch = crate::lifecycle::BranchName::new("b2a-task").unwrap();
        let intent = crate::lifecycle::CreateIntent {
            repository: identity.clone(),
            source: crate::lifecycle::CreateSource::NewBranch {
                branch: branch.clone(),
                base: None,
            },
            destination: Some(stored(&destination)),
            selected_tasks,
            skipped_rules: BTreeSet::new(),
            granted_consents,
            task_contracts: BTreeMap::new(),
            current_worktree_root: None,
            artifact_rule_contracts: BTreeMap::new(),
        };
        let input = crate::planner::CreatePlanInput {
            operation_id: crate::planner::new_operation_id(),
            repository: identity.clone(),
            intent,
            bare: false,
            primary_count: 1,
            invocation_cwd: stored(root),
            primary_root: identity.primary_root.clone(),
            current_worktree_root: identity.primary_root.clone(),
            destination: crate::planner::DestinationFacts {
                path: stored(&destination),
                state: crate::planner::DestinationState::Absent,
                parent: stored(destination.parent().unwrap()),
                parent_safe: true,
            },
            source_facts: crate::planner::CreateSourceFacts::NewBranch {
                branch,
                base_ref: crate::lifecycle::RefName::new("HEAD").unwrap(),
                base_oid: oid(root, "HEAD"),
                branch_absent: true,
            },
            branch_checked_out: false,
            branch_collision: false,
            known_rules: BTreeSet::new(),
            enabled_rules: BTreeSet::new(),
            known_tasks: tasks.iter().map(|task| task.name.clone()).collect(),
            manifests: vec![],
            tasks,
        };
        (
            temp,
            crate::planner::plan_create(input).unwrap(),
            destination,
        )
    }

    #[cfg(unix)]
    fn task_step_indices(plan: &crate::lifecycle::OperationPlan) -> Vec<usize> {
        plan.steps()
            .iter()
            .enumerate()
            .filter_map(|(index, step)| {
                matches!(step.action(), StepAction::RunTask { .. }).then_some(index)
            })
            .collect()
    }

    #[cfg(unix)]
    fn task_logs_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn collect(root: &Path, path: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        collect(root, &path, out);
                    } else {
                        out.push((
                            path.strip_prefix(root).unwrap().to_owned(),
                            fs::read(path).unwrap(),
                        ));
                    }
                }
            }
        }
        let mut result = Vec::new();
        collect(root, root, &mut result);
        result.sort_by(|left, right| left.0.cmp(&right.0));
        result
    }

    #[cfg(unix)]
    #[test]
    fn task_started_write_fault_has_no_task_effect_and_exact_order() {
        let _faults = crate::journal_store::test_fault_guard();
        let (temp, plan, destination) = generated_task_plan(
            true,
            vec![
                ("first".into(), "printf never > \"$1\"".into()),
                ("later".into(), "printf later > later".into()),
            ],
        );
        let task_indices = task_step_indices(&plan);
        assert_eq!(task_indices, vec![1, 2]);
        crate::journal_store::inject_fail_on_atomic_write(4);
        assert!(matches!(
            ExecutionEngine::new(ProductionBackend::new(temp.path().to_owned()))
                .execute(plan.clone()),
            Err(crate::execution::ExecutionError::Journal(_))
        ));
        let journal = journal_for(temp.path(), &plan);
        assert_eq!(
            journal.revision(),
            2,
            "writes 1..2 persisted; write 3 was task Started"
        );
        assert_eq!(journal.status(), crate::journal::OperationStatus::Pending);
        assert_eq!(
            journal.steps()[0].status(),
            crate::journal::StepStatus::Applied
        );
        assert_eq!(
            journal.steps()[1].status(),
            crate::journal::StepStatus::Pending
        );
        assert!(destination.exists());
        assert!(!destination.join("marker").exists());
        assert!(task_logs_snapshot(&temp.path().join(".git/ewtm/task-logs")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn task_applied_write_fault_restarts_attention_without_reinvocation() {
        let _faults = crate::journal_store::test_fault_guard();
        let (temp, plan, destination) = generated_task_plan(
            true,
            vec![(
                "first".into(),
                "printf once > \"$1\"; printf task-output".into(),
            )],
        );
        crate::journal_store::inject_fail_on_atomic_write(5);
        assert!(matches!(
            ExecutionEngine::new(ProductionBackend::new(temp.path().to_owned()))
                .execute(plan.clone()),
            Err(crate::execution::ExecutionError::Journal(_))
        ));
        assert_eq!(fs::read(destination.join("marker")).unwrap(), b"once");
        let logs = task_logs_snapshot(&temp.path().join(".git/ewtm/task-logs"));
        let journal = journal_for(temp.path(), &plan);
        assert_eq!(journal.revision(), 3);
        assert_eq!(journal.status(), crate::journal::OperationStatus::Running);
        assert_eq!(
            journal.steps()[1].status(),
            crate::journal::StepStatus::Started
        );
        let restarted = ExecutionEngine::new(ProductionBackend::new(temp.path().to_owned()))
            .execute(plan.clone())
            .unwrap();
        assert!(matches!(
            restarted,
            ExecutionOutcome::ExistingOperation {
                status: crate::journal::OperationStatus::NeedsAttention,
                ..
            }
        ));
        assert_eq!(
            journal_for(temp.path(), &plan).status(),
            crate::journal::OperationStatus::NeedsAttention
        );
        assert_eq!(
            task_logs_snapshot(&temp.path().join(".git/ewtm/task-logs")),
            logs
        );
        assert_eq!(fs::read(destination.join("marker")).unwrap(), b"once");
    }

    #[cfg(unix)]
    #[test]
    fn failed_task_applied_write_fault_restarts_unknown_without_mutation() {
        let _faults = crate::journal_store::test_fault_guard();
        let (temp, plan, destination) = generated_task_plan(
            false,
            vec![
                (
                    "first".into(),
                    "printf failed > \"$1\"; printf noisy; printf error >&2; exit 9".into(),
                ),
                ("later".into(), "printf later > later".into()),
            ],
        );
        crate::journal_store::inject_fail_on_atomic_write(5);
        assert!(matches!(
            ExecutionEngine::new(ProductionBackend::new(temp.path().to_owned()))
                .execute(plan.clone()),
            Err(crate::execution::ExecutionError::Journal(_))
        ));
        let logs = task_logs_snapshot(&temp.path().join(".git/ewtm/task-logs"));
        assert!(destination.join("marker").exists());
        assert!(!destination.join("later").exists());
        assert_eq!(
            journal_for(temp.path(), &plan).steps()[1].status(),
            crate::journal::StepStatus::Started
        );
        let restarted = ExecutionEngine::new(ProductionBackend::new(temp.path().to_owned()))
            .execute(plan.clone())
            .unwrap();
        assert!(matches!(
            restarted,
            ExecutionOutcome::ExistingOperation {
                status: crate::journal::OperationStatus::NeedsAttention,
                ..
            }
        ));
        let journal = journal_for(temp.path(), &plan);
        assert_eq!(
            journal.status(),
            crate::journal::OperationStatus::NeedsAttention
        );
        assert_eq!(
            journal.steps()[1].status(),
            crate::journal::StepStatus::NeedsAttention
        );
        assert_eq!(
            task_logs_snapshot(&temp.path().join(".git/ewtm/task-logs")),
            logs
        );
    }

    #[cfg(unix)]
    #[test]
    fn successful_task_restart_is_already_applied_and_private_journal_is_redacted() {
        let (temp, plan, destination) = generated_task_plan(
            true,
            vec![(
                "first".into(),
                "printf durable > \"$1\"; printf '%s' \"$$\"; printf '%s' \"$PPID\" >&2".into(),
            )],
        );
        let operation = *plan.operation_id();
        let mut engine = ExecutionEngine::new(ProductionBackend::new(temp.path().to_owned()));
        assert!(matches!(
            engine.execute(plan.clone()).unwrap(),
            ExecutionOutcome::Applied { .. }
        ));
        let before = task_logs_snapshot(&temp.path().join(".git/ewtm/task-logs"));
        let journal_path = temp
            .path()
            .join(".git/ewtm/journal")
            .join(format!("{operation}.json"));
        let wire = fs::read(&journal_path).unwrap();
        let text = String::from_utf8_lossy(&wire);
        assert!(
            !text.contains("task-logs")
                && !text.contains(
                    temp.path()
                        .join(".git/ewtm/task-logs")
                        .to_string_lossy()
                        .as_ref()
                )
        );
        assert!(matches!(
            ExecutionEngine::new(ProductionBackend::new(temp.path().to_owned()))
                .execute(plan)
                .unwrap(),
            ExecutionOutcome::AlreadyApplied { .. }
        ));
        assert_eq!(
            task_logs_snapshot(&temp.path().join(".git/ewtm/task-logs")),
            before
        );
        assert_eq!(fs::read(destination.join("marker")).unwrap(), b"durable");
    }

    #[cfg(unix)]
    #[test]
    fn generated_tasks_run_in_order_and_finish_applied() {
        let (temp, plan, destination) = generated_task_plan(
            false,
            vec![
                (
                    "first".into(),
                    "printf first > \"$1\"; printf first-output".into(),
                ),
                (
                    "second".into(),
                    "printf second > \"second\"; printf second-output".into(),
                ),
            ],
        );
        let operation = *plan.operation_id();
        let mut backend = ProductionBackend::new(temp.path().to_owned());
        let task = plan
            .steps()
            .iter()
            .find(|step| matches!(step.action(), StepAction::RunTask { .. }))
            .unwrap();
        assert!(backend.supports_action(&crate::execution::StepExecutionContext::new(&plan, task)));
        let context = crate::execution::StepExecutionContext::new(&plan, task);
        assert_eq!(
            backend
                .probe(
                    &context,
                    ProbeContext::AfterAttempt {
                        executor_succeeded: true
                    }
                )
                .unwrap(),
            ProbeVerdict::Applied
        );
        assert_eq!(
            backend
                .probe(
                    &context,
                    ProbeContext::AfterAttempt {
                        executor_succeeded: false
                    }
                )
                .unwrap(),
            ProbeVerdict::Unknown
        );
        assert_eq!(
            backend
                .probe(&context, ProbeContext::StartupReconciliation)
                .unwrap(),
            ProbeVerdict::Unknown
        );
        let outcome = ExecutionEngine::new(ProductionBackend::new(temp.path().to_owned()))
            .execute(plan.clone())
            .unwrap();
        assert!(matches!(outcome, ExecutionOutcome::Applied { .. }));
        assert_eq!(
            fs::read_to_string(destination.join("marker")).unwrap(),
            "first"
        );
        assert_eq!(
            fs::read_to_string(destination.join("second")).unwrap(),
            "second"
        );
        let journal = crate::journal_store::JournalStore::new(&temp.path().join(".git"))
            .read(&operation)
            .unwrap();
        assert_eq!(journal.status(), crate::journal::OperationStatus::Applied);
        for step in plan
            .steps()
            .iter()
            .filter(|step| matches!(step.action(), StepAction::RunTask { .. }))
        {
            let mut hash = sha2::Sha256::new();
            hash.update(b"ewtm-task-log-v1\0");
            hash.update(operation.as_uuid().as_bytes());
            hash.update([0]);
            hash.update(step.id().as_str().as_bytes());
            let log_dir = temp
                .path()
                .join(".git/ewtm/task-logs/v1")
                .join(operation.to_string())
                .join(format!("{:x}", hash.finalize()));
            assert!(log_dir.join("stdout.log").is_file());
            assert!(log_dir.join("stderr.log").is_file());
            assert!(log_dir.join("result.json").is_file());
            assert!(!log_dir.to_string_lossy().contains("first"));
            assert!(!log_dir.to_string_lossy().contains("second"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn generated_task_failures_pause_without_running_later_tasks() {
        for (required, command, expected) in [
            (true, "exit 7", "nonzero"),
            (false, "exit 8", "nonzero"),
            (true, "__spawn_failure__", "spawn_failed"),
            (false, "kill -TERM $$", "signaled"),
        ] {
            let (temp, plan, destination) = generated_task_plan(
                required,
                vec![
                    ("first".into(), command.into()),
                    ("later".into(), "printf later > \"later\"".into()),
                ],
            );
            let outcome = ExecutionEngine::new(ProductionBackend::new(temp.path().to_owned()))
                .execute(plan)
                .unwrap();
            assert!(matches!(outcome, ExecutionOutcome::NeedsAttention { .. }));
            assert!(!destination.join("later").exists());
            let result = fs::read_dir(temp.path().join(".git/ewtm/task-logs/v1"))
                .unwrap()
                .flat_map(|operation| fs::read_dir(operation.unwrap().path()).unwrap())
                .flat_map(|step| fs::read_dir(step.unwrap().path()).unwrap())
                .find(|entry| entry.as_ref().unwrap().path().ends_with("result.json"))
                .unwrap()
                .unwrap()
                .path();
            let metadata: serde_json::Value =
                serde_json::from_slice(&fs::read(result).unwrap()).unwrap();
            assert_eq!(metadata["outcome"], expected);
            assert!(!metadata.to_string().contains("ewtm-command"));
            assert!(!metadata.to_string().contains("later"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn generated_task_cancellation_contains_child_and_finishes_attention() {
        use std::{
            thread,
            time::{Duration, Instant},
        };
        let (temp, plan, destination) = generated_task_plan(
            false,
            vec![
                (
                    "first".into(),
                    "printf '%s' \"$$\" > \"$1\"; while :; do :; done".into(),
                ),
                ("later".into(), "printf later > \"later\"".into()),
            ],
        );
        let backend = ProductionBackend::with_runtime(
            temp.path().to_owned(),
            CancellationToken::default(),
            crate::task_runtime::TimingPolicy {
                poll: Duration::from_millis(2),
                term_grace: Duration::from_millis(40),
                drain_grace: Duration::from_millis(40),
            },
        );
        let token = backend.cancellation_token();
        let marker = destination.join("marker");
        let worker = thread::spawn(move || ExecutionEngine::new(backend).execute(plan));
        let deadline = Instant::now() + Duration::from_secs(10);
        let pids = loop {
            if let Ok(contents) = fs::read_to_string(&marker) {
                let parsed: Vec<u32> = contents
                    .split_whitespace()
                    .filter_map(|value| value.parse().ok())
                    .collect();
                if parsed.len() == 1 {
                    break parsed;
                }
            }
            assert!(
                Instant::now() < deadline,
                "task readiness marker was not written"
            );
            thread::sleep(Duration::from_millis(2));
        };
        token.cancel();
        let outcome = worker.join().unwrap().unwrap();
        assert!(matches!(outcome, ExecutionOutcome::NeedsAttention { .. }));
        let deadline = Instant::now() + Duration::from_secs(10);
        while pids.iter().any(|pid| {
            Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .status()
                .is_ok_and(|status| status.success())
        }) {
            assert!(Instant::now() < deadline, "cancelled task child survived");
            thread::sleep(Duration::from_millis(2));
        }
        assert!(!destination.join("later").exists());
        let result = fs::read_dir(temp.path().join(".git/ewtm/task-logs/v1"))
            .unwrap()
            .flat_map(|operation| fs::read_dir(operation.unwrap().path()).unwrap())
            .flat_map(|step| fs::read_dir(step.unwrap().path()).unwrap())
            .find(|entry| entry.as_ref().unwrap().path().ends_with("result.json"))
            .unwrap()
            .unwrap()
            .path();
        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(result).unwrap()).unwrap();
        assert_eq!(metadata["outcome"], "cancelled");
        assert_eq!(metadata["cancellation_phase"], "during_run");
    }

    #[cfg(unix)]
    #[test]
    fn generated_task_support_boundary_rejects_contract_and_shape_mutations_before_state() {
        let mutations: Vec<fn(&mut PlanStep, &Path)> = vec![
            |step, _| {
                if let StepAction::RunTask { name, .. } = step.action_mut() {
                    *name = "wrong-name".into();
                }
            },
            |step, _| {
                if let StepAction::RunTask { argv, .. } = step.action_mut() {
                    *argv = crate::lifecycle::CommandArgv::new(vec!["/bin/false".into()]).unwrap();
                }
            },
            |step, _| {
                if let StepAction::RunTask { cwd, .. } = step.action_mut() {
                    *cwd = stored("/tmp/outside-ewtm-task");
                }
            },
            |step, _| {
                if let StepAction::RunTask { required, .. } = step.action_mut() {
                    *required = !*required;
                }
            },
            |step, _| {
                if let StepAction::RunTask {
                    environment_allowlist,
                    ..
                } = step.action_mut()
                {
                    environment_allowlist
                        .push(crate::lifecycle::EnvironmentName::new("HOME").unwrap());
                }
            },
            |step, _| {
                *step = PlanStep::new(
                    step.id().clone(),
                    step.name().to_owned(),
                    step.action().clone(),
                    step.preconditions().to_vec(),
                    step.postconditions().to_vec(),
                    Some(crate::lifecycle::Compensation::RemoveCreatedArtifact(
                        crate::lifecycle::CreatedArtifact {
                            path: stored("/tmp/compensation"),
                            fingerprint: ObjectId::new("0000000000000000000000000000000000000000")
                                .unwrap(),
                        },
                    )),
                    false,
                )
                .unwrap();
            },
            |step, _| {
                *step.action_mut() = StepAction::RemoveWorktree {
                    path: stored("/tmp/unsupported-action"),
                };
            },
        ];
        for mutate in mutations {
            let (temp, mut plan, destination) = generated_task_plan(
                false,
                vec![("first".into(), "printf never > \"$1\"".into())],
            );
            let task = plan
                .steps_mut()
                .iter_mut()
                .find(|step| matches!(step.action(), StepAction::RunTask { .. }))
                .unwrap();
            mutate(task, &destination);
            assert!(plan.validate_executable_plan().is_err());
            assert!(matches!(
                ExecutionEngine::new(ProductionBackend::new(temp.path().to_owned())).execute(plan),
                Err(crate::execution::ExecutionError::UnsupportedPlan(_))
            ));
            assert!(!temp.path().join(".git/ewtm").exists());
            assert!(!destination.exists());
        }
        for (field, value) in [
            (
                "postconditions",
                serde_json::json!([{ "BranchDeleted": "later" }]),
            ),
            (
                "compensation",
                serde_json::json!({ "RemoveCreatedArtifact": null }),
            ),
            ("required_consents", serde_json::json!([])),
        ] {
            let (temp, plan, destination) = generated_task_plan(
                false,
                vec![("first".into(), "printf never > \"$1\"".into())],
            );
            let mut wire = serde_json::to_value(plan).unwrap();
            let task = wire["steps"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|step| step["action"].get("RunTask").is_some())
                .unwrap();
            if field == "required_consents" {
                wire[field] = value;
            } else {
                task[field] = value;
            }
            if let Some(restored) = serde_json::from_value::<crate::lifecycle::OperationPlan>(wire)
                .ok()
                .filter(|restored| restored.validate_executable_plan().is_ok())
            {
                let task = restored
                    .steps()
                    .iter()
                    .find(|step| matches!(step.action(), StepAction::RunTask { .. }))
                    .unwrap();
                if field == "postconditions" {
                    assert!(!task.postconditions().is_empty(), "{field}");
                }
                let backend = ProductionBackend::new(temp.path().to_owned());
                assert!(
                    !backend.supports_action(&crate::execution::StepExecutionContext::new(
                        &restored, task
                    ))
                );
            }
            assert!(!temp.path().join(".git/ewtm").exists());
            assert!(!destination.exists());
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_rejects_run_task_before_state() {
        let temp = TempDir::new().unwrap();
        let mut plan = crate::lifecycle::test_plan(1);
        *plan.steps_mut()[0].action_mut() = StepAction::RunTask {
            name: "task".into(),
            argv: crate::lifecycle::CommandArgv::new(vec!["tool".into()]).unwrap(),
            cwd: stored(temp.path()),
            required: false,
            environment_allowlist: vec![],
        };
        let context = crate::execution::StepExecutionContext::new(&plan, &plan.steps()[0]);
        assert!(!ProductionBackend::new(temp.path().to_owned()).supports_action(&context));
        assert!(!temp.path().join("ewtm").exists());
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

    #[cfg(unix)]
    #[test]
    fn readonly_observer_fifo_swap_is_nonblocking_and_raii_scoped() {
        use std::os::unix::fs::FileTypeExt;

        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let regular = root.join("regular");
        let directory = root.join("directory");
        fs::write(&regular, b"stable bytes").unwrap();
        fs::create_dir(&directory).unwrap();
        assert!(matches!(
            infrastructure::readonly_observe_node(root, &regular),
            Ok(Some(infrastructure::ObservedNode::Regular { ref bytes, .. })) if bytes == b"stable bytes"
        ));
        assert!(matches!(
            infrastructure::readonly_observe_node(root, &directory),
            Ok(Some(infrastructure::ObservedNode::Directory))
        ));

        let guard = infrastructure::arm_observation_fifo_swap(&regular);
        assert!(
            infrastructure::readonly_observe_node(root, &regular)
                .unwrap()
                .is_none()
        );
        assert!(
            fs::symlink_metadata(&regular)
                .unwrap()
                .file_type()
                .is_fifo()
        );
        assert_eq!(infrastructure::observation_fifo_swap_invocation_count(), 1);
        drop(guard);
        assert_eq!(infrastructure::observation_fifo_swap_invocation_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_swap_during_copy_preflight_refuses_before_journal_or_invoke() {
        use std::os::unix::fs::FileTypeExt;

        let (temp, plan, destination, staging) = copy_fixture();
        let root = temp.path();
        let source = match artifact_step(&plan).action() {
            StepAction::CopyFileV3 { source, .. } => source.clone().into_path(),
            _ => unreachable!(),
        };
        let before = fs::read(root.join("tracked")).unwrap();
        let guard = infrastructure::arm_observation_fifo_swap(&source);
        let outcome = ExecutionEngine::new(ProductionBackend::new(root.to_owned()))
            .execute(plan.clone())
            .unwrap();
        assert!(matches!(outcome, ExecutionOutcome::PreflightRefused { .. }));
        assert_eq!(infrastructure::observation_fifo_swap_invocation_count(), 1);
        assert!(fs::symlink_metadata(&source).unwrap().file_type().is_fifo());
        assert!(!destination.exists());
        assert!(!staging.exists());
        assert_eq!(fs::read(root.join("tracked")).unwrap(), before);
        let journal_entries = fs::read_dir(root.join(".git/ewtm/journal"))
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(journal_entries, 0);
        for point in [
            infrastructure::ArtifactFaultPoint::StagingCreate,
            infrastructure::ArtifactFaultPoint::Write,
            infrastructure::ArtifactFaultPoint::Fchmod,
            infrastructure::ArtifactFaultPoint::FileFsync,
            infrastructure::ArtifactFaultPoint::Rename,
            infrastructure::ArtifactFaultPoint::CopyParentFsync,
            infrastructure::ArtifactFaultPoint::SymlinkCreate,
            infrastructure::ArtifactFaultPoint::SymlinkParentFsync,
        ] {
            assert_eq!(infrastructure::artifact_fault_invocation_count(point), 0);
        }
        drop(guard);
        assert_eq!(infrastructure::observation_fifo_swap_invocation_count(), 0);
    }

    #[test]
    fn unsupported_actions_remain_mutation_unavailable() {
        let temp = repository();
        let plan = crate::lifecycle::test_plan(1);
        let backend = ProductionBackend::new(temp.path().to_owned());
        for step in plan.steps() {
            let context = crate::execution::StepExecutionContext::new(&plan, step);
            if !backend.supports_action(&context) {
                assert!(matches!(
                    ProductionBackend::new(temp.path().to_owned()).invoke(&context),
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
        let result = backend.check_precondition(
            plan,
            None,
            crate::execution::ConditionPhase::InitialPreflight,
            &condition,
        );
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
        executable_new_branch_plan_with_manifests(root, destination, branch, vec![])
    }

    fn executable_new_branch_plan_with_manifests(
        root: &Path,
        destination: &Path,
        branch: &str,
        manifests: Vec<crate::planner::FileActionManifest>,
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
        let mut granted_consents = BTreeSet::new();
        if manifests
            .iter()
            .flat_map(|manifest| manifest.artifacts.iter())
            .any(|artifact| artifact.sensitive || artifact.replace_symlink)
        {
            granted_consents.insert(crate::lifecycle::ConsentId::new("file-rule:fixture").unwrap());
        }
        let intent = crate::lifecycle::CreateIntent {
            repository: repository.clone(),
            source: source.clone(),
            destination: Some(stored(destination.to_owned())),
            selected_tasks: BTreeSet::new(),
            skipped_rules: BTreeSet::new(),
            granted_consents,
            task_contracts: BTreeMap::new(),
            current_worktree_root: Some(stored(root.to_owned())),
            artifact_rule_contracts: BTreeMap::new(),
        };
        let known_rules = manifests
            .iter()
            .map(|m| m.rule.clone())
            .collect::<BTreeSet<_>>();
        let enabled_rules = known_rules.clone();
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
            known_rules,
            enabled_rules,
            known_tasks: BTreeSet::new(),
            manifests,
            tasks: vec![],
        })
        .unwrap()
    }

    #[cfg(unix)]
    fn artifact_manifest(
        root: &Path,
        destination: &Path,
        source_name: &str,
        kind: crate::planner::FileArtifactKind,
        mode_policy: crate::planner::FileModePolicy,
    ) -> crate::planner::FileActionManifest {
        use std::os::unix::fs::PermissionsExt;
        let source_root = root.canonicalize().unwrap();
        let source = source_root.join(source_name);
        let source_path = stored(source.clone());
        let destination_path = stored(destination.join(source_name));
        let (expectation, bytes, digest, link_target) = match kind {
            crate::planner::FileArtifactKind::CopyFile => {
                let data = fs::read(&source).unwrap();
                let mode = fs::metadata(&source).unwrap().permissions().mode() & 0o7777;
                (
                    crate::lifecycle::ArtifactSourceExpectationV3::Regular(
                        crate::lifecycle::RegularFileStateV3 {
                            bytes: data.len() as u64,
                            digest: crate::planner::artifact_digest(&data),
                            mode,
                        },
                    ),
                    data.len() as u64,
                    crate::planner::artifact_digest(&data),
                    None,
                )
            }
            crate::planner::FileArtifactKind::CreateSymlink => {
                let target = source.as_os_str().as_encoded_bytes();
                (
                    if source.is_dir() {
                        crate::lifecycle::ArtifactSourceExpectationV3::Directory
                    } else {
                        let data = fs::read(&source).unwrap();
                        crate::lifecycle::ArtifactSourceExpectationV3::Regular(
                            crate::lifecycle::RegularFileStateV3 {
                                bytes: data.len() as u64,
                                digest: crate::planner::artifact_digest(&data),
                                mode: fs::metadata(&source).unwrap().permissions().mode() & 0o7777,
                            },
                        )
                    },
                    target.len() as u64,
                    crate::planner::artifact_digest(target),
                    Some(source_path.clone()),
                )
            }
            crate::planner::FileArtifactKind::RelinkSymlink => unreachable!(),
        };
        crate::planner::FileActionManifest {
            rule: "fixture".into(),
            source_root: stored(source_root),
            artifacts: vec![crate::planner::FileArtifact {
                kind,
                source: source_path,
                destination: destination_path,
                bytes,
                digest: digest.clone(),
                source_expectation: expectation,
                fingerprint: digest,
                link_target,
                sensitive: mode_policy == crate::planner::FileModePolicy::Private,
                mode_policy,
                confirm: false,
                conflict: false,
                overlap: false,
                replace_symlink: false,
                compensation: None,
                relink_facts: None,
            }],
            digest: crate::planner::artifact_digest(b"fixture-manifest-placeholder"),
        }
    }

    #[cfg(unix)]
    fn relink_manifest(root: &Path, destination: &Path) -> crate::planner::FileActionManifest {
        relink_manifest_with_targets(root, destination, b"old\n", b"new\n")
    }

    #[cfg(unix)]
    fn relink_manifest_with_targets(
        root: &Path,
        destination: &Path,
        old_bytes: &[u8],
        new_bytes: &[u8],
    ) -> crate::planner::FileActionManifest {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;
        let root = root.canonicalize().unwrap();
        symlink(OsString::from_vec(old_bytes.to_vec()), root.join("link")).unwrap();
        git(&root, &["add", "link"]);
        git(&root, &["commit", "-m", "tracked-link"]);
        symlink(
            OsString::from_vec(new_bytes.to_vec()),
            root.join("source-link"),
        )
        .unwrap();
        let checkout_oid = oid(&root, "refs/heads/main");
        let desired = crate::lifecycle::SymlinkStateV3 {
            target: stored(PathBuf::from(OsString::from_vec(new_bytes.to_vec()))),
            target_digest: crate::planner::artifact_digest(new_bytes),
        };
        let old = crate::lifecycle::SymlinkStateV3 {
            target: stored(PathBuf::from(OsString::from_vec(old_bytes.to_vec()))),
            target_digest: crate::planner::artifact_digest(old_bytes),
        };
        crate::planner::FileActionManifest {
            rule: "fixture".into(),
            source_root: stored(root.clone()),
            artifacts: vec![crate::planner::FileArtifact {
                kind: crate::planner::FileArtifactKind::RelinkSymlink,
                source: stored(root.join("source-link")),
                destination: stored(destination.join("link")),
                bytes: new_bytes.len() as u64,
                digest: desired.target_digest.clone(),
                source_expectation: crate::lifecycle::ArtifactSourceExpectationV3::Symlink(
                    desired.clone(),
                ),
                fingerprint: desired.target_digest.clone(),
                link_target: Some(desired.target.clone()),
                sensitive: false,
                mode_policy: crate::planner::FileModePolicy::NotApplicable,
                confirm: false,
                conflict: false,
                overlap: false,
                replace_symlink: true,
                compensation: None,
                relink_facts: Some(crate::planner::RelinkCheckoutFacts {
                    checkout_oid,
                    checkout_relative_path: stored(PathBuf::from("link")),
                    expected_old: old,
                }),
            }],
            digest: crate::planner::artifact_digest(b"fixture-manifest-placeholder"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn generated_relink_plan_executes_create_and_forward_relink() {
        let temp = repository();
        let root = temp.path().canonicalize().unwrap();
        let destination = root.join("future");
        let manifest = relink_manifest(&root, &destination);
        let plan = executable_new_branch_plan_with_manifests(
            &root,
            &destination,
            "feature",
            vec![manifest],
        );
        let (replacement, backup) = match plan.steps()[1].action() {
            StepAction::RelinkSymlinkV3 {
                replacement_staging,
                backup_staging,
                ..
            } => (
                replacement_staging.path.clone().into_path(),
                backup_staging.path.clone().into_path(),
            ),
            _ => unreachable!(),
        };
        let outcome = ExecutionEngine::new(ProductionBackend::new(root.clone()))
            .execute(plan.clone())
            .unwrap();
        assert!(matches!(outcome, ExecutionOutcome::Applied { .. }));
        assert_eq!(
            fs::read_link(destination.join("link")).unwrap(),
            PathBuf::from("new\n")
        );
        assert!(!replacement.exists());
        assert!(!backup.exists());
        assert_eq!(
            fs::read_link(root.join("link")).unwrap(),
            PathBuf::from("old\n")
        );
        assert_eq!(
            fs::read_link(root.join("source-link")).unwrap(),
            PathBuf::from("new\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn generated_relink_fault_matrix_reconciles_exact_tuples() {
        let points = [
            (
                infrastructure::ArtifactFaultPoint::RelinkReplacementCreate,
                (Some("old\n".to_owned()), Some("new\n".to_owned()), None),
                false,
            ),
            (
                infrastructure::ArtifactFaultPoint::RelinkReplacementFsync,
                (Some("old\n".to_owned()), Some("new\n".to_owned()), None),
                false,
            ),
            (
                infrastructure::ArtifactFaultPoint::RelinkBackupRename,
                (None, Some("new\n".to_owned()), Some("old\n".to_owned())),
                false,
            ),
            (
                infrastructure::ArtifactFaultPoint::RelinkBackupFsync,
                (None, Some("new\n".to_owned()), Some("old\n".to_owned())),
                false,
            ),
            (
                infrastructure::ArtifactFaultPoint::RelinkPublicationRename,
                (Some("new\n".to_owned()), None, Some("old\n".to_owned())),
                false,
            ),
            (
                infrastructure::ArtifactFaultPoint::RelinkPublicationFsync,
                (Some("new\n".to_owned()), None, Some("old\n".to_owned())),
                false,
            ),
            (
                infrastructure::ArtifactFaultPoint::RelinkBackupUnlink,
                (Some("new\n".to_owned()), None, None),
                true,
            ),
            (
                infrastructure::ArtifactFaultPoint::RelinkCleanupFsync,
                (Some("new\n".to_owned()), None, None),
                true,
            ),
        ];
        for (point, expected, clean) in points {
            let temp = repository();
            let root = temp.path().canonicalize().unwrap();
            let destination = root.join("future");
            let plan = executable_new_branch_plan_with_manifests(
                &root,
                &destination,
                "feature",
                vec![relink_manifest(&root, &destination)],
            );
            let (destination, replacement, backup) = match plan.steps()[1].action() {
                StepAction::RelinkSymlinkV3 {
                    destination,
                    replacement_staging,
                    backup_staging,
                    ..
                } => (
                    destination.clone().into_path(),
                    replacement_staging.path.clone().into_path(),
                    backup_staging.path.clone().into_path(),
                ),
                _ => unreachable!(),
            };
            let _fault = infrastructure::arm_artifact_fault(point);
            let operation_id = *plan.operation_id();
            let outcome = ExecutionEngine::new(ProductionBackend::new(root.clone()))
                .execute(plan.clone())
                .unwrap();
            assert_eq!(matches!(outcome, ExecutionOutcome::Applied { .. }), clean);
            let target = |path: &Path| {
                fs::read_link(path)
                    .ok()
                    .and_then(|value| value.to_str().map(str::to_owned))
            };
            assert_eq!(
                (target(&destination), target(&replacement), target(&backup)),
                expected
            );
            let journal = crate::journal_store::JournalStore::new(&root.join(".git"))
                .read(&operation_id)
                .unwrap();
            assert_eq!(
                journal.steps()[0].status(),
                crate::journal::StepStatus::Applied
            );
            if clean {
                assert_eq!(journal.status(), crate::journal::OperationStatus::Applied);
                assert_eq!(
                    journal.steps()[1].status(),
                    crate::journal::StepStatus::Applied
                );
            } else {
                assert!(matches!(outcome, ExecutionOutcome::NeedsAttention { .. }));
                assert_eq!(
                    journal.status(),
                    crate::journal::OperationStatus::NeedsAttention
                );
                assert_eq!(
                    journal.steps()[1].status(),
                    crate::journal::StepStatus::NeedsAttention
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn generated_relink_preserves_non_utf8_target_bytes_and_journal_roundtrip() {
        let temp = repository();
        let root = temp.path().canonicalize().unwrap();
        let destination = root.join("future");
        let old = b"old-\xff\n";
        let new = b"new-\xfe\n";
        let plan = executable_new_branch_plan_with_manifests(
            &root,
            &destination,
            "feature",
            vec![relink_manifest_with_targets(&root, &destination, old, new)],
        );
        let (link, _, _) = relink_paths(&plan);
        let operation_id = *plan.operation_id();
        let outcome = ExecutionEngine::new(ProductionBackend::new(root.clone()))
            .execute(plan)
            .unwrap();
        assert!(matches!(outcome, ExecutionOutcome::Applied { .. }));
        assert_eq!(
            fs::read_link(&link).unwrap().as_os_str().as_encoded_bytes(),
            new
        );
        let journal = journal_for_id(&root, &operation_id);
        let wire = serde_json::to_vec(&journal).unwrap();
        let roundtrip: crate::journal::Journal = serde_json::from_slice(&wire).unwrap();
        assert_eq!(roundtrip.status(), crate::journal::OperationStatus::Applied);
    }

    #[cfg(unix)]
    #[test]
    fn relink_started_write_fault_four_skips_relink_effect() {
        let _journal_faults = crate::journal_store::test_fault_guard();
        let temp = repository();
        let root = temp.path().canonicalize().unwrap();
        let destination = root.join("future");
        let plan = executable_new_branch_plan_with_manifests(
            &root,
            &destination,
            "feature",
            vec![relink_manifest(&root, &destination)],
        );
        let (link, replacement, backup) = relink_paths(&plan);
        crate::journal_store::inject_fail_on_atomic_write(4);
        assert!(matches!(
            ExecutionEngine::new(ProductionBackend::new(root.clone())).execute(plan.clone()),
            Err(crate::execution::ExecutionError::Journal(_))
        ));
        assert_eq!(link_snapshot(&link), Some((true, b"old\n".to_vec())));
        assert_eq!(link_snapshot(&replacement), None);
        assert_eq!(link_snapshot(&backup), None);
        let journal = journal_for(&root, &plan);
        assert_eq!(
            journal.steps()[0].status(),
            crate::journal::StepStatus::Applied
        );
        assert_eq!(
            journal.steps()[1].status(),
            crate::journal::StepStatus::Pending
        );
    }

    #[cfg(unix)]
    #[test]
    fn relink_started_restart_write_five_preserves_partial_and_clean_snapshots() {
        let cases = [
            (
                Some(infrastructure::ArtifactFaultPoint::RelinkReplacementFsync),
                (
                    Some((true, b"old\n".to_vec())),
                    Some((true, b"new\n".to_vec())),
                    None,
                ),
            ),
            (
                Some(infrastructure::ArtifactFaultPoint::RelinkBackupFsync),
                (
                    None,
                    Some((true, b"new\n".to_vec())),
                    Some((true, b"old\n".to_vec())),
                ),
            ),
            (
                Some(infrastructure::ArtifactFaultPoint::RelinkPublicationFsync),
                (
                    Some((true, b"new\n".to_vec())),
                    None,
                    Some((true, b"old\n".to_vec())),
                ),
            ),
            (None, (Some((true, b"new\n".to_vec())), None, None)),
        ];
        for (fault, expected) in cases {
            let _journal_faults = crate::journal_store::test_fault_guard();
            let temp = repository();
            let root = temp.path().canonicalize().unwrap();
            let destination = root.join("future");
            let plan = executable_new_branch_plan_with_manifests(
                &root,
                &destination,
                "feature",
                vec![relink_manifest(&root, &destination)],
            );
            let paths = relink_paths(&plan);
            let _artifact_fault = fault.map(infrastructure::arm_artifact_fault);
            crate::journal_store::inject_fail_on_atomic_write(5);
            assert!(matches!(
                ExecutionEngine::new(ProductionBackend::new(root.clone())).execute(plan.clone()),
                Err(crate::execution::ExecutionError::Journal(_))
            ));
            let journal = journal_for(&root, &plan);
            assert_eq!(journal.status(), crate::journal::OperationStatus::Running);
            assert_eq!(
                journal.steps()[1].status(),
                crate::journal::StepStatus::Started
            );
            let snapshot = (
                link_snapshot(&paths.0),
                link_snapshot(&paths.1),
                link_snapshot(&paths.2),
            );
            assert_eq!(snapshot, expected);
            drop(_artifact_fault);
            let restarted = ExecutionEngine::new(ProductionBackend::new(root.clone()))
                .execute(plan.clone())
                .unwrap();
            if fault.is_some() {
                assert!(matches!(
                    restarted,
                    ExecutionOutcome::ExistingOperation { .. }
                ));
            } else {
                assert!(matches!(restarted, ExecutionOutcome::AlreadyApplied { .. }));
            }
            assert_eq!(
                (
                    link_snapshot(&paths.0),
                    link_snapshot(&paths.1),
                    link_snapshot(&paths.2)
                ),
                snapshot
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn relink_probe_tuple_and_collision_matrix_is_read_only() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let parent = temp.path().join("parent");
        fs::create_dir(&parent).unwrap();
        let destination = parent.join("destination");
        let replacement = parent.join("replacement");
        let backup = parent.join("backup");
        let old = crate::lifecycle::SymlinkStateV3 {
            target: stored(PathBuf::from("old")),
            target_digest: crate::planner::artifact_digest(b"old"),
        };
        let new = crate::lifecycle::SymlinkStateV3 {
            target: stored(PathBuf::from("new")),
            target_digest: crate::planner::artifact_digest(b"new"),
        };
        let clear = || {
            for path in [&destination, &replacement, &backup] {
                let _ = fs::remove_file(path);
            }
        };
        let put = |path: &Path, target: Option<&str>| {
            if let Some(target) = target {
                symlink(target, path).unwrap();
            }
        };
        let cases = [
            (
                (Some("old"), None, None),
                infrastructure::RelinkProbeState::NotApplied,
            ),
            (
                (Some("old"), Some("new"), None),
                infrastructure::RelinkProbeState::Unknown,
            ),
            (
                (None, Some("new"), Some("old")),
                infrastructure::RelinkProbeState::Unknown,
            ),
            (
                (Some("new"), None, Some("old")),
                infrastructure::RelinkProbeState::Unknown,
            ),
            (
                (Some("new"), None, None),
                infrastructure::RelinkProbeState::Applied,
            ),
            (
                (Some("wrong"), None, None),
                infrastructure::RelinkProbeState::Unknown,
            ),
            (
                (Some("old"), Some("wrong"), None),
                infrastructure::RelinkProbeState::Unknown,
            ),
            (
                (Some("old"), None, Some("wrong")),
                infrastructure::RelinkProbeState::Unknown,
            ),
        ];
        for (tuple, expected) in cases {
            clear();
            put(&destination, tuple.0);
            put(&replacement, tuple.1);
            put(&backup, tuple.2);
            let before = (
                link_snapshot(&destination),
                link_snapshot(&replacement),
                link_snapshot(&backup),
            );
            let actual = infrastructure::probe_relink_symlink_v3(
                &destination,
                &replacement,
                &backup,
                &old,
                &new,
            )
            .unwrap();
            assert_eq!(actual, expected);
            assert_eq!(
                (
                    link_snapshot(&destination),
                    link_snapshot(&replacement),
                    link_snapshot(&backup)
                ),
                before
            );
        }
        for path in [&destination, &replacement, &backup] {
            clear();
            fs::write(path, b"foreign").unwrap();
            assert_eq!(
                infrastructure::probe_relink_symlink_v3(
                    &destination,
                    &replacement,
                    &backup,
                    &old,
                    &new
                )
                .unwrap(),
                infrastructure::RelinkProbeState::Unknown
            );
            assert!(fs::symlink_metadata(path).unwrap().is_file());
        }
        clear();
        assert_eq!(
            infrastructure::probe_relink_symlink_v3(
                &parent.join("missing/destination"),
                &replacement,
                &backup,
                &old,
                &new
            )
            .unwrap(),
            infrastructure::RelinkProbeState::Unknown
        );
        symlink(&parent, temp.path().join("parent-link")).unwrap();
        assert!(
            infrastructure::probe_relink_symlink_v3(
                &temp.path().join("parent-link/destination"),
                &temp.path().join("parent-link/replacement"),
                &temp.path().join("parent-link/backup"),
                &old,
                &new
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    fn artifact_step(plan: &crate::lifecycle::OperationPlan) -> &PlanStep {
        &plan.steps()[1]
    }

    #[cfg(unix)]
    fn artifact_destination(plan: &crate::lifecycle::OperationPlan) -> PathBuf {
        match artifact_step(plan).action() {
            StepAction::CopyFileV3 { destination, .. }
            | StepAction::CreateSymlinkV3 { destination, .. } => destination.clone().into_path(),
            _ => unreachable!(),
        }
    }

    #[cfg(unix)]
    fn relink_paths(plan: &crate::lifecycle::OperationPlan) -> (PathBuf, PathBuf, PathBuf) {
        match artifact_step(plan).action() {
            StepAction::RelinkSymlinkV3 {
                destination,
                replacement_staging,
                backup_staging,
                ..
            } => (
                destination.clone().into_path(),
                replacement_staging.path.clone().into_path(),
                backup_staging.path.clone().into_path(),
            ),
            _ => unreachable!(),
        }
    }

    #[cfg(unix)]
    fn link_snapshot(path: &Path) -> Option<(bool, Vec<u8>)> {
        let metadata = fs::symlink_metadata(path).ok()?;
        if metadata.file_type().is_symlink() {
            Some((
                true,
                fs::read_link(path)
                    .ok()?
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec(),
            ))
        } else if metadata.is_file() {
            Some((false, fs::read(path).ok()?))
        } else {
            Some((false, Vec::new()))
        }
    }

    #[cfg(unix)]
    fn artifact_probe(
        backend: &mut ProductionBackend,
        plan: &crate::lifecycle::OperationPlan,
    ) -> ProbeVerdict {
        let step = artifact_step(plan);
        let context = crate::execution::StepExecutionContext::new(plan, step);
        backend
            .probe(
                &context,
                ProbeContext::AfterAttempt {
                    executor_succeeded: false,
                },
            )
            .unwrap()
    }

    #[cfg(unix)]
    fn journal_for(root: &Path, plan: &crate::lifecycle::OperationPlan) -> crate::journal::Journal {
        crate::journal_store::JournalStore::new(&root.join(".git"))
            .read(plan.operation_id())
            .unwrap()
    }

    #[cfg(unix)]
    fn journal_for_id(root: &Path, id: &crate::lifecycle::OperationId) -> crate::journal::Journal {
        crate::journal_store::JournalStore::new(&root.join(".git"))
            .read(id)
            .unwrap()
    }

    #[cfg(unix)]
    fn copy_fixture() -> (TempDir, crate::lifecycle::OperationPlan, PathBuf, PathBuf) {
        let temp = repository();
        let root = temp.path();
        let source = root.join("artifact");
        fs::write(&source, b"durable fixture").unwrap();
        let destination = root.join("linked");
        let manifest = artifact_manifest(
            root,
            &destination,
            "artifact",
            crate::planner::FileArtifactKind::CopyFile,
            crate::planner::FileModePolicy::Private,
        );
        let plan = executable_new_branch_plan_with_manifests(
            root,
            &destination,
            "artifact-branch",
            vec![manifest],
        );
        let staging = match artifact_step(&plan).action() {
            StepAction::CopyFileV3 { staging, .. } => staging.path.clone().into_path(),
            _ => unreachable!(),
        };
        (temp, plan.clone(), artifact_destination(&plan), staging)
    }

    #[cfg(unix)]
    #[test]
    fn copy_faults_before_publication_remain_owned_and_need_attention() {
        use std::os::unix::fs::PermissionsExt;
        for point in [
            infrastructure::ArtifactFaultPoint::StagingCreate,
            infrastructure::ArtifactFaultPoint::Write,
            infrastructure::ArtifactFaultPoint::Fchmod,
            infrastructure::ArtifactFaultPoint::FileFsync,
        ] {
            let (temp, plan, final_path, staging) = copy_fixture();
            let root = temp.path();
            let guard = infrastructure::arm_artifact_fault(point);
            let first = ExecutionEngine::new(ProductionBackend::new(root.to_owned()))
                .execute(plan.clone())
                .unwrap();
            assert!(matches!(first, ExecutionOutcome::NeedsAttention { .. }));
            assert_eq!(infrastructure::artifact_fault_invocation_count(point), 1);
            assert!(!final_path.exists());
            let staged_bytes = fs::read(&staging).unwrap();
            if point == infrastructure::ArtifactFaultPoint::StagingCreate {
                assert!(staged_bytes.is_empty());
            } else {
                assert_eq!(staged_bytes, b"durable fixture");
            }
            let snapshot = (
                fs::read(&staging).unwrap(),
                fs::metadata(&staging).unwrap().permissions().mode() & 0o7777,
            );
            assert_eq!(
                artifact_probe(&mut ProductionBackend::new(root.to_owned()), &plan),
                ProbeVerdict::Unknown
            );
            let journal = journal_for(root, &plan);
            assert_eq!(
                journal.status(),
                crate::journal::OperationStatus::NeedsAttention
            );
            assert_eq!(
                journal.steps()[1].status(),
                crate::journal::StepStatus::NeedsAttention
            );
            let bytes = fs::read(
                root.join(".git/ewtm/journal")
                    .join(format!("{}.json", plan.operation_id())),
            )
            .unwrap();
            let second = ExecutionEngine::new(ProductionBackend::new(root.to_owned()))
                .execute(plan.clone())
                .unwrap();
            assert!(matches!(second, ExecutionOutcome::ExistingOperation { .. }));
            assert_eq!(infrastructure::artifact_fault_invocation_count(point), 1);
            assert_eq!(
                snapshot,
                (
                    fs::read(&staging).unwrap(),
                    fs::metadata(&staging).unwrap().permissions().mode() & 0o7777
                )
            );
            assert!(!final_path.exists());
            assert_eq!(
                bytes,
                fs::read(
                    root.join(".git/ewtm/journal")
                        .join(format!("{}.json", plan.operation_id()))
                )
                .unwrap()
            );
            drop(guard);
            assert_eq!(infrastructure::artifact_fault_invocation_count(point), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn copy_faults_after_publication_reconcile_applied() {
        use std::os::unix::fs::PermissionsExt;
        for point in [
            infrastructure::ArtifactFaultPoint::Rename,
            infrastructure::ArtifactFaultPoint::CopyParentFsync,
        ] {
            let (temp, plan, final_path, staging) = copy_fixture();
            let root = temp.path();
            let guard = infrastructure::arm_artifact_fault(point);
            let outcome = ExecutionEngine::new(ProductionBackend::new(root.to_owned()))
                .execute(plan.clone())
                .unwrap();
            assert!(
                matches!(outcome, ExecutionOutcome::Applied { .. }),
                "{outcome:?}"
            );
            assert_eq!(infrastructure::artifact_fault_invocation_count(point), 1);
            assert_eq!(fs::read(&final_path).unwrap(), b"durable fixture");
            assert_eq!(
                fs::metadata(&final_path).unwrap().permissions().mode() & 0o7777,
                0o600
            );
            assert!(!staging.exists());
            assert_eq!(
                artifact_probe(&mut ProductionBackend::new(root.to_owned()), &plan),
                ProbeVerdict::Applied
            );
            assert_eq!(
                journal_for(root, &plan).status(),
                crate::journal::OperationStatus::Applied
            );
            drop(guard);
            assert_eq!(infrastructure::artifact_fault_invocation_count(point), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_durability_faults_reconcile_applied() {
        for (point, source_name) in [
            (
                infrastructure::ArtifactFaultPoint::SymlinkCreate,
                "link-source",
            ),
            (
                infrastructure::ArtifactFaultPoint::SymlinkParentFsync,
                "link-source-2",
            ),
        ] {
            let temp = repository();
            let root = temp.path();
            let source = root.join(source_name);
            fs::write(&source, b"target").unwrap();
            let destination = root.join(format!("linked-{source_name}"));
            let manifest = artifact_manifest(
                root,
                &destination,
                source_name,
                crate::planner::FileArtifactKind::CreateSymlink,
                crate::planner::FileModePolicy::NotApplicable,
            );
            let plan = executable_new_branch_plan_with_manifests(
                root,
                &destination,
                &format!("branch-{source_name}"),
                vec![manifest],
            );
            let guard = infrastructure::arm_artifact_fault(point);
            let outcome = ExecutionEngine::new(ProductionBackend::new(root.to_owned()))
                .execute(plan.clone())
                .unwrap();
            assert!(
                matches!(outcome, ExecutionOutcome::Applied { .. }),
                "{outcome:?}"
            );
            assert_eq!(infrastructure::artifact_fault_invocation_count(point), 1);
            let desired_target = match artifact_step(&plan).action() {
                StepAction::CreateSymlinkV3 { desired, .. } => desired.target.clone().into_path(),
                _ => unreachable!(),
            };
            assert_eq!(
                fs::read_link(artifact_destination(&plan)).unwrap(),
                desired_target
            );
            assert_eq!(
                artifact_probe(&mut ProductionBackend::new(root.to_owned()), &plan),
                ProbeVerdict::Applied
            );
            assert_eq!(
                journal_for(root, &plan).status(),
                crate::journal::OperationStatus::Applied
            );
            drop(guard);
            assert_eq!(infrastructure::artifact_fault_invocation_count(point), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn artifact_probe_classifies_contradictory_states_unknown() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (temp, plan, final_path, staging) = copy_fixture();
        let root = temp.path();
        fs::create_dir(root.join("linked")).unwrap();
        let mut backend = ProductionBackend::new(root.to_owned());
        assert_eq!(
            artifact_probe(&mut backend, &plan),
            ProbeVerdict::NotApplied
        );
        fs::write(&staging, b"durable fixture").unwrap();
        assert_eq!(artifact_probe(&mut backend, &plan), ProbeVerdict::Unknown);
        fs::remove_file(&staging).unwrap();
        fs::write(&final_path, b"wrong").unwrap();
        assert_eq!(artifact_probe(&mut backend, &plan), ProbeVerdict::Unknown);
        fs::write(&final_path, b"durable fixture").unwrap();
        fs::set_permissions(&final_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(artifact_probe(&mut backend, &plan), ProbeVerdict::Unknown);
        fs::remove_file(&final_path).unwrap();
        fs::create_dir(&final_path).unwrap();
        assert_eq!(artifact_probe(&mut backend, &plan), ProbeVerdict::Unknown);
        fs::remove_dir(&final_path).unwrap();
        let temp = repository();
        let root = temp.path();
        let source = root.join("target");
        fs::write(&source, b"target").unwrap();
        let destination = root.join("linked");
        let manifest = artifact_manifest(
            root,
            &destination,
            "target",
            crate::planner::FileArtifactKind::CreateSymlink,
            crate::planner::FileModePolicy::NotApplicable,
        );
        let plan = executable_new_branch_plan_with_manifests(
            root,
            &destination,
            "symlink-probe",
            vec![manifest],
        );
        let final_path = artifact_destination(&plan);
        let desired_target = match artifact_step(&plan).action() {
            StepAction::CreateSymlinkV3 { desired, .. } => desired.target.clone().into_path(),
            _ => unreachable!(),
        };
        fs::create_dir(&destination).unwrap();
        let mut backend = ProductionBackend::new(root.to_owned());
        assert_eq!(
            artifact_probe(&mut backend, &plan),
            ProbeVerdict::NotApplied
        );
        symlink(&desired_target, &final_path).unwrap();
        assert_eq!(artifact_probe(&mut backend, &plan), ProbeVerdict::Applied);
        fs::remove_file(&final_path).unwrap();
        symlink("wrong", &final_path).unwrap();
        assert_eq!(artifact_probe(&mut backend, &plan), ProbeVerdict::Unknown);
        fs::remove_file(&final_path).unwrap();
        fs::write(&final_path, b"regular").unwrap();
        assert_eq!(artifact_probe(&mut backend, &plan), ProbeVerdict::Unknown);
    }

    #[cfg(unix)]
    #[test]
    fn artifact_support_boundaries_precede_state() {
        let (temp, plan, _, _) = copy_fixture();
        let root = temp.path();
        let step = artifact_step(&plan).clone();
        let mut mismatched = step.clone();
        mismatched.preconditions_mut()[0] = Precondition::ArtifactSourceAtV3 {
            rule: "other".into(),
            source_root: stored(root.to_owned()),
            source: stored(root.join("artifact")),
            expectation: match step.action() {
                StepAction::CopyFileV3 {
                    expected_source, ..
                } => {
                    crate::lifecycle::ArtifactSourceExpectationV3::Regular(expected_source.clone())
                }
                _ => unreachable!(),
            },
            manifest_digest: ObjectId::new("0".repeat(40)).unwrap(),
        };
        let mismatched_plan = context_plan(&plan, mismatched.clone());
        let backend = ProductionBackend::new(root.to_owned());
        for phase in [
            crate::execution::ConditionPhase::InitialPreflight,
            crate::execution::ConditionPhase::BeforeInvoke,
        ] {
            assert!(!backend.supports_precondition(
                &mismatched_plan,
                Some(&mismatched),
                phase,
                &mismatched.preconditions()[0]
            ));
        }
        assert!(matches!(
            ExecutionEngine::new(ProductionBackend::new(root.to_owned())).execute(mismatched_plan),
            Err(crate::execution::ExecutionError::UnsupportedPlan(_))
        ));
        assert!(!root.join(".git/ewtm").exists());
        let mut probe_only = step;
        probe_only
            .preconditions_mut()
            .push(Precondition::TreeSymlinkAtV3 {
                commit_oid: oid(root, "HEAD"),
                checkout_relative_path: stored("artifact"),
                expected: crate::lifecycle::SymlinkStateV3 {
                    target: stored(root.join("artifact")),
                    target_digest: crate::planner::artifact_digest(
                        root.join("artifact").as_os_str().as_encoded_bytes(),
                    ),
                },
            });
        let probe_plan = context_plan(&plan, probe_only.clone());
        assert!(!backend.supports_precondition(
            &probe_plan,
            Some(&probe_only),
            crate::execution::ConditionPhase::InitialPreflight,
            probe_only.preconditions().last().unwrap()
        ));
        assert!(!backend.supports_precondition(
            &probe_plan,
            Some(&probe_only),
            crate::execution::ConditionPhase::BeforeInvoke,
            probe_only.preconditions().last().unwrap()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn engine_applies_v3_artifacts_with_exact_state() {
        use std::os::unix::fs::PermissionsExt;
        for (kind, policy, source_name) in [
            (
                crate::planner::FileArtifactKind::CopyFile,
                crate::planner::FileModePolicy::Private,
                "private",
            ),
            (
                crate::planner::FileArtifactKind::CopyFile,
                crate::planner::FileModePolicy::PreserveSafe,
                "safe",
            ),
            (
                crate::planner::FileArtifactKind::CreateSymlink,
                crate::planner::FileModePolicy::NotApplicable,
                "regular",
            ),
            (
                crate::planner::FileArtifactKind::CreateSymlink,
                crate::planner::FileModePolicy::NotApplicable,
                "directory",
            ),
        ] {
            let temp = repository();
            let root = temp.path();
            let source = root.join(source_name);
            if source_name == "directory" {
                fs::create_dir(&source).unwrap();
            } else {
                fs::write(&source, b"fixture bytes\0").unwrap();
            }
            fs::set_permissions(
                &source,
                fs::Permissions::from_mode(if source_name == "safe" { 0o6777 } else { 0o754 }),
            )
            .unwrap_or(());
            let destination = root.join(format!("linked-{source_name}"));
            let manifest = artifact_manifest(root, &destination, source_name, kind, policy);
            let plan = executable_new_branch_plan_with_manifests(
                root,
                &destination,
                &format!("b-{source_name}"),
                vec![manifest],
            );
            assert_eq!(plan.steps().len(), 2);
            assert!(matches!(
                artifact_step(&plan).action(),
                StepAction::CopyFileV3 { .. } | StepAction::CreateSymlinkV3 { .. }
            ));
            let backend = ProductionBackend::new(root.to_owned());
            let context = crate::execution::StepExecutionContext::new(&plan, artifact_step(&plan));
            assert_eq!(
                backend.probe_capability(&context),
                ProbeCapability::Deterministic
            );
            for phase in [
                crate::execution::ConditionPhase::InitialPreflight,
                crate::execution::ConditionPhase::BeforeInvoke,
            ] {
                let guard = &artifact_step(&plan).preconditions()[0];
                assert!(backend.supports_precondition(
                    &plan,
                    Some(artifact_step(&plan)),
                    phase,
                    guard
                ));
            }
            let outcome = ExecutionEngine::new(ProductionBackend::new(root.to_owned()))
                .execute(plan.clone())
                .unwrap();
            assert!(matches!(outcome, ExecutionOutcome::Applied { .. }));
            let output = artifact_destination(&plan);
            match kind {
                crate::planner::FileArtifactKind::CopyFile => {
                    let desired = match artifact_step(&plan).action() {
                        StepAction::CopyFileV3 { desired_output, .. } => desired_output,
                        _ => unreachable!(),
                    };
                    assert_eq!(fs::read(&output).unwrap(), fs::read(&source).unwrap());
                    assert_eq!(
                        fs::metadata(&output).unwrap().permissions().mode() & 0o7777,
                        desired.mode
                    );
                    if let StepAction::CopyFileV3 { staging, .. } = artifact_step(&plan).action() {
                        assert!(!staging.path.as_path().exists());
                    }
                }
                crate::planner::FileArtifactKind::CreateSymlink => {
                    let target = fs::read_link(&output).unwrap();
                    let desired = match artifact_step(&plan).action() {
                        StepAction::CreateSymlinkV3 { desired, .. } => desired,
                        _ => unreachable!(),
                    };
                    assert_eq!(target, desired.target.as_path());
                    assert_eq!(
                        crate::planner::artifact_digest(target.as_os_str().as_encoded_bytes()),
                        desired.target_digest
                    );
                }
                _ => unreachable!(),
            }
            let journal = journal_for(root, &plan);
            assert_eq!(journal.status(), crate::journal::OperationStatus::Applied);
            assert!(
                journal
                    .steps()
                    .iter()
                    .all(|s| s.status() == crate::journal::StepStatus::Applied)
            );
            assert!(!journal.is_unresolved());
        }
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
            ExecutionEngine::new(ProductionBackend::new(root.to_owned())).execute(plan.clone()),
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
        let clean_plan = context_plan(&plan, clean.clone());
        let context =
            crate::execution::StepExecutionContext::new(&clean_plan, &clean_plan.steps()[0]);
        assert!(backend.supports_action(&context));
        let mut extra = clean.clone();
        extra
            .preconditions_mut()
            .push(Precondition::BareRepositoryFalse);
        let extra_plan = context_plan(&plan, extra);
        assert!(
            ProductionBackend::new(root.to_owned()).supports_action(
                &crate::execution::StepExecutionContext::new(&extra_plan, &extra_plan.steps()[0])
            ),
            "dirty-removal consent does not replace exact clean guard"
        );
        let mut missing = clean.clone();
        missing.preconditions_mut().clear();
        let missing_plan = context_plan(&plan, missing);
        assert!(!ProductionBackend::new(root.to_owned()).supports_action(
            &crate::execution::StepExecutionContext::new(&missing_plan, &missing_plan.steps()[0]),
        ));
        let mut mismatched = clean;
        mismatched.preconditions_mut()[0] = Precondition::WorktreeClean {
            path: stored(root.join("other")),
        };
        let mismatched_plan = context_plan(&plan, mismatched);
        assert!(!ProductionBackend::new(root.to_owned()).supports_action(
            &crate::execution::StepExecutionContext::new(
                &mismatched_plan,
                &mismatched_plan.steps()[0]
            ),
        ));
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
        assert!(
            backend
                .check_precondition(
                    &plan,
                    None,
                    crate::execution::ConditionPhase::InitialPreflight,
                    &fatal,
                )
                .is_ok()
        );
        assert!(!backend.supports_precondition(
            &plan,
            None,
            crate::execution::ConditionPhase::InitialPreflight,
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
        let base_plan = crate::lifecycle::test_plan(1);
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
        let delete_plan = context_plan(&base_plan, delete.clone());
        ProductionBackend::new(root.to_owned())
            .invoke(&crate::execution::StepExecutionContext::new(
                &delete_plan,
                &delete_plan.steps()[0],
            ))
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
        let stale_plan = context_plan(&base_plan, stale.clone());
        assert!(
            ProductionBackend::new(root.to_owned())
                .invoke(&crate::execution::StepExecutionContext::new(
                    &stale_plan,
                    &stale_plan.steps()[0]
                ))
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
        let symbolic_plan = context_plan(&base_plan, symbolic.clone());
        assert!(
            ProductionBackend::new(root.to_owned())
                .invoke(&crate::execution::StepExecutionContext::new(
                    &symbolic_plan,
                    &symbolic_plan.steps()[0]
                ))
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
        let remove_plan = context_plan(&base_plan, remove.clone());
        ProductionBackend::new(root.to_owned())
            .invoke(&crate::execution::StepExecutionContext::new(
                &remove_plan,
                &remove_plan.steps()[0],
            ))
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
        let base_plan = crate::lifecycle::test_plan(1);
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
        let step_plan = context_plan(&base_plan, step.clone());
        assert_eq!(
            ProductionBackend::new(root.to_owned())
                .probe(
                    &crate::execution::StepExecutionContext::new(&step_plan, &step_plan.steps()[0]),
                    ProbeContext::StartupReconciliation
                )
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
        let mismatch_plan = context_plan(&base_plan, mismatch.clone());
        assert_eq!(
            ProductionBackend::new(root.to_owned())
                .probe(
                    &crate::execution::StepExecutionContext::new(
                        &mismatch_plan,
                        &mismatch_plan.steps()[0]
                    ),
                    ProbeContext::StartupReconciliation
                )
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
        let removed_plan = context_plan(&base_plan, removed.clone());
        assert_eq!(
            ProductionBackend::new(root.to_owned())
                .probe(
                    &crate::execution::StepExecutionContext::new(
                        &removed_plan,
                        &removed_plan.steps()[0]
                    ),
                    ProbeContext::StartupReconciliation
                )
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
        let artifact_plan = context_plan(&base_plan, artifact.clone());
        assert_eq!(
            ProductionBackend::new(root.to_owned())
                .probe(
                    &crate::execution::StepExecutionContext::new(
                        &artifact_plan,
                        &artifact_plan.steps()[0]
                    ),
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
                    &crate::execution::StepExecutionContext::new(
                        &artifact_plan,
                        &artifact_plan.steps()[0]
                    ),
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
        let run_plan = context_plan(&base_plan, run.clone());
        assert_eq!(
            ProductionBackend::new(temp.path().to_owned()).probe_capability(
                &crate::execution::StepExecutionContext::new(&run_plan, &run_plan.steps()[0]),
            ),
            ProbeCapability::UnknownAfterCrash
        );
        assert_eq!(
            ProductionBackend::new(temp.path().to_owned())
                .probe(
                    &crate::execution::StepExecutionContext::new(&run_plan, &run_plan.steps()[0]),
                    ProbeContext::StartupReconciliation,
                )
                .unwrap(),
            ProbeVerdict::Unknown
        );
        let context = crate::execution::StepExecutionContext::new(&run_plan, &run_plan.steps()[0]);
        assert_eq!(
            ProductionBackend::new(temp.path().to_owned())
                .probe(
                    &context,
                    ProbeContext::AfterAttempt {
                        executor_succeeded: true,
                    },
                )
                .unwrap(),
            ProbeVerdict::Applied
        );
        assert_eq!(
            ProductionBackend::new(temp.path().to_owned())
                .probe(
                    &context,
                    ProbeContext::AfterAttempt {
                        executor_succeeded: false,
                    },
                )
                .unwrap(),
            ProbeVerdict::Unknown
        );
    }

    #[test]
    fn application_outcome_mapping_is_exhaustive_and_truthful() {
        let operation_id = crate::planner::new_operation_id();
        let step_id = StepId::new("mapping-step").unwrap();
        let outcomes = [
            (
                ExecutionOutcome::Applied { operation_id },
                ExecutionOutcomeKind::Applied,
                true,
            ),
            (
                ExecutionOutcome::AlreadyApplied { operation_id },
                ExecutionOutcomeKind::AlreadyApplied,
                true,
            ),
            (
                ExecutionOutcome::PreflightRefused {
                    operation_id,
                    condition: crate::lifecycle::Precondition::BareRepositoryFalse,
                },
                ExecutionOutcomeKind::PreflightRefused,
                false,
            ),
            (
                ExecutionOutcome::Paused {
                    operation_id,
                    step_id: step_id.clone(),
                    condition: crate::lifecycle::Precondition::BareRepositoryFalse,
                },
                ExecutionOutcomeKind::Paused,
                false,
            ),
            (
                ExecutionOutcome::NeedsAttention {
                    operation_id,
                    step_id: step_id.clone(),
                },
                ExecutionOutcomeKind::NeedsAttention,
                false,
            ),
            (
                ExecutionOutcome::ExistingOperation {
                    operation_id,
                    status: crate::journal::OperationStatus::Running,
                },
                ExecutionOutcomeKind::ExistingOperation,
                false,
            ),
        ];
        for (outcome, kind, success) in outcomes {
            let result = map_execution_outcome(outcome);
            assert_eq!(result.outcome, kind);
            assert_eq!(result.is_success(), success);
            assert_eq!(result.operation_id, operation_id);
        }
    }

    #[test]
    fn application_error_mapping_is_exhaustive_and_stable() {
        let consent = crate::lifecycle::ConsentId::new("test-consent").unwrap();
        let errors = [
            (
                ExecutionError::Backend(ProductionBackendError::TaskExecutionFailed),
                "backend_error",
                "execution backend failed",
            ),
            (
                ExecutionError::Journal(crate::journal_store::JournalError::NotFound),
                "journal_io",
                "journal I/O failed",
            ),
            (
                ExecutionError::UnsupportedPlan("secret".into()),
                "unsupported_plan",
                "execution plan is unsupported",
            ),
            (
                ExecutionError::MissingConsent(consent),
                "missing_consent",
                "required execution consent is missing",
            ),
            (
                ExecutionError::<ProductionBackendError>::RepositoryIdentityMismatch,
                "repository_identity_mismatch",
                "repository identity does not match plan",
            ),
            (
                ExecutionError::<ProductionBackendError>::ImmutableCollision,
                "immutable_collision",
                "operation identity collides with another plan",
            ),
        ];
        for (error, code, message) in errors {
            let mapped = map_execution_error(error);
            assert_eq!((mapped.code, mapped.message), (code, message));
        }
        let journal_errors = [
            (
                crate::journal_store::JournalError::RepositoryBusy,
                "repository_busy",
            ),
            (crate::journal_store::JournalError::NotFound, "journal_io"),
            (
                crate::journal_store::JournalError::Corrupt("secret".into()),
                "journal_corrupt",
            ),
            (
                crate::journal_store::JournalError::InvalidId,
                "journal_corrupt",
            ),
            (
                crate::journal_store::JournalError::RevisionConflict,
                "journal_corrupt",
            ),
            (
                crate::journal_store::JournalError::ImmutableMismatch,
                "journal_corrupt",
            ),
            (
                crate::journal_store::JournalError::InvalidTransition,
                "journal_corrupt",
            ),
            (
                crate::journal_store::JournalError::Io(std::io::Error::other("secret")),
                "journal_io",
            ),
        ];
        for (error, code) in journal_errors {
            assert_eq!(
                map_execution_error::<ProductionBackendError>(ExecutionError::Journal(error)).code,
                code
            );
        }
    }

    #[test]
    fn signal_scope_can_cancel_the_backend_token_before_execution() {
        struct CancellingScope(std::cell::Cell<bool>);
        impl SignalScope for CancellingScope {
            type Guard = ();
            fn install(&self, token: &CancellationToken) -> Result<Self::Guard, ApplyError> {
                token.cancel();
                self.0.set(true);
                Ok(())
            }
        }

        let backend = ProductionBackend::new(PathBuf::from("/unused"));
        let token = backend.cancellation_token();
        let scope = CancellingScope(std::cell::Cell::new(false));
        let _ = scope.install(&token);
        assert!(scope.0.get());
    }
}
