//! Immutable, read-only compensation proposals.

use crate::{
    domain::{StoredPath, WorktreeClass},
    journal::{Journal, OperationStatus, StepStatus},
    lifecycle::{
        ArtifactStateV3, BranchName, Compensation, CreatedArtifactV3, CreatedLocalBranch,
        CreatedWorktree, ObjectId, OperationId, OperationPlan, RepositoryIdentity, StepId,
    },
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt, path::Path, str::FromStr};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompensationAllowanceV1 {
    FileArtifact,
    Worktree,
    LocalBranch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(value))
        } else {
            Err("digest must be 64 lowercase hexadecimal characters".into())
        }
    }

    fn trusted(value: String) -> Self {
        debug_assert_eq!(value.len(), 64);
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Sha256Digest {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProposalId(Uuid);

impl ProposalId {
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl FromStr for ProposalId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let uuid = Uuid::parse_str(value).map_err(|_| "invalid proposal id".to_owned())?;
        if uuid.get_version_num() != 4 || value != uuid.hyphenated().to_string() {
            return Err("proposal id must be canonical lowercase hyphenated UUID v4".into());
        }
        Ok(Self(uuid))
    }
}

impl Serialize for ProposalId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ProposalId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_str(&String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

impl fmt::Display for ProposalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.hyphenated().to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompensationProposalSourceV1 {
    pub operation_id: OperationId,
    pub plan_schema_version: u8,
    pub journal_schema_version: u8,
    pub journal_revision: u64,
    pub forward_plan_digest: Sha256Digest,
    pub forward_journal_digest: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompensationProposalStepV1 {
    pub forward_step_id: StepId,
    pub action: CompensationActionV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "descriptor", rename_all = "snake_case")]
pub enum CompensationActionV1 {
    RemoveCreatedArtifactV3(CreatedArtifactV3),
    RemoveCreatedWorktree(CreatedWorktree),
    DeleteCreatedLocalBranch(CreatedLocalBranch),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompensationProposalV1 {
    pub proposal_schema_version: u8,
    pub proposal_id: ProposalId,
    pub executable: bool,
    pub repository: RepositoryIdentity,
    pub source: CompensationProposalSourceV1,
    pub allowed_categories: Vec<CompensationAllowanceV1>,
    pub steps: Vec<CompensationProposalStepV1>,
}

impl CompensationProposalV1 {
    pub fn validate(&self) -> Result<(), String> {
        if self.proposal_schema_version != 1 || self.executable {
            return Err("invalid proposal header".into());
        }
        ProposalId::from_str(&self.proposal_id.to_string())?;
        Sha256Digest::new(self.source.forward_plan_digest.as_str())?;
        Sha256Digest::new(self.source.forward_journal_digest.as_str())?;
        if self.steps.is_empty() {
            return Err("proposal must contain steps".into());
        }
        if self
            .allowed_categories
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err("allowances must be sorted and unique".into());
        }

        let mut categories = BTreeSet::new();
        let mut worktree = None;
        let mut artifacts = Vec::new();
        let mut staging = Vec::new();
        let mut branches = BTreeSet::new();
        let mut source_ids = BTreeSet::new();

        for (index, step) in self.steps.iter().enumerate() {
            let source_id = step.forward_step_id.clone();
            if !source_ids.insert(source_id.clone()) {
                let exact_expansion = index > 0
                    && matches!(
                        self.steps[index - 1].action,
                        CompensationActionV1::RemoveCreatedWorktree(_)
                    )
                    && matches!(
                        step.action,
                        CompensationActionV1::DeleteCreatedLocalBranch(_)
                    )
                    && self.steps[index - 1].forward_step_id == source_id;
                if !exact_expansion {
                    return Err("source step actions are not canonical".into());
                }
            }
            match &step.action {
                CompensationActionV1::RemoveCreatedArtifactV3(value) => {
                    categories.insert(CompensationAllowanceV1::FileArtifact);
                    artifacts.push(value.path.as_path());
                    if let Some(value) = &value.staging {
                        staging.push(value.path.as_path());
                    }
                }
                CompensationActionV1::RemoveCreatedWorktree(value) => {
                    categories.insert(CompensationAllowanceV1::Worktree);
                    if worktree.replace((index, value)).is_some() {
                        return Err("proposal must contain exactly one worktree".into());
                    }
                }
                CompensationActionV1::DeleteCreatedLocalBranch(value) => {
                    categories.insert(CompensationAllowanceV1::LocalBranch);
                    if !branches.insert((&value.branch, &value.expected_oid)) {
                        return Err("local branch targets must be unique".into());
                    }
                }
            }
        }

        let (worktree_index, worktree) = worktree.ok_or("proposal must contain one worktree")?;
        let derived = self
            .steps
            .iter()
            .filter(|step| {
                matches!(
                    step.action,
                    CompensationActionV1::DeleteCreatedLocalBranch(_)
                )
            })
            .collect::<Vec<_>>();
        if worktree.branch_was_created {
            if derived.len() != 1 || worktree_index + 1 >= self.steps.len() {
                return Err("created worktree branch expansion is missing".into());
            }
            let next = &self.steps[worktree_index + 1];
            match &next.action {
                CompensationActionV1::DeleteCreatedLocalBranch(branch)
                    if next.forward_step_id == self.steps[worktree_index].forward_step_id
                        && branch.branch == worktree.branch
                        && branch.expected_oid == worktree.expected_oid => {}
                _ => return Err("created branch expansion is not adjacent or exact".into()),
            }
        } else if !derived.is_empty() {
            return Err("unrelated local branch compensation".into());
        }
        if worktree_index + 1 < self.steps.len()
            && matches!(
                self.steps[worktree_index + 1].action,
                CompensationActionV1::DeleteCreatedLocalBranch(_)
            )
            && !worktree.branch_was_created
        {
            return Err("branch action must derive from created worktree".into());
        }

        let descendant = |child: &Path, parent: &Path| child != parent && child.starts_with(parent);
        if artifacts.iter().enumerate().any(|(i, left)| {
            artifacts.iter().enumerate().any(|(j, right)| {
                i != j && (left == right || descendant(left, right) || descendant(right, left))
            })
        }) || staging.iter().enumerate().any(|(i, left)| {
            staging
                .iter()
                .enumerate()
                .any(|(j, right)| i != j && left == right)
        }) || staging
            .iter()
            .any(|path| artifacts.iter().any(|final_path| path == final_path))
        {
            return Err("compensation paths overlap".into());
        }
        if artifacts
            .iter()
            .any(|path| !descendant(path, worktree.path.as_path()))
            || staging
                .iter()
                .any(|path| !descendant(path, worktree.path.as_path()))
        {
            return Err("artifact paths must be strict worktree descendants".into());
        }

        if categories.into_iter().collect::<Vec<_>>() != self.allowed_categories {
            return Err("allowances do not match actions".into());
        }
        Ok(())
    }
}

fn digest(domain: &[u8], bytes: &[u8]) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    Sha256Digest::trusted(format!("{:x}", hasher.finalize()))
}

pub fn forward_plan_digest(plan: &OperationPlan) -> Result<Sha256Digest, String> {
    Ok(digest(
        b"ewtm:forward-plan:v1\0",
        &serde_json::to_vec(plan).map_err(|error| error.to_string())?,
    ))
}

pub fn forward_journal_digest(raw: &[u8]) -> Sha256Digest {
    digest(b"ewtm:forward-journal:v1\0", raw)
}

/// Re-checks every immutable fact used to create a proposal.  This is pure: it
/// performs no observation or persistence and is intended to sit inside the
/// repository-lock guard used by a later executor.
pub fn revalidate_proposal(
    proposal: &CompensationProposalV1,
    repository: &RepositoryIdentity,
    raw_journal: &[u8],
) -> Result<(), CompensationError> {
    let value: serde_json::Value =
        serde_json::from_slice(raw_journal).map_err(|_| CompensationError::JournalCorrupt)?;
    if !value.is_object() || crate::compensation_authority::has_duplicate_keys(raw_journal) {
        return Err(CompensationError::JournalCorrupt);
    }
    let journal: Journal =
        serde_json::from_slice(raw_journal).map_err(|_| CompensationError::JournalCorrupt)?;
    let canonical =
        serde_json::to_value(&journal).map_err(|_| CompensationError::JournalCorrupt)?;
    if canonical != value {
        return Err(CompensationError::JournalCorrupt);
    }
    proposal
        .validate()
        .map_err(|_| CompensationError::JournalCorrupt)?;
    if proposal.repository != *repository
        || journal.status() != OperationStatus::Applied
        || journal.operation_id() != &proposal.source.operation_id
        || journal.plan().repository() != &proposal.repository
        || journal.plan().plan_schema_version() != proposal.source.plan_schema_version
        || journal.schema_version() != proposal.source.journal_schema_version
        || journal.revision() != proposal.source.journal_revision
        || journal.steps().len() != journal.plan().steps().len()
        || journal
            .steps()
            .iter()
            .any(|s| s.status() != StepStatus::Applied)
        || !matches!(
            journal.plan().intent(),
            crate::lifecycle::OperationIntent::Create(_)
        )
    {
        return Err(CompensationError::StateChanged);
    }
    journal
        .plan()
        .validate_executable_plan()
        .map_err(|_| CompensationError::StateChanged)?;
    let plan_digest =
        forward_plan_digest(journal.plan()).map_err(|_| CompensationError::Internal)?;
    if plan_digest != proposal.source.forward_plan_digest
        || forward_journal_digest(raw_journal) != proposal.source.forward_journal_digest
    {
        return Err(CompensationError::StateChanged);
    }
    let steps = reverse_map(&journal)?;
    let mut allows = categories(&steps).into_iter().collect::<Vec<_>>();
    allows.sort();
    let regenerated = CompensationProposalV1 {
        proposal_schema_version: 1,
        proposal_id: proposal.proposal_id,
        executable: false,
        repository: repository.clone(),
        source: proposal.source.clone(),
        allowed_categories: allows,
        steps,
    };
    if regenerated != *proposal {
        return Err(CompensationError::StateChanged);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedArtifactState {
    Absent,
    Regular {
        bytes: u64,
        digest: ObjectId,
        mode: u32,
    },
    Symlink {
        target: StoredPath,
        target_digest: ObjectId,
    },
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedWorktree {
    pub path: StoredPath,
    pub branch: BranchName,
    pub head_oid: ObjectId,
    pub classification: WorktreeClass,
}

pub trait CompensationObservationPort {
    type Error;

    fn discover_repository(&self, anchor: &Path) -> Result<RepositoryIdentity, Self::Error>;
    fn observe_artifact(
        &self,
        path: &Path,
        expected: &ArtifactStateV3,
    ) -> Result<ObservedArtifactState, Self::Error>;
    fn observe_absence(&self, path: &Path) -> Result<bool, Self::Error>;
    fn observe_worktree(
        &self,
        anchor: &Path,
        path: &Path,
    ) -> Result<Option<ObservedWorktree>, Self::Error>;
    fn observe_local_ref(
        &self,
        anchor: &Path,
        branch: &BranchName,
    ) -> Result<Option<ObjectId>, Self::Error>;
}

pub trait LockedForwardEvidence {
    fn journal(&self) -> &Journal;
    fn raw_bytes(&self) -> &[u8];
}

pub trait ForwardEvidencePort {
    type Guard: LockedForwardEvidence;

    fn acquire(
        &self,
        common_dir: &Path,
        id: &OperationId,
    ) -> Result<Self::Guard, CompensationError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompensationError {
    ForwardOperationNotFound,
    RepositoryBusy,
    JournalCorrupt,
    RepositoryIdentityMismatch,
    PlatformUnsupported,
    Repository,
    SourceNotApplied,
    SourceNotCreate,
    UnsupportedStep,
    Overlap,
    MissingAllow,
    UnrelatedAllow,
    StateChanged,
    ObservationFailed,
    Internal,
}

impl CompensationError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::ForwardOperationNotFound => "forward_operation_not_found",
            Self::RepositoryBusy => "repository_busy",
            Self::JournalCorrupt => "journal_corrupt",
            Self::RepositoryIdentityMismatch => "repository_identity_mismatch",
            Self::PlatformUnsupported => "compensation_platform_unsupported",
            Self::Repository => "compensation_repository_error",
            Self::SourceNotApplied => "compensation_source_not_applied",
            Self::SourceNotCreate => "compensation_source_not_create",
            Self::UnsupportedStep => "compensation_unsupported_forward_step",
            Self::Overlap => "compensation_overlap",
            Self::MissingAllow => "compensation_missing_allow",
            Self::UnrelatedAllow => "compensation_unrelated_allow",
            Self::StateChanged => "compensation_state_changed",
            Self::ObservationFailed => "compensation_observation_failed",
            Self::Internal => "compensation_internal",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::ForwardOperationNotFound => "forward operation was not found",
            Self::RepositoryBusy => "repository is busy",
            Self::JournalCorrupt => "journal is corrupt",
            Self::RepositoryIdentityMismatch => "repository identity mismatch",
            Self::PlatformUnsupported => "compensation is unsupported on this platform",
            Self::Repository => "compensation repository error",
            Self::SourceNotApplied => "compensation source was not applied",
            Self::SourceNotCreate => "compensation source was not a create operation",
            Self::UnsupportedStep => "forward step is unsupported for compensation",
            Self::Overlap => "compensation paths overlap",
            Self::MissingAllow => "required compensation allowance is missing",
            Self::UnrelatedAllow => "compensation allowance is unrelated",
            Self::StateChanged => "compensation state changed",
            Self::ObservationFailed => "compensation observation failed",
            Self::Internal => "compensation internal error",
        }
    }
}

impl fmt::Display for CompensationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for CompensationError {}

pub struct CompensationProposalService<F, O, G> {
    pub evidence: F,
    pub observer: O,
    pub next_id: G,
}

impl<F, O, G> CompensationProposalService<F, O, G>
where
    F: ForwardEvidencePort,
    O: CompensationObservationPort,
    G: Fn() -> ProposalId,
{
    pub fn propose(
        &self,
        anchor: &Path,
        operation_id: &OperationId,
        allows: &[CompensationAllowanceV1],
    ) -> Result<CompensationProposalV1, CompensationError> {
        let boot = self
            .observer
            .discover_repository(anchor)
            .map_err(|_| CompensationError::Repository)?;
        let guard = self
            .evidence
            .acquire(boot.common_dir.as_path(), operation_id)?;
        let journal = guard.journal();
        if journal.operation_id() != operation_id {
            return Err(CompensationError::JournalCorrupt);
        }
        journal
            .validate()
            .map_err(|_| CompensationError::JournalCorrupt)?;
        if journal.status() != OperationStatus::Applied {
            return Err(CompensationError::SourceNotApplied);
        }
        if !matches!(
            journal.plan().intent(),
            crate::lifecycle::OperationIntent::Create(_)
        ) {
            return Err(CompensationError::SourceNotCreate);
        }
        if journal.steps().len() != journal.plan().steps().len()
            || journal
                .steps()
                .iter()
                .any(|step| step.status() != StepStatus::Applied)
        {
            return Err(CompensationError::SourceNotApplied);
        }
        journal
            .plan()
            .validate_executable_plan()
            .map_err(|_| CompensationError::JournalCorrupt)?;
        let current = self
            .observer
            .discover_repository(anchor)
            .map_err(|_| CompensationError::Repository)?;
        if current != boot || current != *journal.plan().repository() {
            return Err(CompensationError::RepositoryIdentityMismatch);
        }

        let steps = reverse_map(journal)?;
        validate_steps_structurally(&steps).map_err(|_| CompensationError::Overlap)?;
        let categories = categories(&steps);
        let given = allows.iter().copied().collect::<BTreeSet<_>>();
        if !categories.is_subset(&given) {
            return Err(CompensationError::MissingAllow);
        }
        if !given.is_subset(&categories) {
            return Err(CompensationError::UnrelatedAllow);
        }
        observe(&self.observer, anchor, &steps)?;

        let mut allowed_categories = categories.into_iter().collect::<Vec<_>>();
        allowed_categories.sort();
        let source = CompensationProposalSourceV1 {
            operation_id: *operation_id,
            plan_schema_version: journal.plan().plan_schema_version(),
            journal_schema_version: journal.schema_version(),
            journal_revision: journal.revision(),
            forward_plan_digest: forward_plan_digest(journal.plan())
                .map_err(|_| CompensationError::Internal)?,
            forward_journal_digest: forward_journal_digest(guard.raw_bytes()),
        };
        let proposal = CompensationProposalV1 {
            proposal_schema_version: 1,
            proposal_id: ProposalId::from_str("00000000-0000-4000-8000-000000000000")
                .map_err(|_| CompensationError::Internal)?,
            executable: false,
            repository: current,
            source,
            allowed_categories,
            steps,
        };
        proposal
            .validate()
            .map_err(|_| CompensationError::Internal)?;
        let proposal_id = (self.next_id)();
        Ok(CompensationProposalV1 {
            proposal_id,
            ..proposal
        })
    }
}

fn validate_steps_structurally(steps: &[CompensationProposalStepV1]) -> Result<(), ()> {
    let mut worktree: Option<(usize, &CreatedWorktree)> = None;
    let mut source_ids = BTreeSet::new();
    let mut artifacts = Vec::new();
    let mut staging = Vec::new();
    let mut branches = BTreeSet::new();
    for (index, step) in steps.iter().enumerate() {
        if !source_ids.insert(step.forward_step_id.clone()) {
            let adjacent = index > 0
                && matches!(
                    steps[index - 1].action,
                    CompensationActionV1::RemoveCreatedWorktree(_)
                )
                && matches!(
                    step.action,
                    CompensationActionV1::DeleteCreatedLocalBranch(_)
                )
                && steps[index - 1].forward_step_id == step.forward_step_id;
            if !adjacent {
                return Err(());
            }
        }
        match &step.action {
            CompensationActionV1::RemoveCreatedArtifactV3(value) => {
                artifacts.push(value.path.as_path());
                if let Some(value) = &value.staging {
                    staging.push(value.path.as_path());
                }
            }
            CompensationActionV1::RemoveCreatedWorktree(value) => {
                if worktree.replace((index, value)).is_some() {
                    return Err(());
                }
            }
            CompensationActionV1::DeleteCreatedLocalBranch(value) => {
                if !branches.insert((&value.branch, &value.expected_oid)) {
                    return Err(());
                }
            }
        }
    }
    let (worktree_index, worktree) = worktree.ok_or(())?;
    let derived = steps
        .iter()
        .filter(|step| {
            matches!(
                step.action,
                CompensationActionV1::DeleteCreatedLocalBranch(_)
            )
        })
        .count();
    if worktree.branch_was_created {
        if derived != 1 || worktree_index + 1 >= steps.len() {
            return Err(());
        }
        match &steps[worktree_index + 1].action {
            CompensationActionV1::DeleteCreatedLocalBranch(value)
                if steps[worktree_index + 1].forward_step_id
                    == steps[worktree_index].forward_step_id
                    && value.branch == worktree.branch
                    && value.expected_oid == worktree.expected_oid => {}
            _ => return Err(()),
        }
    } else if derived != 0 {
        return Err(());
    }
    let descendant = |child: &Path, parent: &Path| child != parent && child.starts_with(parent);
    if artifacts.iter().enumerate().any(|(i, left)| {
        artifacts.iter().enumerate().any(|(j, right)| {
            i != j && (left == right || descendant(left, right) || descendant(right, left))
        })
    }) || staging.iter().enumerate().any(|(i, left)| {
        staging
            .iter()
            .enumerate()
            .any(|(j, right)| i != j && left == right)
    }) || staging
        .iter()
        .any(|path| artifacts.iter().any(|final_path| path == final_path))
        || artifacts
            .iter()
            .any(|path| !descendant(path, worktree.path.as_path()))
        || staging
            .iter()
            .any(|path| !descendant(path, worktree.path.as_path()))
    {
        return Err(());
    }
    Ok(())
}

fn categories(steps: &[CompensationProposalStepV1]) -> BTreeSet<CompensationAllowanceV1> {
    steps
        .iter()
        .map(|step| match step.action {
            CompensationActionV1::RemoveCreatedArtifactV3(_) => {
                CompensationAllowanceV1::FileArtifact
            }
            CompensationActionV1::RemoveCreatedWorktree(_) => CompensationAllowanceV1::Worktree,
            CompensationActionV1::DeleteCreatedLocalBranch(_) => {
                CompensationAllowanceV1::LocalBranch
            }
        })
        .collect()
}

fn reverse_map(journal: &Journal) -> Result<Vec<CompensationProposalStepV1>, CompensationError> {
    let mut steps = Vec::new();
    for index in (0..journal.plan().steps().len()).rev() {
        let plan_step = &journal.plan().steps()[index];
        if plan_step.irreversible() {
            return Err(CompensationError::UnsupportedStep);
        }
        match plan_step.compensation() {
            Some(Compensation::RemoveCreatedArtifactV3(value)) => {
                steps.push(CompensationProposalStepV1 {
                    forward_step_id: plan_step.id().clone(),
                    action: CompensationActionV1::RemoveCreatedArtifactV3(value.clone()),
                })
            }
            Some(Compensation::RemoveCreatedWorktree(value)) => {
                steps.push(CompensationProposalStepV1 {
                    forward_step_id: plan_step.id().clone(),
                    action: CompensationActionV1::RemoveCreatedWorktree(value.clone()),
                });
                if value.branch_was_created {
                    steps.push(CompensationProposalStepV1 {
                        forward_step_id: plan_step.id().clone(),
                        action: CompensationActionV1::DeleteCreatedLocalBranch(
                            CreatedLocalBranch {
                                branch: value.branch.clone(),
                                expected_oid: value.expected_oid.clone(),
                            },
                        ),
                    });
                }
            }
            _ => return Err(CompensationError::UnsupportedStep),
        }
    }
    Ok(steps)
}

fn observe<O: CompensationObservationPort>(
    observer: &O,
    anchor: &Path,
    steps: &[CompensationProposalStepV1],
) -> Result<(), CompensationError> {
    for step in steps {
        match &step.action {
            CompensationActionV1::RemoveCreatedArtifactV3(value) => {
                let observed = observer
                    .observe_artifact(value.path.as_path(), &value.expected)
                    .map_err(|_| CompensationError::ObservationFailed)?;
                let equal = match (&observed, &value.expected) {
                    (
                        ObservedArtifactState::Regular {
                            bytes,
                            digest,
                            mode,
                        },
                        ArtifactStateV3::Regular(expected),
                    ) => {
                        *bytes == expected.bytes
                            && digest == &expected.digest
                            && *mode == expected.mode
                    }
                    (
                        ObservedArtifactState::Symlink {
                            target,
                            target_digest,
                        },
                        ArtifactStateV3::Symlink(expected),
                    ) => target == &expected.target && target_digest == &expected.target_digest,
                    _ => false,
                };
                if !equal {
                    return Err(CompensationError::StateChanged);
                }
                if let Some(staging) = &value.staging
                    && !observer
                        .observe_absence(staging.path.as_path())
                        .map_err(|_| CompensationError::ObservationFailed)?
                {
                    return Err(CompensationError::StateChanged);
                }
            }
            CompensationActionV1::RemoveCreatedWorktree(value) => {
                let observed = observer
                    .observe_worktree(anchor, value.path.as_path())
                    .map_err(|_| CompensationError::ObservationFailed)?
                    .ok_or(CompensationError::StateChanged)?;
                if observed.path != value.path
                    || observed.branch != value.branch
                    || observed.head_oid != value.expected_oid
                    || observed.classification != WorktreeClass::Linked
                {
                    return Err(CompensationError::StateChanged);
                }
            }
            CompensationActionV1::DeleteCreatedLocalBranch(value) => {
                if observer
                    .observe_local_ref(anchor, &value.branch)
                    .map_err(|_| CompensationError::ObservationFailed)?
                    != Some(value.expected_oid.clone())
                {
                    return Err(CompensationError::StateChanged);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{ArtifactStateV3, RegularFileStateV3};

    fn path(value: &str) -> StoredPath {
        StoredPath::new(Path::new(value).to_path_buf())
    }

    fn oid() -> ObjectId {
        ObjectId::new("0123456789012345678901234567890123456789").unwrap()
    }

    fn repository() -> RepositoryIdentity {
        RepositoryIdentity {
            common_dir: path("/repo/.git"),
            primary_root: path("/repo"),
            repository_oid: oid(),
        }
    }

    fn worktree(branch_was_created: bool) -> CreatedWorktree {
        CreatedWorktree {
            path: path("/repo/worktree"),
            branch: BranchName::new("feature").unwrap(),
            expected_oid: oid(),
            branch_was_created,
        }
    }

    fn step(id: &str, action: CompensationActionV1) -> CompensationProposalStepV1 {
        CompensationProposalStepV1 {
            forward_step_id: StepId::new(id.to_owned()).unwrap(),
            action,
        }
    }

    fn proposal(steps: Vec<CompensationProposalStepV1>) -> CompensationProposalV1 {
        let digest = Sha256Digest::new("a".repeat(64)).unwrap();
        CompensationProposalV1 {
            proposal_schema_version: 1,
            proposal_id: ProposalId::from_str("00000000-0000-4000-8000-000000000000").unwrap(),
            executable: false,
            repository: repository(),
            source: CompensationProposalSourceV1 {
                operation_id: OperationId::new(Uuid::new_v4()),
                plan_schema_version: 3,
                journal_schema_version: 1,
                journal_revision: 2,
                forward_plan_digest: digest.clone(),
                forward_journal_digest: digest,
            },
            allowed_categories: vec![
                CompensationAllowanceV1::FileArtifact,
                CompensationAllowanceV1::Worktree,
            ],
            steps,
        }
    }

    fn artifact() -> CreatedArtifactV3 {
        CreatedArtifactV3 {
            path: path("/repo/worktree/result.txt"),
            expected: ArtifactStateV3::Regular(RegularFileStateV3 {
                bytes: 3,
                digest: oid(),
                mode: 0o644,
            }),
            staging: None,
        }
    }

    #[test]
    fn digest_accepts_only_lowercase_sha256() {
        assert!(Sha256Digest::new("0".repeat(64)).is_ok());
        assert!(Sha256Digest::new("A".repeat(64)).is_err());
        assert!(serde_json::from_str::<Sha256Digest>(&format!("\"{}\"", "g".repeat(64))).is_err());
    }

    #[test]
    fn proposal_id_requires_canonical_v4() {
        assert!(ProposalId::from_str("00000000-0000-4000-8000-000000000000").is_ok());
        assert!(ProposalId::from_str("00000000-0000-4000-8000-00000000000A").is_err());
        assert!(ProposalId::from_str("00000000-0000-3000-8000-000000000000").is_err());
    }

    #[test]
    fn proposal_wire_is_explicit_and_excludes_consent_fields() {
        let value = serde_json::to_value(proposal(vec![
            step(
                "artifact",
                CompensationActionV1::RemoveCreatedArtifactV3(artifact()),
            ),
            step(
                "worktree",
                CompensationActionV1::RemoveCreatedWorktree(worktree(false)),
            ),
        ]))
        .unwrap();
        assert_eq!(
            value["steps"][0]["action"]["kind"],
            "remove_created_artifact_v3"
        );
        assert_eq!(
            serde_json::to_string(&value["steps"][0]["action"]).unwrap(),
            r#"{"descriptor":{"expected":{"Regular":{"bytes":3,"digest":"0123456789012345678901234567890123456789","mode":420}},"path":"/repo/worktree/result.txt","staging":null},"kind":"remove_created_artifact_v3"}"#
        );
        assert!(value.get("risks").is_none());
        assert!(value.get("blockers").is_none());
    }

    #[test]
    fn proposal_requires_exact_worktree_and_descendant_artifacts() {
        let mut value = proposal(vec![step(
            "worktree",
            CompensationActionV1::RemoveCreatedWorktree(worktree(false)),
        )]);
        value.allowed_categories = vec![CompensationAllowanceV1::Worktree];
        assert!(value.validate().is_ok());
        let mut bad = value.clone();
        bad.steps.push(step(
            "other",
            CompensationActionV1::RemoveCreatedWorktree(worktree(false)),
        ));
        assert!(bad.validate().is_err());
    }

    #[test]
    fn proposal_requires_adjacent_derived_branch() {
        let mut value = proposal(vec![
            step(
                "artifact",
                CompensationActionV1::RemoveCreatedArtifactV3(artifact()),
            ),
            step(
                "worktree",
                CompensationActionV1::RemoveCreatedWorktree(worktree(true)),
            ),
            step(
                "worktree",
                CompensationActionV1::DeleteCreatedLocalBranch(CreatedLocalBranch {
                    branch: BranchName::new("feature").unwrap(),
                    expected_oid: oid(),
                }),
            ),
        ]);
        value
            .allowed_categories
            .push(CompensationAllowanceV1::LocalBranch);
        assert!(value.validate().is_ok());
        value.steps.swap(1, 2);
        assert!(value.validate().is_err());
    }

    #[test]
    fn proposal_rejects_artifact_overlap_and_staging_collision() {
        let mut first = artifact();
        first.staging = Some(crate::lifecycle::OwnedStagingV3 {
            path: path("/repo/worktree/tmp"),
            ownership_token: oid(),
        });
        let mut second = artifact();
        second.path = path("/repo/worktree/result.txt/child");
        let value = proposal(vec![
            step("one", CompensationActionV1::RemoveCreatedArtifactV3(first)),
            step("two", CompensationActionV1::RemoveCreatedArtifactV3(second)),
            step(
                "worktree",
                CompensationActionV1::RemoveCreatedWorktree(worktree(false)),
            ),
        ]);
        assert!(value.validate().is_err());
    }

    #[test]
    fn forward_journal_digest_is_whitespace_sensitive() {
        assert_ne!(
            forward_journal_digest(br"{}"),
            forward_journal_digest(br"{ }")
        );
    }

    #[test]
    fn every_error_has_stable_code_and_message() {
        let errors = [
            CompensationError::ForwardOperationNotFound,
            CompensationError::RepositoryBusy,
            CompensationError::JournalCorrupt,
            CompensationError::RepositoryIdentityMismatch,
            CompensationError::PlatformUnsupported,
            CompensationError::Repository,
            CompensationError::SourceNotApplied,
            CompensationError::SourceNotCreate,
            CompensationError::UnsupportedStep,
            CompensationError::Overlap,
            CompensationError::MissingAllow,
            CompensationError::UnrelatedAllow,
            CompensationError::StateChanged,
            CompensationError::ObservationFailed,
            CompensationError::Internal,
        ];
        for error in errors {
            assert!(!error.code().is_empty());
            assert_eq!(error.to_string(), error.message());
        }
    }

    #[test]
    fn proposal_rejects_schema_two() {
        let mut value = proposal(vec![step(
            "worktree",
            CompensationActionV1::RemoveCreatedWorktree(worktree(false)),
        )]);
        value.proposal_schema_version = 2;
        value.allowed_categories = vec![CompensationAllowanceV1::Worktree];
        assert!(value.validate().is_err());
    }

    #[test]
    fn proposal_rejects_executable_true() {
        let mut value = proposal(vec![step(
            "worktree",
            CompensationActionV1::RemoveCreatedWorktree(worktree(false)),
        )]);
        value.executable = true;
        value.allowed_categories = vec![CompensationAllowanceV1::Worktree];
        assert!(value.validate().is_err());
    }

    #[test]
    fn proposal_rejects_empty_steps() {
        let mut value = proposal(Vec::new());
        value.allowed_categories.clear();
        assert!(value.validate().is_err());
    }

    #[test]
    fn proposal_rejects_unsorted_allowances() {
        let mut value = proposal(vec![step(
            "worktree",
            CompensationActionV1::RemoveCreatedWorktree(worktree(false)),
        )]);
        value.allowed_categories = vec![
            CompensationAllowanceV1::Worktree,
            CompensationAllowanceV1::FileArtifact,
        ];
        assert!(value.validate().is_err());
    }

    #[test]
    fn proposal_rejects_duplicate_allowances() {
        let mut value = proposal(vec![step(
            "worktree",
            CompensationActionV1::RemoveCreatedWorktree(worktree(false)),
        )]);
        value.allowed_categories = vec![
            CompensationAllowanceV1::Worktree,
            CompensationAllowanceV1::Worktree,
        ];
        assert!(value.validate().is_err());
    }

    #[test]
    fn proposal_rejects_unrelated_allowance() {
        let mut value = proposal(vec![step(
            "worktree",
            CompensationActionV1::RemoveCreatedWorktree(worktree(false)),
        )]);
        value.allowed_categories = vec![
            CompensationAllowanceV1::FileArtifact,
            CompensationAllowanceV1::Worktree,
        ];
        assert!(value.validate().is_err());
    }

    #[test]
    fn proposal_rejects_duplicate_final_path() {
        let value = proposal(vec![
            step(
                "one",
                CompensationActionV1::RemoveCreatedArtifactV3(artifact()),
            ),
            step(
                "two",
                CompensationActionV1::RemoveCreatedArtifactV3(artifact()),
            ),
            step(
                "worktree",
                CompensationActionV1::RemoveCreatedWorktree(worktree(false)),
            ),
        ]);
        assert!(value.validate().is_err());
    }

    #[test]
    fn proposal_rejects_staging_equal_to_final() {
        let mut value = artifact();
        value.staging = Some(crate::lifecycle::OwnedStagingV3 {
            path: value.path.clone(),
            ownership_token: oid(),
        });
        let proposal = proposal(vec![
            step(
                "artifact",
                CompensationActionV1::RemoveCreatedArtifactV3(value),
            ),
            step(
                "worktree",
                CompensationActionV1::RemoveCreatedWorktree(worktree(false)),
            ),
        ]);
        assert!(proposal.validate().is_err());
    }

    #[test]
    fn proposal_rejects_staging_outside_worktree() {
        let mut value = artifact();
        value.staging = Some(crate::lifecycle::OwnedStagingV3 {
            path: path("/other/tmp"),
            ownership_token: oid(),
        });
        let proposal = proposal(vec![
            step(
                "artifact",
                CompensationActionV1::RemoveCreatedArtifactV3(value),
            ),
            step(
                "worktree",
                CompensationActionV1::RemoveCreatedWorktree(worktree(false)),
            ),
        ]);
        assert!(proposal.validate().is_err());
    }

    #[test]
    fn proposal_rejects_branch_for_non_created_worktree() {
        let proposal = proposal(vec![
            step(
                "worktree",
                CompensationActionV1::RemoveCreatedWorktree(worktree(false)),
            ),
            step(
                "branch",
                CompensationActionV1::DeleteCreatedLocalBranch(CreatedLocalBranch {
                    branch: BranchName::new("feature").unwrap(),
                    expected_oid: oid(),
                }),
            ),
        ]);
        assert!(proposal.validate().is_err());
    }

    #[test]
    fn proposal_rejects_non_adjacent_branch() {
        let proposal = proposal(vec![
            step(
                "worktree",
                CompensationActionV1::RemoveCreatedWorktree(worktree(true)),
            ),
            step(
                "artifact",
                CompensationActionV1::RemoveCreatedArtifactV3(artifact()),
            ),
            step(
                "worktree",
                CompensationActionV1::DeleteCreatedLocalBranch(CreatedLocalBranch {
                    branch: BranchName::new("feature").unwrap(),
                    expected_oid: oid(),
                }),
            ),
        ]);
        assert!(proposal.validate().is_err());
    }

    #[test]
    fn proposal_rejects_wrong_derived_branch_oid() {
        let wrong = ObjectId::new("abcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap();
        let proposal = proposal(vec![
            step(
                "worktree",
                CompensationActionV1::RemoveCreatedWorktree(worktree(true)),
            ),
            step(
                "worktree",
                CompensationActionV1::DeleteCreatedLocalBranch(CreatedLocalBranch {
                    branch: BranchName::new("feature").unwrap(),
                    expected_oid: wrong,
                }),
            ),
        ]);
        assert!(proposal.validate().is_err());
    }

    #[test]
    fn plan_digest_domain_is_distinct_from_journal_domain() {
        let raw = b"same bytes";
        assert_ne!(
            forward_journal_digest(raw).as_str(),
            digest(b"ewtm:forward-plan:v1\0", raw).as_str()
        );
    }

    #[test]
    fn proposal_id_round_trips_wire() {
        let id = ProposalId::new_v4();
        let wire = serde_json::to_string(&id).unwrap();
        let decoded: ProposalId = serde_json::from_str(&wire).unwrap();
        assert_eq!(id, decoded);
    }

    struct TestGuard {
        journal: Journal,
        raw: Vec<u8>,
    }

    impl LockedForwardEvidence for TestGuard {
        fn journal(&self) -> &Journal {
            &self.journal
        }
        fn raw_bytes(&self) -> &[u8] {
            &self.raw
        }
    }

    struct TestEvidence(TestGuard);

    impl ForwardEvidencePort for TestEvidence {
        type Guard = TestGuard;
        fn acquire(&self, _: &Path, _: &OperationId) -> Result<Self::Guard, CompensationError> {
            Ok(TestGuard {
                journal: self.0.journal.clone(),
                raw: self.0.raw.clone(),
            })
        }
    }

    struct TestObserver {
        discoveries: std::cell::Cell<usize>,
        observations: std::cell::Cell<usize>,
        repository: RepositoryIdentity,
    }

    impl CompensationObservationPort for TestObserver {
        type Error = ();
        fn discover_repository(&self, _: &Path) -> Result<RepositoryIdentity, Self::Error> {
            self.discoveries.set(self.discoveries.get() + 1);
            Ok(self.repository.clone())
        }
        fn observe_artifact(
            &self,
            _: &Path,
            _: &ArtifactStateV3,
        ) -> Result<ObservedArtifactState, Self::Error> {
            self.observations.set(self.observations.get() + 1);
            Ok(ObservedArtifactState::Other)
        }
        fn observe_absence(&self, _: &Path) -> Result<bool, Self::Error> {
            self.observations.set(self.observations.get() + 1);
            Ok(true)
        }
        fn observe_worktree(
            &self,
            _: &Path,
            _: &Path,
        ) -> Result<Option<ObservedWorktree>, Self::Error> {
            self.observations.set(self.observations.get() + 1);
            Ok(Some(ObservedWorktree {
                path: StoredPath::new(Path::new("/r/w").to_path_buf()),
                branch: BranchName::new("feature").unwrap(),
                head_oid: ObjectId::new("0000000000000000000000000000000000000000").unwrap(),
                classification: WorktreeClass::Linked,
            }))
        }
        fn observe_local_ref(
            &self,
            _: &Path,
            _: &BranchName,
        ) -> Result<Option<ObjectId>, Self::Error> {
            self.observations.set(self.observations.get() + 1);
            Ok(Some(
                ObjectId::new("0000000000000000000000000000000000000000").unwrap(),
            ))
        }
    }

    fn test_journal() -> Journal {
        Journal::new(crate::lifecycle::test_plan(1))
    }

    fn revalidation_fixture() -> (CompensationProposalV1, RepositoryIdentity, Vec<u8>) {
        let mut journal = test_journal();
        let step_id = journal.steps()[0].id().clone();
        journal.start_step(&step_id).unwrap();
        journal.apply_step(&step_id).unwrap();
        let raw = serde_json::to_vec(&journal).unwrap();
        let repository = journal.plan().repository().clone();
        let service = CompensationProposalService {
            evidence: TestEvidence(TestGuard {
                raw: raw.clone(),
                journal: journal.clone(),
            }),
            observer: TestObserver {
                discoveries: std::cell::Cell::new(0),
                observations: std::cell::Cell::new(0),
                repository: repository.clone(),
            },
            next_id: || ProposalId::from_str("00000000-0000-4000-8000-000000000001").unwrap(),
        };
        let proposal = service
            .propose(
                Path::new("/r"),
                journal.operation_id(),
                &[
                    CompensationAllowanceV1::Worktree,
                    CompensationAllowanceV1::LocalBranch,
                ],
            )
            .unwrap();
        (proposal, repository, raw)
    }

    #[test]
    fn service_refuses_all_non_applied_statuses_before_mapping_observation_or_id() {
        let mut journals = Vec::new();
        let pending = test_journal();
        journals.push(pending.clone());
        let mut running = pending.clone();
        let running_id = running.steps()[0].id().clone();
        running.start_step(&running_id).unwrap();
        journals.push(running.clone());
        let mut attention = running;
        let attention_id = attention.steps()[0].id().clone();
        attention
            .reconcile_step(
                &attention_id,
                crate::journal::Reconciliation::NeedsAttention,
            )
            .unwrap();
        journals.push(attention);
        let mut failed = pending.clone();
        failed.fail_operation().unwrap();
        journals.push(failed);
        for journal in journals {
            let repository = journal.plan().repository().clone();
            let observer = TestObserver {
                discoveries: std::cell::Cell::new(0),
                observations: std::cell::Cell::new(0),
                repository,
            };
            let ids = std::cell::Cell::new(0);
            let service = CompensationProposalService {
                evidence: TestEvidence(TestGuard {
                    raw: serde_json::to_vec(&journal).unwrap(),
                    journal: journal.clone(),
                }),
                observer,
                next_id: || {
                    ids.set(ids.get() + 1);
                    ProposalId::new_v4()
                },
            };
            assert_eq!(
                service
                    .propose(Path::new("/r"), journal.operation_id(), &[])
                    .unwrap_err(),
                CompensationError::SourceNotApplied
            );
            assert_eq!(ids.get(), 0);
            assert_eq!(service.observer.observations.get(), 0);
        }
    }

    #[test]
    fn service_missing_allowance_is_before_observation_and_id() {
        let mut journal = test_journal();
        let step_id = journal.steps()[0].id().clone();
        journal.start_step(&step_id).unwrap();
        journal.apply_step(&step_id).unwrap();
        let repository = journal.plan().repository().clone();
        let observer = TestObserver {
            discoveries: std::cell::Cell::new(0),
            observations: std::cell::Cell::new(0),
            repository,
        };
        let ids = std::cell::Cell::new(0);
        let service = CompensationProposalService {
            evidence: TestEvidence(TestGuard {
                raw: serde_json::to_vec(&journal).unwrap(),
                journal: journal.clone(),
            }),
            observer,
            next_id: || {
                ids.set(ids.get() + 1);
                ProposalId::new_v4()
            },
        };
        assert_eq!(
            service
                .propose(Path::new("/r"), journal.operation_id(), &[])
                .unwrap_err(),
            CompensationError::MissingAllow
        );
        assert_eq!(ids.get(), 0);
        assert_eq!(service.observer.observations.get(), 0);
    }

    #[test]
    fn service_rejects_journal_repository_mismatch_before_mapping_observation_or_id() {
        let mut journal = test_journal();
        let step_id = journal.steps()[0].id().clone();
        journal.start_step(&step_id).unwrap();
        journal.apply_step(&step_id).unwrap();
        let observer = TestObserver {
            discoveries: std::cell::Cell::new(0),
            observations: std::cell::Cell::new(0),
            repository: repository(),
        };
        let ids = std::cell::Cell::new(0);
        let service = CompensationProposalService {
            evidence: TestEvidence(TestGuard {
                raw: serde_json::to_vec(&journal).unwrap(),
                journal: journal.clone(),
            }),
            observer,
            next_id: || {
                ids.set(ids.get() + 1);
                ProposalId::new_v4()
            },
        };
        assert_eq!(
            service
                .propose(
                    Path::new("/r"),
                    journal.operation_id(),
                    &[
                        CompensationAllowanceV1::Worktree,
                        CompensationAllowanceV1::LocalBranch,
                    ],
                )
                .unwrap_err(),
            CompensationError::RepositoryIdentityMismatch
        );
        assert_eq!(service.observer.discoveries.get(), 2);
        assert_eq!(service.observer.observations.get(), 0);
        assert_eq!(ids.get(), 0);
    }

    #[test]
    fn service_success_maps_worktree_then_derived_branch_and_allocates_once() {
        let mut journal = test_journal();
        let step_id = journal.steps()[0].id().clone();
        journal.start_step(&step_id).unwrap();
        journal.apply_step(&step_id).unwrap();
        let repository = journal.plan().repository().clone();
        let observer = TestObserver {
            discoveries: std::cell::Cell::new(0),
            observations: std::cell::Cell::new(0),
            repository,
        };
        let ids = std::cell::Cell::new(0);
        let service = CompensationProposalService {
            evidence: TestEvidence(TestGuard {
                raw: serde_json::to_vec(&journal).unwrap(),
                journal: journal.clone(),
            }),
            observer,
            next_id: || {
                ids.set(ids.get() + 1);
                ProposalId::new_v4()
            },
        };
        let proposal = service
            .propose(
                Path::new("/r"),
                journal.operation_id(),
                &[
                    CompensationAllowanceV1::Worktree,
                    CompensationAllowanceV1::LocalBranch,
                ],
            )
            .unwrap();
        assert_eq!(ids.get(), 1);
        assert_eq!(proposal.steps.len(), 2);
        assert!(matches!(
            proposal.steps[0].action,
            CompensationActionV1::RemoveCreatedWorktree(_)
        ));
        assert!(matches!(
            proposal.steps[1].action,
            CompensationActionV1::DeleteCreatedLocalBranch(_)
        ));
        assert_eq!(
            proposal.allowed_categories,
            vec![
                CompensationAllowanceV1::Worktree,
                CompensationAllowanceV1::LocalBranch
            ]
        );
    }

    #[test]
    fn golden_forward_plan_bytes_and_digest() {
        let mut wire = serde_json::to_value(crate::lifecycle::test_plan(1)).unwrap();
        wire["operation_id"] = serde_json::json!("00000000-0000-4000-8000-000000000001");
        let plan: OperationPlan = serde_json::from_value(wire).unwrap();
        let bytes = serde_json::to_vec(&plan).unwrap();
        println!(
            "golden plan bytes: {}",
            String::from_utf8(bytes.clone()).unwrap()
        );
        println!(
            "golden plan digest: {}",
            forward_plan_digest(&plan).unwrap()
        );
        assert_eq!(
            forward_plan_digest(&plan).unwrap().as_str(),
            "fc527542ace4cc09c3568c0466ec5aaeb19c9acc9c93c324e4a576e8282a3d8c"
        );
        assert!(!bytes.contains(&b'\n'));
        assert!(
            String::from_utf8(bytes)
                .unwrap()
                .contains("\"operation_id\":\"00000000-0000-4000-8000-000000000001\"")
        );
    }

    #[test]
    fn revalidation_binds_exact_raw_forward_journal_and_proposal_shape() {
        let mut journal = test_journal();
        let id = journal.steps()[0].id().clone();
        journal.start_step(&id).unwrap();
        journal.apply_step(&id).unwrap();
        let raw = serde_json::to_vec(&journal).unwrap();
        let repository = journal.plan().repository().clone();
        let observer = TestObserver {
            discoveries: std::cell::Cell::new(0),
            observations: std::cell::Cell::new(0),
            repository: repository.clone(),
        };
        let service = CompensationProposalService {
            evidence: TestEvidence(TestGuard {
                raw: raw.clone(),
                journal: journal.clone(),
            }),
            observer,
            next_id: || ProposalId::from_str("00000000-0000-4000-8000-000000000001").unwrap(),
        };
        let proposal = service
            .propose(
                Path::new("/r"),
                journal.operation_id(),
                &[
                    CompensationAllowanceV1::Worktree,
                    CompensationAllowanceV1::LocalBranch,
                ],
            )
            .unwrap();
        assert!(revalidate_proposal(&proposal, &repository, &raw).is_ok());
        let mut changed = proposal.clone();
        changed.allowed_categories.clear();
        assert_eq!(
            revalidate_proposal(&changed, &repository, &raw),
            Err(CompensationError::JournalCorrupt)
        );
        assert_eq!(
            revalidate_proposal(&proposal, &repository, &[raw.as_slice(), b" "].concat()),
            Err(CompensationError::StateChanged)
        );
        assert_eq!(
            revalidate_proposal(&proposal, &repository, br#"{"revision":1,"revision":1}"#),
            Err(CompensationError::JournalCorrupt)
        );
        assert_eq!(
            revalidate_proposal(&proposal, &repository, &[raw.as_slice(), b"{}"].concat()),
            Err(CompensationError::JournalCorrupt)
        );
        let mut wrong_repository = repository.clone();
        wrong_repository.repository_oid =
            ObjectId::new("1111111111111111111111111111111111111111").unwrap();
        assert_eq!(
            revalidate_proposal(&proposal, &wrong_repository, &raw),
            Err(CompensationError::StateChanged)
        );
    }

    #[test]
    fn revalidation_axes_are_isolated_and_named() {
        let (proposal, repository, raw) = revalidation_fixture();
        let mut repository_drift = repository.clone();
        repository_drift.primary_root = StoredPath::new("/other".into());
        assert!(
            revalidate_proposal(&proposal, &repository_drift, &raw).is_err(),
            "proposal repository mismatch"
        );

        let mut plan_repository = serde_json::from_slice::<serde_json::Value>(&raw).unwrap();
        plan_repository["plan"]["repository"]["primary_root"] = serde_json::json!("/other");
        assert!(
            revalidate_proposal(
                &proposal,
                &repository,
                &serde_json::to_vec(&plan_repository).unwrap()
            )
            .is_err(),
            "forward plan repository mismatch"
        );

        let mut plan_schema = serde_json::from_slice::<serde_json::Value>(&raw).unwrap();
        plan_schema["plan"]["plan_schema_version"] = serde_json::json!(99);
        assert!(
            revalidate_proposal(
                &proposal,
                &repository,
                &serde_json::to_vec(&plan_schema).unwrap()
            )
            .is_err(),
            "plan schema"
        );

        let mut journal_schema = serde_json::from_slice::<serde_json::Value>(&raw).unwrap();
        journal_schema["schema_version"] = serde_json::json!(99);
        assert!(
            revalidate_proposal(
                &proposal,
                &repository,
                &serde_json::to_vec(&journal_schema).unwrap()
            )
            .is_err(),
            "journal schema"
        );
        assert!(
            revalidate_proposal(&proposal, &repository, b"{}").is_err(),
            "journal corruption"
        );

        let mut journal_revision = serde_json::from_slice::<serde_json::Value>(&raw).unwrap();
        journal_revision["revision"] = serde_json::json!(0);
        assert!(
            revalidate_proposal(
                &proposal,
                &repository,
                &serde_json::to_vec(&journal_revision).unwrap()
            )
            .is_err(),
            "journal revision"
        );

        let mut pending = serde_json::from_slice::<serde_json::Value>(&raw).unwrap();
        pending["status"] = serde_json::json!("pending");
        pending["steps"][0]["status"] = serde_json::json!("pending");
        assert!(
            revalidate_proposal(
                &proposal,
                &repository,
                &serde_json::to_vec(&pending).unwrap()
            )
            .is_err(),
            "non-applied operation/step"
        );
        assert!(
            revalidate_proposal(&proposal, &repository, &[raw.as_slice(), b" "].concat()).is_err(),
            "exact raw whitespace drift"
        );

        let mut plan_digest = proposal.clone();
        plan_digest.source.forward_plan_digest = Sha256Digest::new("c".repeat(64)).unwrap();
        assert!(
            revalidate_proposal(&plan_digest, &repository, &raw).is_err(),
            "plan digest"
        );
        let mut journal_digest = proposal.clone();
        journal_digest.source.forward_journal_digest = Sha256Digest::new("d".repeat(64)).unwrap();
        assert!(
            revalidate_proposal(&journal_digest, &repository, &raw).is_err(),
            "journal digest"
        );
        let mut allowance = proposal.clone();
        allowance.allowed_categories.reverse();
        assert!(
            revalidate_proposal(&allowance, &repository, &raw).is_err(),
            "allowance drift"
        );
        let mut action = proposal.clone();
        if let CompensationActionV1::RemoveCreatedWorktree(ref mut worktree) =
            action.steps[0].action
        {
            worktree.path = StoredPath::new("/repo/other".into());
        }
        assert!(
            revalidate_proposal(&action, &repository, &raw).is_err(),
            "action/path drift"
        );
        let mut order = proposal.clone();
        order.steps.reverse();
        assert!(
            revalidate_proposal(&order, &repository, &raw).is_err(),
            "regenerated reverse mismatch"
        );
    }
}
