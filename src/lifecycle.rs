//! Pure, serializable lifecycle vocabulary. No Git or filesystem operations live here.

use crate::domain::StoredPath;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};
use uuid::Uuid;

fn legacy_mode_policy() -> crate::planner::FileModePolicy {
    crate::planner::FileModePolicy::LegacyUnspecified
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OperationId(Uuid);
impl OperationId {
    pub fn new(value: Uuid) -> Self {
        Self(value)
    }
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}
impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for OperationId {
    type Err = uuid::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(value)?))
    }
}

macro_rules! text_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                validate_text(&value).map(|()| Self(value))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl TryFrom<String> for $name {
            type Error = String;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}
text_id!(StepId);
text_id!(ConsentId);
text_id!(BranchName);
text_id!(RefName);
text_id!(RemoteName);
text_id!(EnvironmentName);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct CommandArgv(Vec<String>);
impl CommandArgv {
    pub fn new(value: Vec<String>) -> Result<Self, String> {
        if value.is_empty() || value.iter().any(|arg| arg.is_empty() || arg.contains('\0')) {
            Err("argv must be non-empty and contain no NUL".into())
        } else {
            Ok(Self(value))
        }
    }
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}
impl TryFrom<Vec<String>> for CommandArgv {
    type Error = String;
    fn try_from(value: Vec<String>) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<CommandArgv> for Vec<String> {
    fn from(value: CommandArgv) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ObjectId(String);
impl ObjectId {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if (value.len() == 40 || value.len() == 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
        {
            Ok(Self(value))
        } else {
            Err("object id must be 40 or 64 ASCII hex characters".into())
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for ObjectId {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<ObjectId> for String {
    fn from(value: ObjectId) -> Self {
        value.0
    }
}
fn validate_text(value: &str) -> Result<(), String> {
    if value.is_empty() || value.bytes().any(|b| b == 0 || b.is_ascii_control()) {
        Err("value must be non-empty and contain no NUL/control characters".into())
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryIdentity {
    pub common_dir: StoredPath,
    pub primary_root: StoredPath,
    pub repository_oid: ObjectId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBranch {
    pub remote: RemoteName,
    pub branch: BranchName,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreateSource {
    NewBranch {
        branch: BranchName,
        base: Option<RefName>,
    },
    ExistingLocal {
        branch: BranchName,
    },
    RemoteTracking {
        remote: RemoteName,
        remote_branch: BranchName,
        local_branch: BranchName,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateIntent {
    pub repository: RepositoryIdentity,
    pub source: CreateSource,
    pub destination: Option<StoredPath>,
    pub selected_tasks: BTreeSet<String>,
    pub skipped_rules: BTreeSet<String>,
    pub granted_consents: BTreeSet<ConsentId>,
    #[serde(default)]
    pub task_contracts: BTreeMap<String, TaskContract>,
    #[serde(default)]
    pub current_worktree_root: Option<StoredPath>,
    #[serde(default)]
    pub artifact_rule_contracts: BTreeMap<String, ArtifactRuleContract>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskContract {
    pub argv: CommandArgv,
    pub cwd: StoredPath,
    pub required: bool,
    pub environment_allowlist: Vec<EnvironmentName>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ArtifactSourceProvenance {
    Primary,
    CurrentWorktree,
    #[default]
    LegacyUnspecified,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRuleContract {
    pub provenance: ArtifactSourceProvenance,
    pub source_root: StoredPath,
    pub manifest_digest: ObjectId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveIntent {
    pub repository: RepositoryIdentity,
    pub worktree: StoredPath,
    pub allow_dirty_removal: bool,
    pub delete_local_branch: bool,
    pub force_delete_local_branch: bool,
    pub delete_remote_branch: Option<RemoteBranch>,
    pub granted_consents: BTreeSet<ConsentId>,
}
impl RemoveIntent {
    pub fn new(
        repository: RepositoryIdentity,
        worktree: StoredPath,
        allow_dirty_removal: bool,
        delete_local_branch: bool,
        force_delete_local_branch: bool,
        delete_remote_branch: Option<RemoteBranch>,
        granted_consents: BTreeSet<ConsentId>,
    ) -> Result<Self, String> {
        if force_delete_local_branch && !delete_local_branch {
            return Err("force-delete-local-branch requires delete-local-branch".into());
        }
        Ok(Self {
            repository,
            worktree,
            allow_dirty_removal,
            delete_local_branch,
            force_delete_local_branch,
            delete_remote_branch,
            granted_consents,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationIntent {
    Create(CreateIntent),
    Remove(RemoveIntent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MergeTargetProvenance {
    Primary,
    Upstream {
        branch: BranchName,
        upstream_ref: RefName,
    },
    #[serde(alias = "legacy_unspecified")]
    #[default]
    LegacyUnspecified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Precondition {
    CommonDirectory(StoredPath),
    ExactlyOnePrimary,
    BareRepositoryFalse,
    PathAbsent(StoredPath),
    ParentSafe(StoredPath),
    RefAbsent(RefName),
    RefAt {
        reference: RefName,
        oid: ObjectId,
    },
    RefMergedInto {
        reference: RefName,
        #[serde(default)]
        target_ref: Option<RefName>,
        target_oid: ObjectId,
        #[serde(default)]
        provenance: MergeTargetProvenance,
    },
    BranchUpstreamIs {
        branch: BranchName,
        upstream_ref: RefName,
    },
    WorktreeAt {
        path: StoredPath,
        branch: BranchName,
        oid: ObjectId,
        class: crate::domain::WorktreeClass,
    },
    SymlinkAt {
        path: StoredPath,
        target_digest: ObjectId,
    },
    SymlinkAtV3 {
        path: StoredPath,
        expected: SymlinkStateV3,
    },
    RemoteRefAt {
        remote: RemoteName,
        branch: BranchName,
        oid: ObjectId,
    },
    WorktreeRegistered {
        path: StoredPath,
        oid: ObjectId,
    },
    WorktreeClass {
        path: StoredPath,
        class: crate::domain::WorktreeClass,
    },
    WorktreeUnlocked {
        path: StoredPath,
    },
    WorktreeNotPrunable {
        path: StoredPath,
    },
    WorktreeClean {
        path: StoredPath,
    },
    NoOngoingGitOperation {
        path: StoredPath,
    },
    BranchNotElsewhere(BranchName),
    BranchNotCheckedOut(BranchName),
    RemoteBranchNotDefault(RemoteBranch),
    SourceManifest {
        rule: String,
        source: StoredPath,
        destination: StoredPath,
        digest: ObjectId,
    },
    ArtifactSourceAt {
        rule: String,
        source_root: StoredPath,
        source: StoredPath,
        destination: StoredPath,
        bytes: u64,
        digest: ObjectId,
        manifest_digest: ObjectId,
    },
    ArtifactSourceAtV3 {
        rule: String,
        source_root: StoredPath,
        source: StoredPath,
        expectation: ArtifactSourceExpectationV3,
        manifest_digest: ObjectId,
    },
    TreeSymlinkAtV3 {
        commit_oid: ObjectId,
        checkout_relative_path: StoredPath,
        expected: SymlinkStateV3,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Postcondition {
    WorktreeCreated {
        path: StoredPath,
        oid: ObjectId,
    },
    WorktreeRemoved {
        path: StoredPath,
        oid: ObjectId,
    },
    BranchCreated {
        branch: BranchName,
        oid: ObjectId,
    },
    BranchUpstreamAt {
        branch: BranchName,
        remote: RemoteName,
        remote_branch: BranchName,
    },
    BranchDeleted(BranchName),
    RemoteBranchDeleted(RemoteBranch),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskKind {
    SensitiveMaterialization,
    ReplaceExistingSymlink,
    ExecuteTask,
    DirtyDataLoss,
    DeleteLocalBranch,
    ForceDeleteLocalBranch,
    DeleteRemoteBranch,
    IrreversibleStep,
    RemoveWorktree,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Risk {
    pub kind: RiskKind,
    pub message: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentRequirement {
    pub id: ConsentId,
    pub risks: Vec<Risk>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedArtifact {
    pub path: StoredPath,
    pub fingerprint: ObjectId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplacedSymlink {
    pub path: StoredPath,
    pub expected_current: ObjectId,
    pub original_target: StoredPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegularFileStateV3 {
    pub bytes: u64,
    pub digest: ObjectId,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymlinkStateV3 {
    pub target: StoredPath,
    pub target_digest: ObjectId,
}
impl PartialEq<&SymlinkStateV3> for SymlinkStateV3 {
    fn eq(&self, other: &&SymlinkStateV3) -> bool {
        self == *other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactSourceKindV3 {
    RegularFile,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedStagingV3 {
    pub path: StoredPath,
    pub ownership_token: ObjectId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublicationStrategyV3 {
    AtomicNoReplaceV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactStateV3 {
    Regular(RegularFileStateV3),
    Symlink(SymlinkStateV3),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactSourceExpectationV3 {
    Regular(RegularFileStateV3),
    Directory,
    Symlink(SymlinkStateV3),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedArtifactV3 {
    pub path: StoredPath,
    pub expected: ArtifactStateV3,
    pub staging: Option<OwnedStagingV3>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplacedSymlinkV3 {
    pub path: StoredPath,
    pub expected_current: SymlinkStateV3,
    pub restore: SymlinkStateV3,
    pub replacement_staging: OwnedStagingV3,
    pub backup_staging: OwnedStagingV3,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedWorktree {
    pub path: StoredPath,
    pub branch: BranchName,
    pub expected_oid: ObjectId,
    pub branch_was_created: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedLocalBranch {
    pub branch: BranchName,
    pub expected_oid: ObjectId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Compensation {
    RemoveCreatedArtifact(CreatedArtifact),
    RestoreReplacedSymlink(ReplacedSymlink),
    RemoveCreatedWorktree(CreatedWorktree),
    DeleteCreatedLocalBranch(CreatedLocalBranch),
    RemoveCreatedArtifactV3(CreatedArtifactV3),
    RestoreReplacedSymlinkV3(ReplacedSymlinkV3),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepAction {
    CreateWorktree {
        destination: StoredPath,
        source: CreateSource,
    },
    FileArtifact {
        rule: String,
        kind: crate::planner::FileArtifactKind,
        source: StoredPath,
        destination: StoredPath,
        bytes: u64,
        digest: ObjectId,
        fingerprint: ObjectId,
        link_target: Option<StoredPath>,
        manifest_digest: ObjectId,
        #[serde(default)]
        sensitive: bool,
        #[serde(default)]
        confirm: bool,
        #[serde(default = "legacy_mode_policy")]
        mode_policy: crate::planner::FileModePolicy,
    },
    CopyFileV3 {
        rule: String,
        source_root: StoredPath,
        source: StoredPath,
        expected_source: RegularFileStateV3,
        destination: StoredPath,
        desired_output: RegularFileStateV3,
        staging: OwnedStagingV3,
        publication: PublicationStrategyV3,
        manifest_digest: ObjectId,
        sensitive: bool,
        confirm: bool,
    },
    CreateSymlinkV3 {
        rule: String,
        source_root: StoredPath,
        source: StoredPath,
        expected_source: ArtifactSourceExpectationV3,
        destination: StoredPath,
        desired: SymlinkStateV3,
        manifest_digest: ObjectId,
        sensitive: bool,
        confirm: bool,
    },
    RelinkSymlinkV3 {
        rule: String,
        source_root: StoredPath,
        source: StoredPath,
        expected_source: SymlinkStateV3,
        checkout_oid: ObjectId,
        checkout_relative_path: StoredPath,
        destination: StoredPath,
        expected_old: SymlinkStateV3,
        desired_new: SymlinkStateV3,
        replacement_staging: OwnedStagingV3,
        backup_staging: OwnedStagingV3,
        manifest_digest: ObjectId,
        sensitive: bool,
        confirm: bool,
    },
    RunTask {
        name: String,
        argv: CommandArgv,
        cwd: StoredPath,
        required: bool,
        environment_allowlist: Vec<EnvironmentName>,
    },
    RemoveWorktree {
        path: StoredPath,
    },
    DeleteLocalBranch {
        branch: BranchName,
    },
    DeleteRemoteBranch {
        target: RemoteBranch,
        #[serde(default)]
        expected_oid: Option<ObjectId>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    id: StepId,
    name: String,
    action: StepAction,
    preconditions: Vec<Precondition>,
    postconditions: Vec<Postcondition>,
    compensation: Option<Compensation>,
    irreversible: bool,
}
impl PlanStep {
    #[cfg(test)]
    pub(crate) fn action_mut(&mut self) -> &mut StepAction {
        &mut self.action
    }

    #[cfg(test)]
    pub(crate) fn preconditions_mut(&mut self) -> &mut Vec<Precondition> {
        &mut self.preconditions
    }

    pub fn new(
        id: StepId,
        name: String,
        action: StepAction,
        preconditions: Vec<Precondition>,
        postconditions: Vec<Postcondition>,
        compensation: Option<Compensation>,
        irreversible: bool,
    ) -> Result<Self, String> {
        validate_text(&name)
            .map_err(|_| "step name must be non-empty and non-control".to_owned())?;
        if irreversible && compensation.is_some() {
            return Err("irreversible step cannot have automatic compensation".into());
        }
        Ok(Self {
            id,
            name,
            action,
            preconditions,
            postconditions,
            compensation,
            irreversible,
        })
    }
    pub fn id(&self) -> &StepId {
        &self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn action(&self) -> &StepAction {
        &self.action
    }
    pub fn preconditions(&self) -> &[Precondition] {
        &self.preconditions
    }
    pub fn postconditions(&self) -> &[Postcondition] {
        &self.postconditions
    }
    pub fn compensation(&self) -> Option<&Compensation> {
        self.compensation.as_ref()
    }
    pub fn irreversible(&self) -> bool {
        self.irreversible
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationPlan {
    plan_schema_version: u8,
    operation_id: OperationId,
    kind: OperationKind,
    repository: RepositoryIdentity,
    intent: OperationIntent,
    preconditions: Vec<Precondition>,
    steps: Vec<PlanStep>,
    risks: Vec<Risk>,
    required_consents: Vec<ConsentRequirement>,
    granted_consents: BTreeSet<ConsentId>,
}
#[derive(Debug, Clone)]
pub struct OperationPlanDraft {
    pub(crate) operation_id: OperationId,
    pub(crate) kind: OperationKind,
    pub(crate) repository: RepositoryIdentity,
    pub(crate) intent: OperationIntent,
    pub(crate) preconditions: Vec<Precondition>,
    pub(crate) steps: Vec<PlanStep>,
    pub(crate) risks: Vec<Risk>,
    pub(crate) required_consents: Vec<ConsentRequirement>,
    pub(crate) granted_consents: BTreeSet<ConsentId>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Create,
    Remove,
}
impl OperationPlan {
    pub fn new(draft: OperationPlanDraft) -> Result<Self, String> {
        let OperationPlanDraft {
            operation_id,
            kind,
            repository,
            intent,
            preconditions,
            steps,
            risks,
            required_consents,
            granted_consents,
        } = draft;
        if steps.is_empty() {
            return Err("plan must contain steps".into());
        }
        let step_ids: BTreeSet<_> = steps.iter().map(|s| s.id.clone()).collect();
        if step_ids.len() != steps.len() {
            return Err("step ids must be unique".into());
        }
        let mut required_consents = required_consents;
        for consent in &mut required_consents {
            let mut unique = Vec::new();
            for risk in consent.risks.drain(..) {
                if !unique.contains(&risk) {
                    unique.push(risk);
                }
            }
            consent.risks = unique;
            if consent.risks.is_empty() {
                return Err("consent requirement must cover at least one risk".into());
            }
        }
        let consent_ids: BTreeSet<_> = required_consents.iter().map(|c| c.id.clone()).collect();
        if consent_ids.len() != required_consents.len() {
            return Err("consent ids must be unique".into());
        }
        if !granted_consents.is_subset(&consent_ids) {
            return Err("granted consents must be a subset of required consents".into());
        }
        if matches!(
            (&kind, &intent),
            (OperationKind::Create, OperationIntent::Remove(_))
                | (OperationKind::Remove, OperationIntent::Create(_))
        ) {
            return Err("operation kind and intent mismatch".into());
        }
        let intent_grants = match &intent {
            OperationIntent::Create(value) => &value.granted_consents,
            OperationIntent::Remove(value) => &value.granted_consents,
        };
        if (kind == OperationKind::Create || kind == OperationKind::Remove)
            && &granted_consents != intent_grants
        {
            return Err("plan and intent granted consents mismatch".into());
        }
        let intent_repository = match &intent {
            OperationIntent::Create(value) => &value.repository,
            OperationIntent::Remove(value) => &value.repository,
        };
        if intent_repository != &repository {
            return Err("plan repository and intent repository mismatch".into());
        }
        Ok(Self {
            plan_schema_version: 3,
            operation_id,
            kind,
            repository,
            intent,
            preconditions,
            steps,
            risks,
            required_consents,
            granted_consents,
        })
    }
    pub fn plan_schema_version(&self) -> u8 {
        self.plan_schema_version
    }
    pub fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }
    pub fn kind(&self) -> OperationKind {
        self.kind
    }
    pub fn repository(&self) -> &RepositoryIdentity {
        &self.repository
    }
    pub fn intent(&self) -> &OperationIntent {
        &self.intent
    }
    pub fn preconditions(&self) -> &[Precondition] {
        &self.preconditions
    }
    pub fn steps(&self) -> &[PlanStep] {
        &self.steps
    }
    #[cfg(test)]
    pub(crate) fn steps_mut(&mut self) -> &mut [PlanStep] {
        &mut self.steps
    }
    pub fn risks(&self) -> &[Risk] {
        &self.risks
    }
    pub fn required_consents(&self) -> &[ConsentRequirement] {
        &self.required_consents
    }
    pub fn granted_consents(&self) -> &BTreeSet<ConsentId> {
        &self.granted_consents
    }
    pub fn validate_persisted(&self) -> Result<(), String> {
        if !(1..=3).contains(&self.plan_schema_version) {
            return Err("unsupported operation plan schema".into());
        }
        self.validate_shape()?;
        if matches!(self.plan_schema_version, 2 | 3) && self.grants_match_intent() {
            let mut restored = Self::new(OperationPlanDraft {
                operation_id: self.operation_id,
                kind: self.kind,
                repository: self.repository.clone(),
                intent: self.intent.clone(),
                preconditions: self.preconditions.clone(),
                steps: self.steps.clone(),
                risks: self.risks.clone(),
                required_consents: self.required_consents.clone(),
                granted_consents: self.granted_consents.clone(),
            })?;
            restored.plan_schema_version = self.plan_schema_version;
            if restored != *self {
                return Err("persisted operation plan is not canonical".into());
            }
        }
        Ok(())
    }

    fn validate_shape(&self) -> Result<(), String> {
        if self.steps.is_empty() {
            return Err("plan must contain steps".into());
        }
        let ids: BTreeSet<_> = self.steps.iter().map(|step| step.id.clone()).collect();
        if ids.len() != self.steps.len() {
            return Err("step ids must be unique".into());
        }
        for step in &self.steps {
            validate_text(&step.name)
                .map_err(|_| "persisted step name must be non-empty and non-control".to_owned())?;
            if step.irreversible && step.compensation.is_some() {
                return Err("persisted irreversible step has compensation".into());
            }
            validate_v3_modes(step)?;
        }
        let consent_ids: BTreeSet<_> = self
            .required_consents
            .iter()
            .map(|consent| consent.id.clone())
            .collect();
        if consent_ids.len() != self.required_consents.len()
            || self
                .required_consents
                .iter()
                .any(|consent| consent.risks.is_empty())
            || !self.granted_consents.is_subset(&consent_ids)
        {
            return Err("invalid persisted consent shape".into());
        }
        if matches!(
            (&self.kind, &self.intent),
            (OperationKind::Create, OperationIntent::Remove(_))
                | (OperationKind::Remove, OperationIntent::Create(_))
        ) {
            return Err("operation kind and intent mismatch".into());
        }
        let intent_repository = match &self.intent {
            OperationIntent::Create(value) => &value.repository,
            OperationIntent::Remove(value) => &value.repository,
        };
        if intent_repository != &self.repository {
            return Err("plan repository and intent repository mismatch".into());
        }
        Ok(())
    }

    pub fn validate_executable_plan(&self) -> Result<(), String> {
        self.validate_persisted()?;
        if self.plan_schema_version == 1 {
            return Err("schema-1 plans are read-only and not executable".into());
        }
        if self.plan_schema_version == 2
            && (self.contains_v3_artifact() || self.contains_legacy_artifact())
        {
            return Err("schema-2 plans containing artifact semantics are not executable".into());
        }
        if self.plan_schema_version == 3 && self.contains_legacy_artifact() {
            return Err("schema-3 plans containing legacy artifacts are not executable".into());
        }
        let intent_repository = match &self.intent {
            OperationIntent::Create(value) => &value.repository,
            OperationIntent::Remove(value) => &value.repository,
        };
        if self.repository != *intent_repository {
            return Err("executable plan repository mismatch".into());
        }
        if !self.grants_match_intent() {
            return Err("plan and intent granted consents mismatch".into());
        }
        match (&self.kind, &self.intent) {
            (OperationKind::Create, OperationIntent::Create(intent)) => {
                self.validate_executable_create(intent)
            }
            (OperationKind::Remove, OperationIntent::Remove(intent)) => {
                self.validate_executable_remove(intent)
            }
            _ => Err("executable plan kind and intent mismatch".into()),
        }
    }
    fn contains_legacy_artifact(&self) -> bool {
        self.preconditions.iter().any(|condition| {
            matches!(
                condition,
                Precondition::SourceManifest { .. }
                    | Precondition::ArtifactSourceAt { .. }
                    | Precondition::SymlinkAt { .. }
            )
        }) || self.steps.iter().any(|step| {
            matches!(step.action, StepAction::FileArtifact { .. })
                || step.compensation.as_ref().is_some_and(|value| {
                    matches!(
                        value,
                        Compensation::RemoveCreatedArtifact(_)
                            | Compensation::RestoreReplacedSymlink(_)
                    )
                })
                || step.preconditions.iter().any(|condition| {
                    matches!(
                        condition,
                        Precondition::SourceManifest { .. }
                            | Precondition::ArtifactSourceAt { .. }
                            | Precondition::SymlinkAt { .. }
                    )
                })
        })
    }
    fn contains_v3_artifact(&self) -> bool {
        self.steps.iter().any(|step| {
            matches!(
                step.action,
                StepAction::CopyFileV3 { .. }
                    | StepAction::CreateSymlinkV3 { .. }
                    | StepAction::RelinkSymlinkV3 { .. }
            ) || step.compensation.as_ref().is_some_and(|value| {
                matches!(
                    value,
                    Compensation::RemoveCreatedArtifactV3(_)
                        | Compensation::RestoreReplacedSymlinkV3(_)
                )
            }) || step.preconditions.iter().any(|condition| {
                matches!(
                    condition,
                    Precondition::ArtifactSourceAtV3 { .. }
                        | Precondition::TreeSymlinkAtV3 { .. }
                        | Precondition::SymlinkAtV3 { .. }
                )
            })
        })
    }
    fn grants_match_intent(&self) -> bool {
        match &self.intent {
            OperationIntent::Create(value) => self.granted_consents == value.granted_consents,
            OperationIntent::Remove(value) => self.granted_consents == value.granted_consents,
        }
    }

    fn validate_executable_create(&self, intent: &CreateIntent) -> Result<(), String> {
        let destination = intent
            .destination
            .as_ref()
            .ok_or("create executable plan requires a destination")?;
        let first = self
            .steps
            .first()
            .ok_or("create plan has no worktree step")?;
        let (worktree_destination, source) = match first.action() {
            StepAction::CreateWorktree {
                destination,
                source,
            } => (destination, source),
            _ => return Err("create plan must start with one worktree step".into()),
        };
        if worktree_destination != destination
            || !absolute_normalized(destination.as_path())
            || first.irreversible()
        {
            return Err("create worktree destination does not match intent".into());
        }
        if contained_or_equal(destination.as_path(), self.repository.common_dir.as_path()) {
            return Err("create destination enters repository common directory".into());
        }
        match (&intent.source, source) {
            (CreateSource::ExistingLocal { .. }, CreateSource::ExistingLocal { .. })
            | (CreateSource::RemoteTracking { .. }, CreateSource::RemoteTracking { .. }) => {
                if &intent.source != source {
                    return Err("create source does not match intent".into());
                }
            }
            (
                CreateSource::NewBranch {
                    branch: intent_branch,
                    base: intent_base,
                },
                CreateSource::NewBranch {
                    branch: action_branch,
                    base: Some(action_base),
                },
            ) if intent_branch == action_branch
                && intent_base.as_ref().is_none_or(|base| base == action_base) => {}
            _ => return Err("create source mode or base does not match intent".into()),
        }
        if !has_pre(
            first.preconditions(),
            |p| matches!(p, Precondition::CommonDirectory(path) if path == &self.repository.common_dir),
        ) || !has_pre(first.preconditions(), |p| {
            matches!(p, Precondition::ExactlyOnePrimary)
        }) || !has_pre(first.preconditions(), |p| {
            matches!(p, Precondition::BareRepositoryFalse)
        }) || !has_pre(
            first.preconditions(),
            |p| matches!(p, Precondition::PathAbsent(path) if path == destination),
        ) || !has_pre(
            first.preconditions(),
            |p| matches!(p, Precondition::ParentSafe(parent) if destination.as_path().parent() == Some(parent.as_path())),
        ) {
            return Err("create worktree step is missing mandatory guards".into());
        }
        if self.preconditions != first.preconditions() {
            return Err("create operation snapshot is weaker than its worktree step".into());
        }
        let branch = match source {
            CreateSource::NewBranch { branch, .. } | CreateSource::ExistingLocal { branch } => {
                branch
            }
            CreateSource::RemoteTracking { local_branch, .. } => local_branch,
        };
        if !has_pre(
            first.preconditions(),
            |p| matches!(p, Precondition::BranchNotElsewhere(value) if value == branch),
        ) || !has_pre(
            first.preconditions(),
            |p| matches!(p, Precondition::BranchNotCheckedOut(value) if value == branch),
        ) {
            return Err("create worktree step is missing branch guards".into());
        }
        match source {
            CreateSource::NewBranch { branch, base } => {
                if !has_pre(
                    first.preconditions(),
                    |p| matches!(p, Precondition::RefAbsent(value) if value.as_str() == branch.as_str()),
                ) {
                    return Err("new branch create is missing RefAbsent".into());
                }
                if let Some(base) = base
                    && !has_pre(
                        first.preconditions(),
                        |p| matches!(p, Precondition::RefAt { reference, .. } if reference == base),
                    )
                {
                    return Err("new branch create is missing base RefAt".into());
                }
            }
            CreateSource::ExistingLocal { branch } => {
                if !has_pre(
                    first.preconditions(),
                    |p| matches!(p, Precondition::RefAt { reference, .. } if reference.as_str() == branch.as_str()),
                ) {
                    return Err("existing branch create is missing RefAt".into());
                }
            }
            CreateSource::RemoteTracking {
                remote,
                remote_branch,
                local_branch,
            } => {
                let expected = format!("refs/remotes/{remote}/{remote_branch}");
                if !has_pre(first.preconditions(), |p| {
                    matches!(p, Precondition::RefAt { reference, .. } if reference.as_str() == expected)
                }) || !has_pre(first.preconditions(), |p| {
                    matches!(p, Precondition::RefAbsent(reference) if reference.as_str() == local_branch.as_str())
                }) || first.postconditions().iter().filter(|p| matches!(p, Postcondition::BranchUpstreamAt { .. })).count() != 1 || !first.postconditions().iter().any(|p| matches!(
                    p,
                    Postcondition::BranchUpstreamAt { branch, remote: value, remote_branch: upstream }
                        if branch == local_branch && value == remote && upstream == remote_branch
                )) {
                    return Err("remote tracking create is missing exact ref/upstream guards".into());
                }
            }
        }
        let branch_was_created = matches!(
            source,
            CreateSource::NewBranch { .. } | CreateSource::RemoteTracking { .. }
        );
        if !matches!(source, CreateSource::RemoteTracking { .. })
            && first
                .postconditions()
                .iter()
                .any(|condition| matches!(condition, Postcondition::BranchUpstreamAt { .. }))
        {
            return Err("non-remote create has an upstream postcondition".into());
        }
        let expected_oid = match first.compensation() {
            Some(Compensation::RemoveCreatedWorktree(value))
                if value.path == *destination
                    && value.branch == *branch
                    && value.branch_was_created == branch_was_created =>
            {
                value.expected_oid.clone()
            }
            _ => return Err("create worktree compensation does not match action".into()),
        };
        if first
            .postconditions()
            .iter()
            .filter(|p| matches!(p, Postcondition::WorktreeCreated { .. }))
            .count()
            != 1
            || !first.postconditions().iter().any(|p| {
                matches!(
                    p,
                    Postcondition::WorktreeCreated { path, oid }
                        if path == destination && oid == &expected_oid
                )
            })
        {
            return Err("create worktree is missing exact postcondition".into());
        }
        let source_refs: Vec<_> = first
            .preconditions()
            .iter()
            .filter_map(|condition| match condition {
                Precondition::RefAt { reference, oid } => Some((reference, oid)),
                _ => None,
            })
            .collect();
        if source_refs.len() != 1 || source_refs[0].1 != &expected_oid {
            return Err("create source RefAt is not unique or does not match worktree OID".into());
        }
        if first
            .postconditions()
            .iter()
            .filter(|p| matches!(p, Postcondition::BranchCreated { .. }))
            .count()
            != usize::from(branch_was_created)
        {
            return Err("create branch postcondition does not match source".into());
        }
        if branch_was_created
            && !first.postconditions().iter().any(|condition| {
                matches!(
                    condition,
                    Postcondition::BranchCreated { branch: value, oid }
                        if value == branch && oid == &expected_oid
                )
            })
        {
            return Err("create branch postcondition is not exact".into());
        }
        if self.plan_schema_version == 3 && self.contains_v3_artifact() {
            return self.validate_executable_create_v3(intent, destination);
        }
        let mut task_names = BTreeSet::new();
        let mut rule_manifests = BTreeMap::<String, ObjectId>::new();
        let mut manifest_contracts = BTreeMap::<
            String,
            (
                StoredPath,
                Vec<crate::planner::ManifestDigestArtifact>,
                ObjectId,
            ),
        >::new();
        let mut artifact_destinations = Vec::new();
        let mut seen_task = false;
        for step in self.steps().iter().skip(1) {
            match step.action() {
                StepAction::FileArtifact {
                    rule,
                    kind,
                    source,
                    destination: artifact_destination,
                    bytes,
                    digest,
                    manifest_digest,
                    sensitive,
                    confirm,
                    mode_policy,
                    fingerprint,
                    link_target,
                    ..
                } => {
                    let artifact_guards: Vec<_> = step
                        .preconditions()
                        .iter()
                        .filter(|condition| {
                            matches!(condition, Precondition::ArtifactSourceAt { .. })
                        })
                        .collect();
                    if artifact_guards.len() != 1 {
                        return Err("artifact must have exactly one source guard".into());
                    }
                    let (
                        guard_source_root,
                        guard_source,
                        guard_destination,
                        guard_bytes,
                        guard_digest,
                        guard_manifest_digest,
                        guard_rule,
                    ) = match artifact_guards[0] {
                        Precondition::ArtifactSourceAt {
                            source_root,
                            source: guard_source,
                            destination: guard_destination,
                            bytes: guard_bytes,
                            digest: guard_digest,
                            manifest_digest: guard_manifest_digest,
                            rule: guard_rule,
                        } => (
                            source_root,
                            guard_source,
                            guard_destination,
                            guard_bytes,
                            guard_digest,
                            guard_manifest_digest,
                            guard_rule,
                        ),
                        _ => unreachable!(),
                    };
                    if seen_task
                        || !contained_strict(artifact_destination.as_path(), destination.as_path())
                        || !absolute_normalized(guard_source_root.as_path())
                        || !contained_strict(source.as_path(), guard_source_root.as_path())
                    {
                        return Err("artifact is out of order or outside destination".into());
                    }
                    let required_guard = match kind {
                        crate::planner::FileArtifactKind::CopyFile
                        | crate::planner::FileArtifactKind::CreateSymlink => {
                            Precondition::PathAbsent(artifact_destination.clone())
                        }
                        crate::planner::FileArtifactKind::RelinkSymlink => {
                            Precondition::SymlinkAt {
                                path: artifact_destination.clone(),
                                target_digest: fingerprint.clone(),
                            }
                        }
                    };
                    if !step.preconditions().contains(&required_guard)
                        || guard_rule != rule
                        || guard_source != source
                        || guard_destination != artifact_destination
                        || guard_bytes != bytes
                        || guard_digest != digest
                        || guard_manifest_digest != manifest_digest
                        || step.irreversible()
                        || !step.postconditions().is_empty()
                    {
                        return Err("artifact step has invalid executable guards".into());
                    }
                    let Some(contract) = intent.artifact_rule_contracts.get(rule) else {
                        return Err("artifact rule contract is missing".into());
                    };
                    let expected_root = match contract.provenance {
                        ArtifactSourceProvenance::Primary
                            if contract.source_root == self.repository.primary_root =>
                        {
                            &self.repository.primary_root
                        }
                        ArtifactSourceProvenance::CurrentWorktree => {
                            let Some(root) = intent.current_worktree_root.as_ref() else {
                                return Err("current worktree source root is missing".into());
                            };
                            if contract.source_root != *root {
                                return Err("current worktree source root contract mismatch".into());
                            }
                            root
                        }
                        _ => return Err("artifact source provenance is invalid".into()),
                    };
                    if guard_source_root != expected_root
                        || contract.source_root != *guard_source_root
                        || contract.manifest_digest != *manifest_digest
                    {
                        return Err("artifact provenance contract does not match action".into());
                    }
                    artifact_destinations.push(artifact_destination.clone());
                    if let Some(previous) =
                        rule_manifests.insert(rule.clone(), manifest_digest.clone())
                        && previous != *manifest_digest
                    {
                        return Err("same-rule artifact manifest digests conflict".into());
                    }
                    if let Some((existing_root, _, _)) = manifest_contracts.get(rule)
                        && existing_root != guard_source_root
                    {
                        return Err("same-rule artifacts use mixed source roots".into());
                    }
                    manifest_contracts
                        .entry(rule.clone())
                        .or_insert_with(|| {
                            (
                                guard_source_root.clone(),
                                Vec::new(),
                                manifest_digest.clone(),
                            )
                        })
                        .1
                        .push(crate::planner::ManifestDigestArtifact {
                            source_root: guard_source_root.clone(),
                            source: source.clone(),
                            destination: artifact_destination.clone(),
                            kind: *kind,
                            bytes: *bytes,
                            digest: digest.clone(),
                            fingerprint: fingerprint.clone(),
                            link_target: link_target.clone(),
                            sensitive: *sensitive,
                            confirm: *confirm,
                            mode_policy: *mode_policy,
                        });
                    let expected_policy = match kind {
                        crate::planner::FileArtifactKind::CopyFile if *sensitive => {
                            crate::planner::FileModePolicy::Private
                        }
                        crate::planner::FileArtifactKind::CopyFile => {
                            crate::planner::FileModePolicy::PreserveSafe
                        }
                        _ => crate::planner::FileModePolicy::NotApplicable,
                    };
                    if *mode_policy != expected_policy {
                        return Err("artifact mode policy does not match sensitivity".into());
                    }
                    match kind {
                        crate::planner::FileArtifactKind::CopyFile
                            if link_target.is_some() || fingerprint != digest =>
                        {
                            return Err("copy artifact content contract is invalid".into());
                        }
                        crate::planner::FileArtifactKind::CreateSymlink
                        | crate::planner::FileArtifactKind::RelinkSymlink
                            if link_target.as_ref().is_none_or(|target| {
                                target.as_path().as_os_str().as_encoded_bytes().len() as u64
                                    != *bytes
                            }) || (matches!(
                                kind,
                                crate::planner::FileArtifactKind::CreateSymlink
                            ) && link_target.as_ref() != Some(source))
                                || fingerprint != digest
                                || link_target.as_ref().is_none_or(|target| {
                                    crate::planner::artifact_digest(
                                        target.as_path().as_os_str().as_encoded_bytes(),
                                    ) != *digest
                                }) =>
                        {
                            return Err("symlink artifact content contract is invalid".into());
                        }
                        _ => {}
                    }
                    match step.compensation() {
                        Some(Compensation::RemoveCreatedArtifact(value))
                            if value.path == *artifact_destination
                                && value.fingerprint == *fingerprint
                                && !matches!(
                                    kind,
                                    crate::planner::FileArtifactKind::RelinkSymlink
                                ) => {}
                        Some(Compensation::RestoreReplacedSymlink(value))
                            if matches!(kind, crate::planner::FileArtifactKind::RelinkSymlink)
                                && value.path == *artifact_destination
                                && value.expected_current == *fingerprint
                                && link_target.as_ref() == Some(&value.original_target) => {}
                        _ => return Err("artifact compensation does not match action".into()),
                    }
                    if (*sensitive || *confirm)
                        && !self
                            .required_consents()
                            .iter()
                            .any(|consent| consent.id.as_str() == format!("file-rule:{rule}"))
                    {
                        return Err("artifact consent is missing".into());
                    }
                }
                StepAction::RunTask {
                    name,
                    argv,
                    cwd,
                    required,
                    environment_allowlist,
                } => {
                    seen_task = true;
                    let Some(contract) = intent.task_contracts.get(name) else {
                        return Err("task action has no intent contract".into());
                    };
                    if contract.argv != *argv
                        || contract.cwd != *cwd
                        || contract.required != *required
                        || contract.environment_allowlist != *environment_allowlist
                        || !contained_or_equal(cwd.as_path(), destination.as_path())
                        || !task_names.insert(name.clone())
                        || !step.irreversible()
                        || step.compensation().is_some()
                    {
                        return Err("task step is invalid or not a final suffix".into());
                    }
                }
                _ => return Err("create plan contains an injected destructive action".into()),
            }
        }
        if task_names != intent.selected_tasks {
            return Err("create task actions do not match intent".into());
        }
        if intent
            .task_contracts
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            != intent.selected_tasks
        {
            return Err("create task contracts do not match intent".into());
        }
        if destination_paths_overlap(&artifact_destinations)
            || intent
                .artifact_rule_contracts
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
                != rule_manifests.keys().cloned().collect::<BTreeSet<_>>()
        {
            return Err("artifact destinations overlap or contracts do not match rules".into());
        }
        for (_, (source_root, contracts, expected)) in manifest_contracts {
            if crate::planner::canonical_manifest_digest(&contracts, destination.as_path())
                != expected
            {
                return Err("artifact manifest digest is not canonical".into());
            }
            if !absolute_normalized(source_root.as_path()) {
                return Err("artifact source root is not normalized".into());
            }
        }
        validate_create_consents(self, task_names)
    }

    fn validate_executable_create_v3(
        &self,
        intent: &CreateIntent,
        destination: &StoredPath,
    ) -> Result<(), String> {
        let create_step = self
            .steps
            .first()
            .ok_or("create plan has no worktree step")?;
        let worktree_destination = match create_step.action() {
            StepAction::CreateWorktree { destination, .. } => destination,
            _ => return Err("create plan does not start with a worktree".into()),
        };
        let worktree_posts: Vec<_> = create_step
            .postconditions()
            .iter()
            .filter_map(|post| match post {
                Postcondition::WorktreeCreated { path, oid } => Some((path, oid)),
                _ => None,
            })
            .collect();
        if worktree_posts.len() != 1 || worktree_posts[0].0 != worktree_destination {
            return Err("create worktree postcondition is not exact".into());
        }
        let create_checkout_oid = worktree_posts[0].1;
        let mut descriptors = BTreeMap::<String, Vec<crate::planner::ManifestDescriptorV3>>::new();
        let mut rules = BTreeSet::new();
        let mut paths = Vec::new();
        let mut source_paths = Vec::new();
        let mut tasks = BTreeSet::new();
        let mut saw_task = false;
        for step in self.steps.iter().skip(1) {
            match step.action() {
                StepAction::CopyFileV3 {
                    rule,
                    source_root,
                    source,
                    expected_source,
                    destination: final_path,
                    desired_output,
                    staging,
                    publication,
                    manifest_digest,
                    sensitive,
                    confirm,
                } => {
                    if saw_task
                        || step.irreversible()
                        || !step.postconditions().is_empty()
                        || !absolute_normalized(source_root.as_path())
                        || !contained_strict(source.as_path(), source_root.as_path())
                        || !contained_strict(final_path.as_path(), destination.as_path())
                        || !matches!(publication, PublicationStrategyV3::AtomicNoReplaceV1)
                    {
                        return Err("invalid v3 copy placement or lifecycle".into());
                    }
                    if expected_source.bytes != desired_output.bytes
                        || expected_source.digest != desired_output.digest
                        || desired_output.mode
                            != crate::planner::exact_output_mode(
                                if *sensitive {
                                    crate::planner::FileModePolicy::Private
                                } else {
                                    crate::planner::FileModePolicy::PreserveSafe
                                },
                                expected_source.mode,
                            )
                    {
                        return Err("invalid v3 copy output contract".into());
                    }
                    let expected_stage = crate::planner::artifact_staging_v3(
                        self.operation_id(),
                        step.id(),
                        crate::planner::ArtifactStagingRoleV3::Copy,
                        final_path.as_path(),
                    )
                    .map_err(|_| "invalid v3 copy staging")?;
                    if staging != &expected_stage {
                        return Err("invalid v3 copy staging".into());
                    }
                    let source_guards: Vec<_> = step
                        .preconditions()
                        .iter()
                        .filter(|p| matches!(p, Precondition::ArtifactSourceAtV3 { .. }))
                        .collect();
                    if source_guards.len() != 1 || step.preconditions().iter().any(|p| matches!(p, Precondition::ArtifactSourceAt { .. }))
                        || step.preconditions().iter().filter(|p| matches!(p, Precondition::PathAbsent(path) if path == final_path || path == &staging.path)).count() != 2
                        || !step.preconditions().iter().any(|p| matches!(p, Precondition::PathAbsent(path) if path == final_path))
                        || !step.preconditions().iter().any(|p| matches!(p, Precondition::PathAbsent(path) if path == &staging.path))
                    { return Err("invalid v3 copy guards".into()); }
                    let matches_guard = matches!(source_guards[0], Precondition::ArtifactSourceAtV3 { rule: r, source_root: sr, source: s, expectation: ArtifactSourceExpectationV3::Regular(v), manifest_digest: md } if r == rule && sr == source_root && s == source && v == expected_source && md == manifest_digest);
                    if !matches_guard {
                        return Err("v3 copy source guard does not match action".into());
                    }
                    match step.compensation() {
                        Some(Compensation::RemoveCreatedArtifactV3(c))
                            if c.path == *final_path
                                && c.expected
                                    == ArtifactStateV3::Regular(desired_output.clone())
                                && c.staging.as_ref() == Some(staging) => {}
                        _ => return Err("invalid v3 copy compensation".into()),
                    }
                    paths.extend([final_path.clone(), staging.path.clone()]);
                    source_paths.push(source.clone());
                    rules.insert(rule.clone());
                    descriptors.entry(rule.clone()).or_default().push(
                        crate::planner::ManifestDescriptorV3::CopyFileV3 {
                            source_root: source_root.clone(),
                            source: source.clone(),
                            expected_source: expected_source.clone(),
                            destination: final_path.clone(),
                            desired_output: desired_output.clone(),
                            staging: staging.clone(),
                            publication: *publication,
                            sensitive: *sensitive,
                            confirm: *confirm,
                        },
                    );
                    if !self.artifact_contract_matches(intent, rule, source_root, manifest_digest) {
                        return Err("v3 artifact rule contract mismatch".into());
                    }
                }
                StepAction::CreateSymlinkV3 {
                    rule,
                    source_root,
                    source,
                    expected_source,
                    destination: final_path,
                    desired,
                    manifest_digest,
                    sensitive,
                    confirm,
                } => {
                    if matches!(expected_source, ArtifactSourceExpectationV3::Symlink(_))
                        || matches!(expected_source, ArtifactSourceExpectationV3::Regular(state) if state.mode > 0o7777)
                    {
                        return Err("create symlink source expectation is invalid".into());
                    }
                    if saw_task
                        || step.irreversible()
                        || !step.postconditions().is_empty()
                        || !absolute_normalized(source_root.as_path())
                        || !contained_strict(source.as_path(), source_root.as_path())
                        || !contained_strict(final_path.as_path(), destination.as_path())
                        || desired.target != *source
                        || crate::planner::artifact_digest(
                            desired.target.as_path().as_os_str().as_encoded_bytes(),
                        ) != desired.target_digest
                        || !valid_symlink_payload(desired)
                    {
                        return Err("invalid v3 symlink placement or target".into());
                    }
                    if !step
                        .preconditions()
                        .iter()
                        .any(|p| matches!(p, Precondition::PathAbsent(path) if path == final_path))
                        || step.preconditions().iter().any(|p| {
                            matches!(
                                p,
                                Precondition::ArtifactSourceAt { .. }
                                    | Precondition::TreeSymlinkAtV3 { .. }
                            )
                        })
                    {
                        return Err("invalid v3 symlink guards".into());
                    }
                    let source_guards: Vec<_> = step
                        .preconditions()
                        .iter()
                        .filter(|p| matches!(p, Precondition::ArtifactSourceAtV3 { .. }))
                        .collect();
                    if source_guards.len() != 1
                        || !matches!(source_guards[0], Precondition::ArtifactSourceAtV3 { rule: r, source_root: sr, source: s, expectation: e, manifest_digest: md } if r == rule && sr == source_root && s == source && e == expected_source && md == manifest_digest)
                    {
                        return Err("v3 symlink source guard does not match action".into());
                    }
                    if !matches!(step.compensation(), Some(Compensation::RemoveCreatedArtifactV3(c)) if c.path == *final_path && c.expected == ArtifactStateV3::Symlink(desired.clone()) && c.staging.is_none())
                    {
                        return Err("invalid v3 symlink compensation".into());
                    }
                    paths.push(final_path.clone());
                    source_paths.push(source.clone());
                    rules.insert(rule.clone());
                    descriptors.entry(rule.clone()).or_default().push(
                        crate::planner::ManifestDescriptorV3::CreateSymlinkV3 {
                            source_root: source_root.clone(),
                            source: source.clone(),
                            expected_source: expected_source.clone(),
                            destination: final_path.clone(),
                            desired: desired.clone(),
                            sensitive: *sensitive,
                            confirm: *confirm,
                        },
                    );
                    if !self.artifact_contract_matches(intent, rule, source_root, manifest_digest) {
                        return Err("v3 artifact rule contract mismatch".into());
                    }
                }
                StepAction::RelinkSymlinkV3 {
                    rule,
                    source_root,
                    source,
                    expected_source,
                    checkout_oid,
                    checkout_relative_path,
                    destination: final_path,
                    expected_old,
                    desired_new,
                    replacement_staging,
                    backup_staging,
                    manifest_digest,
                    sensitive,
                    confirm,
                } => {
                    if saw_task
                        || step.irreversible()
                        || !step.postconditions().is_empty()
                        || expected_old == desired_new
                        || expected_source != desired_new
                        || !absolute_normalized(source_root.as_path())
                        || !contained_strict(source.as_path(), source_root.as_path())
                        || !contained_strict(final_path.as_path(), destination.as_path())
                        || !relative_is_safe(checkout_relative_path.as_path())
                        || crate::planner::artifact_digest(
                            expected_source
                                .target
                                .as_path()
                                .as_os_str()
                                .as_encoded_bytes(),
                        ) != expected_source.target_digest
                        || crate::planner::artifact_digest(
                            expected_old.target.as_path().as_os_str().as_encoded_bytes(),
                        ) != expected_old.target_digest
                        || crate::planner::artifact_digest(
                            desired_new.target.as_path().as_os_str().as_encoded_bytes(),
                        ) != desired_new.target_digest
                        || !valid_symlink_payload(expected_source)
                        || !valid_symlink_payload(expected_old)
                        || !valid_symlink_payload(desired_new)
                    {
                        return Err("invalid v3 relink contract".into());
                    }
                    let expected_relative = final_path
                        .as_path()
                        .strip_prefix(worktree_destination.as_path())
                        .map_err(|_| "relink destination is outside checkout")?;
                    if checkout_oid != create_checkout_oid
                        || checkout_relative_path.as_path() != expected_relative
                    {
                        return Err("relink checkout identity is not exact".into());
                    }
                    let replacement = crate::planner::artifact_staging_v3(
                        self.operation_id(),
                        step.id(),
                        crate::planner::ArtifactStagingRoleV3::RelinkReplacement,
                        final_path.as_path(),
                    )
                    .map_err(|_| "invalid relink staging")?;
                    let backup = crate::planner::artifact_staging_v3(
                        self.operation_id(),
                        step.id(),
                        crate::planner::ArtifactStagingRoleV3::RelinkBackup,
                        final_path.as_path(),
                    )
                    .map_err(|_| "invalid relink staging")?;
                    if replacement_staging != &replacement
                        || backup_staging != &backup
                        || replacement_staging.path == backup_staging.path
                    {
                        return Err("invalid v3 relink staging".into());
                    }
                    let source_guards: Vec<_> = step
                        .preconditions()
                        .iter()
                        .filter(|p| matches!(p, Precondition::ArtifactSourceAtV3 { .. }))
                        .collect();
                    let tree_guards: Vec<_> = step
                        .preconditions()
                        .iter()
                        .filter(|p| matches!(p, Precondition::TreeSymlinkAtV3 { .. }))
                        .collect();
                    let destination_guards: Vec<_> = step
                        .preconditions()
                        .iter()
                        .filter(|p| matches!(p, Precondition::SymlinkAtV3 { .. }))
                        .collect();
                    let replacement_absent = step.preconditions().iter().filter(|p| matches!(p, Precondition::PathAbsent(path) if path == &replacement_staging.path)).count();
                    let backup_absent = step.preconditions().iter().filter(|p| matches!(p, Precondition::PathAbsent(path) if path == &backup_staging.path)).count();
                    if source_guards.len() != 1
                        || tree_guards.len() != 1
                        || destination_guards.len() != 1
                        || step.preconditions().len() != 5
                        || step.preconditions().iter().any(
                            |p| matches!(p, Precondition::PathAbsent(path) if path == final_path),
                        )
                        || replacement_absent != 1
                        || backup_absent != 1
                        || !matches!(source_guards[0], Precondition::ArtifactSourceAtV3 { rule: r, source_root: sr, source: s, expectation: ArtifactSourceExpectationV3::Symlink(v), manifest_digest: md } if r == rule && sr == source_root && s == source && v == desired_new && md == manifest_digest)
                        || !matches!(tree_guards[0], Precondition::TreeSymlinkAtV3 { commit_oid: oid, checkout_relative_path: p, expected: v } if oid == create_checkout_oid && p.as_path() == expected_relative && v == expected_old)
                        || !matches!(destination_guards[0], Precondition::SymlinkAtV3 { path, expected } if path == final_path && expected == expected_old)
                    {
                        return Err("invalid v3 relink guards".into());
                    }
                    if !matches!(step.compensation(), Some(Compensation::RestoreReplacedSymlinkV3(c)) if c.path == *final_path && c.expected_current == *desired_new && c.restore == *expected_old && c.replacement_staging == *replacement_staging && c.backup_staging == *backup_staging)
                    {
                        return Err("invalid v3 relink compensation".into());
                    }
                    paths.extend([
                        final_path.clone(),
                        replacement_staging.path.clone(),
                        backup_staging.path.clone(),
                    ]);
                    source_paths.push(source.clone());
                    rules.insert(rule.clone());
                    descriptors.entry(rule.clone()).or_default().push(
                        crate::planner::ManifestDescriptorV3::RelinkSymlinkV3 {
                            source_root: source_root.clone(),
                            source: source.clone(),
                            expected_source: expected_source.clone(),
                            checkout_oid: checkout_oid.clone(),
                            checkout_relative_path: checkout_relative_path.clone(),
                            destination: final_path.clone(),
                            expected_old: expected_old.clone(),
                            desired_new: desired_new.clone(),
                            replacement_staging: replacement_staging.clone(),
                            backup_staging: backup_staging.clone(),
                            sensitive: *sensitive,
                            confirm: *confirm,
                        },
                    );
                    if !self.artifact_contract_matches(intent, rule, source_root, manifest_digest) {
                        return Err("v3 artifact rule contract mismatch".into());
                    }
                }
                StepAction::RunTask {
                    name,
                    argv,
                    cwd,
                    required,
                    environment_allowlist,
                } => {
                    saw_task = true;
                    let Some(contract) = intent.task_contracts.get(name) else {
                        return Err("task action has no intent contract".into());
                    };
                    if contract.argv != *argv
                        || contract.cwd != *cwd
                        || contract.required != *required
                        || contract.environment_allowlist != *environment_allowlist
                        || !contained_or_equal(cwd.as_path(), destination.as_path())
                        || !tasks.insert(name.clone())
                        || !step.irreversible()
                        || step.compensation().is_some()
                    {
                        return Err("invalid task suffix".into());
                    }
                }
                _ => return Err("schema-3 create contains an invalid action".into()),
            }
        }
        if tasks != intent.selected_tasks
            || intent
                .task_contracts
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
                != intent.selected_tasks
            || rules != intent.artifact_rule_contracts.keys().cloned().collect()
        {
            return Err("schema-3 task or artifact rules do not match intent".into());
        }
        if destination_paths_overlap(&paths) {
            return Err("schema-3 artifact paths overlap".into());
        }
        if paths.iter().any(|mutable| {
            source_paths.iter().any(|source| {
                mutable.as_path() == source.as_path()
                    || mutable.as_path().starts_with(source.as_path())
                    || source.as_path().starts_with(mutable.as_path())
            })
        }) {
            return Err("schema-3 mutable and source paths overlap".into());
        }
        for (rule, items) in descriptors {
            let digest =
                crate::planner::canonical_manifest_digest_v3(&items, destination.as_path());
            if intent
                .artifact_rule_contracts
                .get(&rule)
                .map(|c| &c.manifest_digest)
                != Some(&digest)
            {
                return Err("schema-3 manifest digest is not canonical".into());
            }
        }
        validate_create_consents(self, tasks)
    }

    fn artifact_contract_matches(
        &self,
        intent: &CreateIntent,
        rule: &str,
        root: &StoredPath,
        digest: &ObjectId,
    ) -> bool {
        intent
            .artifact_rule_contracts
            .get(rule)
            .is_some_and(|contract| {
                contract.source_root == *root
                    && contract.manifest_digest == *digest
                    && match contract.provenance {
                        ArtifactSourceProvenance::Primary => *root == self.repository.primary_root,
                        ArtifactSourceProvenance::CurrentWorktree => {
                            intent.current_worktree_root.as_ref() == Some(root)
                        }
                        ArtifactSourceProvenance::LegacyUnspecified => false,
                    }
            })
    }

    fn validate_executable_remove(&self, intent: &RemoveIntent) -> Result<(), String> {
        if self.steps.is_empty() || self.steps.len() > 3 {
            return Err("remove plan has an invalid action count".into());
        }
        let worktree = &self.steps[0];
        let path = match worktree.action() {
            StepAction::RemoveWorktree { path } if path == &intent.worktree => path,
            _ => return Err("remove plan must start with its worktree action".into()),
        };
        let worktree_guards: Vec<_> = worktree
            .preconditions()
            .iter()
            .filter_map(|guard| match guard {
                Precondition::WorktreeAt { path, oid, .. } => Some((path, oid)),
                _ => None,
            })
            .collect();
        let registered_guards: Vec<_> = worktree
            .preconditions()
            .iter()
            .filter_map(|guard| match guard {
                Precondition::WorktreeRegistered { path, oid } => Some((path, oid)),
                _ => None,
            })
            .collect();
        if worktree_guards.len() != 1
            || registered_guards.len() != 1
            || registered_guards[0].0 != &intent.worktree
            || registered_guards[0].1 != worktree_guards[0].1
        {
            return Err("remove worktree registration guard is not exact".into());
        }
        if worktree
            .postconditions()
            .iter()
            .filter(|p| matches!(p, Postcondition::WorktreeRemoved { .. }))
            .count()
            != 1
            || !worktree.irreversible()
            || worktree.compensation().is_some()
            || !worktree.postconditions().iter().any(|p| matches!(p, Postcondition::WorktreeRemoved { path: value, oid } if value == path && has_worktree_at_oid(worktree.preconditions(), oid)))
        {
            return Err("remove worktree action is not executable".into());
        }
        if !has_pre(
            worktree.preconditions(),
            |p| matches!(p, Precondition::CommonDirectory(path) if path == &self.repository.common_dir),
        ) || worktree
            .preconditions()
            .iter()
            .filter(|p| matches!(p, Precondition::WorktreeAt { .. }))
            .count()
            != 1
            || !has_pre(
                worktree.preconditions(),
                |p| matches!(p, Precondition::WorktreeAt { path, branch: value, oid, class: crate::domain::WorktreeClass::Linked } if path == &intent.worktree && oid == worktree_oid(worktree.preconditions()) && value == branch_from_worktree(worktree.preconditions())),
            )
            || !has_pre(
                worktree.preconditions(),
                |p| matches!(p, Precondition::WorktreeClass { path, class: crate::domain::WorktreeClass::Linked } if path == &intent.worktree),
            )
            || !has_pre(
                worktree.preconditions(),
                |p| matches!(p, Precondition::WorktreeUnlocked { path } if path == &intent.worktree),
            )
            || !has_pre(
                worktree.preconditions(),
                |p| matches!(p, Precondition::WorktreeNotPrunable { path } if path == &intent.worktree),
            )
            || !has_pre(
                worktree.preconditions(),
                |p| matches!(p, Precondition::NoOngoingGitOperation { path } if path == &intent.worktree),
            )
            || !has_pre(worktree.preconditions(), |p| {
                matches!(p, Precondition::BranchNotElsewhere(_))
            })
        {
            return Err("remove worktree action is missing a mandatory guard".into());
        }
        let branch = worktree
            .preconditions()
            .iter()
            .find_map(|condition| match condition {
                Precondition::BranchNotElsewhere(branch) => Some(branch),
                _ => None,
            })
            .ok_or("remove worktree branch guard is missing")?;
        let worktree_oid = worktree_oid(worktree.preconditions());
        if !intent.allow_dirty_removal
            && !has_pre(
                worktree.preconditions(),
                |p| matches!(p, Precondition::WorktreeClean { path } if path == &intent.worktree),
            )
        {
            return Err("clean removal is missing WorktreeClean".into());
        }
        if self.preconditions != worktree.preconditions() {
            return Err("remove operation snapshot is weaker than its worktree step".into());
        }
        let mut index = 1;
        if intent.delete_local_branch {
            let step = self.steps.get(index).ok_or("missing local deletion step")?;
            let branch = match step.action() {
                StepAction::DeleteLocalBranch { branch } => branch,
                _ => return Err("local deletion action is missing or out of order".into()),
            };
            if branch != branch_from_worktree(worktree.preconditions()) {
                return Err("local deletion branch does not match worktree".into());
            }
            if step
                .postconditions()
                .iter()
                .filter(|p| matches!(p, Postcondition::BranchDeleted(_)))
                .count()
                != 1
                || !step.postconditions().iter().any(|p| {
                    matches!(p, Postcondition::BranchDeleted(value) if value == branch)
                })
                || !step.irreversible()
                || step.compensation().is_some()
                || !has_pre(step.preconditions(), |p| matches!(p, Precondition::CommonDirectory(path) if path == &self.repository.common_dir))
                || !has_pre(step.preconditions(), |p| matches!(p, Precondition::BranchNotElsewhere(value) if value == branch))
                || !has_pre(step.preconditions(), |p| matches!(p, Precondition::BranchNotCheckedOut(value) if value == branch))
                || step.preconditions().iter().filter(|p| matches!(p, Precondition::RefAt { reference, .. } if reference.as_str() == branch.as_str())).count() != 1
                || !has_pre(step.preconditions(), |p| matches!(p, Precondition::RefAt { reference, oid } if reference.as_str() == branch.as_str() && oid == worktree_oid))
                || (!intent.force_delete_local_branch && step.preconditions().iter().filter(|p| matches!(p, Precondition::RefMergedInto { .. })).count() != 1)
                || (!intent.force_delete_local_branch && step.preconditions().iter().filter(|p| matches!(p, Precondition::RefAt { reference, .. } if Some(reference) == merged_target(step.preconditions()).map(|target| target.0))).count() != 1)
                || (!intent.force_delete_local_branch && !has_pre(step.preconditions(), |p| matches!(p, Precondition::RefAt { reference, oid } if Some((reference, oid)) == merged_target(step.preconditions()))))
                || (!intent.force_delete_local_branch && !has_pre(step.preconditions(), |p| matches!(p, Precondition::RefMergedInto { reference, target_ref: Some(target), target_oid, .. } if reference.as_str() == branch.as_str() && Some((target, target_oid)) == merged_target(step.preconditions()))))
                || (intent.force_delete_local_branch && has_pre(step.preconditions(), |p| matches!(p, Precondition::RefMergedInto { .. })))
            {
                return Err("local deletion action is missing exact safety guards".into());
            }
            if !intent.force_delete_local_branch {
                let merge = step
                    .preconditions()
                    .iter()
                    .find_map(|condition| match condition {
                        Precondition::RefMergedInto {
                            target_ref: Some(target_ref),
                            target_oid,
                            provenance,
                            ..
                        } => Some((target_ref, target_oid, provenance)),
                        _ => None,
                    })
                    .ok_or("local deletion merge provenance is missing")?;
                let exact = match merge.2 {
                    MergeTargetProvenance::Primary => {
                        merge.1 == &self.repository.repository_oid && merge.0.as_str() == "HEAD"
                    }
                    MergeTargetProvenance::Upstream {
                        branch: upstream_branch,
                        upstream_ref,
                    } => {
                        upstream_branch == branch
                            && merge.0 == upstream_ref
                            && has_pre(
                                step.preconditions(),
                                |p| matches!(p, Precondition::BranchUpstreamIs { branch: value, upstream_ref: actual } if value == branch && actual == upstream_ref),
                            )
                    }
                    MergeTargetProvenance::LegacyUnspecified => false,
                };
                if normalized_branch_ref(merge.0) == branch.as_str() || !exact {
                    return Err("local deletion merge provenance is not exact".into());
                }
            }
            index += 1;
        } else if intent.force_delete_local_branch {
            return Err("force local deletion is not requested by intent".into());
        }
        if let Some(target) = &intent.delete_remote_branch {
            let step = self
                .steps
                .get(index)
                .ok_or("missing remote deletion step")?;
            match step.action() {
                StepAction::DeleteRemoteBranch {
                    target: actual,
                    expected_oid: Some(_),
                } if actual == target => {}
                _ => return Err("remote deletion action is missing or out of order".into()),
            }
            if step
                .postconditions()
                .iter()
                .filter(|p| matches!(p, Postcondition::RemoteBranchDeleted(_)))
                .count()
                != 1
                || !step.irreversible()
                || step.compensation().is_some()
                || !has_pre(
                    step.preconditions(),
                    |p| matches!(p, Precondition::CommonDirectory(path) if path == &self.repository.common_dir),
                )
                || !has_pre(
                    step.preconditions(),
                    |p| matches!(p, Precondition::BranchNotElsewhere(value) if value == branch),
                )
                || step
                    .preconditions()
                    .iter()
                    .filter(|p| matches!(p, Precondition::RemoteBranchNotDefault(_)))
                    .count()
                    != 1
                || !has_pre(
                    step.preconditions(),
                    |p| matches!(p, Precondition::RemoteBranchNotDefault(actual) if actual == target),
                )
                || step
                    .preconditions()
                    .iter()
                    .filter(|p| matches!(p, Precondition::RemoteRefAt { .. }))
                    .count()
                    != 1
                || !has_pre(
                    step.preconditions(),
                    |p| matches!(p, Precondition::RemoteRefAt { remote, branch, .. } if remote == &target.remote && branch == &target.branch),
                )
                || !matches!(step.action(), StepAction::DeleteRemoteBranch { expected_oid: Some(expected), .. } if step.preconditions().iter().any(|p| matches!(p, Precondition::RemoteRefAt { remote, branch, oid } if remote == &target.remote && branch == &target.branch && oid == expected)))
                || !step.postconditions().iter().any(
                    |p| matches!(p, Postcondition::RemoteBranchDeleted(actual) if actual == target),
                )
            {
                return Err("remote deletion action is missing exact guards".into());
            }
            index += 1;
        }
        if index != self.steps.len() {
            return Err("remove plan contains an unexpected action".into());
        }
        validate_remove_consents(self, intent)
    }
}

fn has_pre<F: Fn(&Precondition) -> bool>(conditions: &[Precondition], predicate: F) -> bool {
    conditions.iter().any(predicate)
}
fn validate_v3_modes(step: &PlanStep) -> Result<(), String> {
    let valid = |state: &RegularFileStateV3| state.mode <= 0o7777;
    let valid_state = |state: &ArtifactStateV3| match state {
        ArtifactStateV3::Regular(value) => valid(value),
        ArtifactStateV3::Symlink(_) => true,
    };
    let valid_expectation = |value: &ArtifactSourceExpectationV3| match value {
        ArtifactSourceExpectationV3::Regular(state) => valid(state),
        ArtifactSourceExpectationV3::Directory | ArtifactSourceExpectationV3::Symlink(_) => true,
    };
    match step.action() {
        StepAction::CopyFileV3 {
            expected_source,
            desired_output,
            ..
        } if !valid(expected_source) || !valid(desired_output) => {
            return Err("v3 regular file mode is out of range".into());
        }
        StepAction::CreateSymlinkV3 {
            expected_source, ..
        } if !valid_expectation(expected_source) => {
            return Err("v3 regular file mode is out of range".into());
        }
        StepAction::RelinkSymlinkV3 { .. }
        | StepAction::CreateWorktree { .. }
        | StepAction::FileArtifact { .. }
        | StepAction::RunTask { .. }
        | StepAction::RemoveWorktree { .. }
        | StepAction::DeleteLocalBranch { .. }
        | StepAction::DeleteRemoteBranch { .. } => {}
        _ => {}
    }
    if step.preconditions().iter().any(|p| matches!(p, Precondition::ArtifactSourceAtV3 { expectation, .. } if !valid_expectation(expectation))) || step.compensation().is_some_and(|c| match c { Compensation::RemoveCreatedArtifactV3(v) => !valid_state(&v.expected), Compensation::RestoreReplacedSymlinkV3(_) | Compensation::RemoveCreatedArtifact(_) | Compensation::RestoreReplacedSymlink(_) | Compensation::RemoveCreatedWorktree(_) | Compensation::DeleteCreatedLocalBranch(_) => false }) { return Err("v3 regular file mode is out of range".into()); }
    Ok(())
}
fn has_worktree_at_oid(conditions: &[Precondition], oid: &ObjectId) -> bool {
    conditions.iter().any(|condition| {
        matches!(condition, Precondition::WorktreeAt { oid: value, .. } if value == oid)
    })
}
fn branch_from_worktree(conditions: &[Precondition]) -> &BranchName {
    conditions
        .iter()
        .find_map(|condition| match condition {
            Precondition::BranchNotElsewhere(branch) => Some(branch),
            _ => None,
        })
        .expect("validated worktree branch guard")
}
fn worktree_oid(conditions: &[Precondition]) -> &ObjectId {
    conditions
        .iter()
        .find_map(|condition| match condition {
            Precondition::WorktreeAt { oid, .. } => Some(oid),
            _ => None,
        })
        .expect("validated worktree OID contract")
}
fn merged_target(conditions: &[Precondition]) -> Option<(&RefName, &ObjectId)> {
    conditions.iter().find_map(|condition| match condition {
        Precondition::RefMergedInto {
            target_ref: Some(target_ref),
            target_oid,
            ..
        } => Some((target_ref, target_oid)),
        _ => None,
    })
}
fn normalized_branch_ref(reference: &RefName) -> &str {
    reference
        .as_str()
        .strip_prefix("refs/heads/")
        .unwrap_or(reference.as_str())
}
fn absolute_normalized(path: &std::path::Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
                    | std::path::Component::Normal(_)
            )
        })
}
pub(crate) fn relative_is_safe_for_planning(path: &std::path::Path) -> bool {
    path.components().all(|component| match component {
        std::path::Component::Normal(value) => {
            value != ".git" && value != ".ewtm" && value != "ewtm"
        }
        _ => false,
    })
}
fn relative_is_safe(path: &std::path::Path) -> bool {
    relative_is_safe_for_planning(path)
}
fn valid_symlink_payload(value: &SymlinkStateV3) -> bool {
    let bytes = value.target.as_path().as_os_str().as_encoded_bytes();
    !bytes.is_empty()
        && !bytes.contains(&0)
        && crate::planner::artifact_digest(bytes) == value.target_digest
}
fn contained_strict(path: &std::path::Path, root: &std::path::Path) -> bool {
    absolute_normalized(root)
        && absolute_normalized(path)
        && path != root
        && path.strip_prefix(root).is_ok_and(|relative| {
            relative.components().next().is_some() && relative_is_safe(relative)
        })
}
fn contained_or_equal(path: &std::path::Path, root: &std::path::Path) -> bool {
    absolute_normalized(root)
        && absolute_normalized(path)
        && (path == root || contained_strict(path, root))
}
pub(crate) fn destination_paths_overlap(paths: &[StoredPath]) -> bool {
    paths.iter().enumerate().any(|(index, left)| {
        paths[index + 1..].iter().any(|right| {
            absolute_normalized(left.as_path())
                && absolute_normalized(right.as_path())
                && (left.as_path().strip_prefix(right.as_path()).is_ok()
                    || right.as_path().strip_prefix(left.as_path()).is_ok())
        })
    })
}
fn validate_create_consents(plan: &OperationPlan, tasks: BTreeSet<String>) -> Result<(), String> {
    let mut expected = BTreeMap::<String, Vec<RiskKind>>::new();
    for step in plan.steps() {
        match step.action() {
            StepAction::FileArtifact {
                rule,
                sensitive,
                confirm,
                kind,
                ..
            } => {
                let id = format!("file-rule:{rule}");
                if *sensitive || *confirm {
                    expected
                        .entry(id.clone())
                        .or_default()
                        .push(RiskKind::SensitiveMaterialization);
                }
                if matches!(kind, crate::planner::FileArtifactKind::RelinkSymlink) {
                    expected
                        .entry(id)
                        .or_default()
                        .push(RiskKind::ReplaceExistingSymlink);
                }
            }
            StepAction::CopyFileV3 {
                rule,
                sensitive,
                confirm,
                ..
            }
            | StepAction::CreateSymlinkV3 {
                rule,
                sensitive,
                confirm,
                ..
            }
            | StepAction::RelinkSymlinkV3 {
                rule,
                sensitive,
                confirm,
                ..
            } => {
                let id = format!("file-rule:{rule}");
                if *sensitive || *confirm {
                    expected
                        .entry(id.clone())
                        .or_default()
                        .push(RiskKind::SensitiveMaterialization);
                }
                if matches!(step.action(), StepAction::RelinkSymlinkV3 { .. }) {
                    expected
                        .entry(id)
                        .or_default()
                        .push(RiskKind::ReplaceExistingSymlink);
                }
            }
            StepAction::RunTask { name, .. } => {
                expected
                    .entry(format!("task:{name}"))
                    .or_default()
                    .push(RiskKind::ExecuteTask);
            }
            _ => {}
        }
    }
    if tasks.len()
        != plan
            .steps()
            .iter()
            .filter(|s| matches!(s.action(), StepAction::RunTask { .. }))
            .count()
    {
        return Err("create task consent mapping is invalid".into());
    }
    let actual_ids: BTreeSet<_> = plan
        .required_consents()
        .iter()
        .map(|consent| consent.id.as_str().to_owned())
        .collect();
    let expected_ids: BTreeSet<_> = expected.keys().cloned().collect();
    if actual_ids != expected_ids {
        return Err("create consent mapping is not exact".into());
    }
    for (id, kinds) in expected {
        let mut unique = kinds;
        unique.sort_by_key(|kind| *kind as u8);
        unique.dedup();
        let consent = plan
            .required_consents()
            .iter()
            .find(|consent| consent.id.as_str() == id)
            .ok_or("create consent is missing")?;
        let actual: Vec<_> = consent.risks.iter().map(|risk| risk.kind).collect();
        if actual != unique
            || unique
                .iter()
                .any(|kind| !plan.risks().iter().any(|risk| risk.kind == *kind))
        {
            return Err("create consent risks are not exact".into());
        }
    }
    Ok(())
}
fn validate_remove_consents(plan: &OperationPlan, intent: &RemoveIntent) -> Result<(), String> {
    let mut expected = BTreeMap::<String, Vec<RiskKind>>::new();
    expected.insert("remove:worktree".into(), vec![RiskKind::RemoveWorktree]);
    if !has_pre(plan.steps()[0].preconditions(), |condition| {
        matches!(condition, Precondition::WorktreeClean { .. })
    }) {
        expected.insert("remove:dirty".into(), vec![RiskKind::DirtyDataLoss]);
    }
    if intent.delete_local_branch {
        expected.insert(
            "remove:local-branch".into(),
            vec![RiskKind::DeleteLocalBranch],
        );
        if intent.force_delete_local_branch {
            expected.insert(
                "remove:force-local-branch".into(),
                vec![RiskKind::ForceDeleteLocalBranch],
            );
        }
    }
    if let Some(remote) = &intent.delete_remote_branch {
        expected.insert(
            format!("remove:remote:{}/{}", remote.remote, remote.branch),
            vec![RiskKind::DeleteRemoteBranch],
        );
    }
    let ids: BTreeSet<_> = plan
        .required_consents()
        .iter()
        .map(|c| c.id.as_str().to_owned())
        .collect();
    let expected_ids: BTreeSet<_> = expected.keys().cloned().collect();
    if ids != expected_ids
        || !plan
            .risks()
            .iter()
            .any(|r| r.kind == RiskKind::RemoveWorktree)
    {
        return Err("remove worktree consent/risk is missing".into());
    }
    for (id, kinds) in expected {
        let consent = plan
            .required_consents()
            .iter()
            .find(|consent| consent.id.as_str() == id)
            .ok_or("remove consent is missing")?;
        let actual: Vec<_> = consent.risks.iter().map(|risk| risk.kind).collect();
        if actual != kinds
            || kinds
                .iter()
                .any(|kind| !plan.risks().iter().any(|risk| risk.kind == *kind))
        {
            return Err("remove consent risks are not exact".into());
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn v3_test_plan(step_count: usize) -> OperationPlan {
    let repository = RepositoryIdentity {
        common_dir: StoredPath::from(std::path::PathBuf::from("/r/.git")),
        primary_root: StoredPath::from(std::path::PathBuf::from("/r")),
        repository_oid: ObjectId::new("0000000000000000000000000000000000000000").unwrap(),
    };
    let branch = BranchName::new("feature").unwrap();
    let source = CreateSource::NewBranch {
        branch,
        base: Some(RefName::new("main").unwrap()),
    };
    let zero = ObjectId::new("0000000000000000000000000000000000000000").unwrap();
    let mut intent = OperationIntent::Create(CreateIntent {
        repository: repository.clone(),
        source: source.clone(),
        destination: Some(StoredPath::from(std::path::PathBuf::from("/r/w"))),
        selected_tasks: BTreeSet::new(),
        skipped_rules: BTreeSet::new(),
        granted_consents: BTreeSet::new(),
        task_contracts: BTreeMap::new(),
        current_worktree_root: None,
        artifact_rule_contracts: BTreeMap::new(),
    });
    let mut steps = vec![
        PlanStep::new(
            StepId::new("step-0").unwrap(),
            "step-0".into(),
            StepAction::CreateWorktree {
                destination: StoredPath::from(std::path::PathBuf::from("/r/w")),
                source: source.clone(),
            },
            vec![
                Precondition::ExactlyOnePrimary,
                Precondition::CommonDirectory(repository.common_dir.clone()),
                Precondition::BareRepositoryFalse,
                Precondition::PathAbsent(StoredPath::from(std::path::PathBuf::from("/r/w"))),
                Precondition::ParentSafe(StoredPath::from(std::path::PathBuf::from("/r"))),
                Precondition::RefAbsent(RefName::new("feature").unwrap()),
                Precondition::RefAt {
                    reference: RefName::new("main").unwrap(),
                    oid: zero.clone(),
                },
                Precondition::BranchNotElsewhere(BranchName::new("feature").unwrap()),
                Precondition::BranchNotCheckedOut(BranchName::new("feature").unwrap()),
            ],
            vec![
                Postcondition::WorktreeCreated {
                    path: StoredPath::from(std::path::PathBuf::from("/r/w")),
                    oid: zero.clone(),
                },
                Postcondition::BranchCreated {
                    branch: BranchName::new("feature").unwrap(),
                    oid: zero.clone(),
                },
            ],
            Some(Compensation::RemoveCreatedWorktree(CreatedWorktree {
                path: StoredPath::from(std::path::PathBuf::from("/r/w")),
                branch: BranchName::new("feature").unwrap(),
                expected_oid: zero.clone(),
                branch_was_created: true,
            })),
            false,
        )
        .unwrap(),
    ];
    for index in 1..step_count {
        let path = StoredPath::from(std::path::PathBuf::from(format!("/r/w/{index}")));
        let source = StoredPath::from(std::path::PathBuf::from(format!("/r/source/{index}")));
        let desired = SymlinkStateV3 {
            target: source.clone(),
            target_digest: crate::planner::artifact_digest(
                source.as_path().as_os_str().as_encoded_bytes(),
            ),
        };
        let expected = ArtifactSourceExpectationV3::Regular(RegularFileStateV3 {
            bytes: 1,
            digest: zero.clone(),
            mode: 0o644,
        });
        let manifest_digest = crate::planner::canonical_manifest_digest_v3(
            &[crate::planner::ManifestDescriptorV3::CreateSymlinkV3 {
                source_root: repository.primary_root.clone(),
                source: source.clone(),
                expected_source: expected.clone(),
                destination: path.clone(),
                desired: desired.clone(),
                sensitive: false,
                confirm: false,
            }],
            std::path::Path::new("/r/w"),
        );
        steps.push(
            PlanStep::new(
                StepId::new(format!("step-{index}")).unwrap(),
                format!("step-{index}"),
                StepAction::CreateSymlinkV3 {
                    rule: format!("rule-{index}"),
                    source_root: repository.primary_root.clone(),
                    source: source.clone(),
                    expected_source: expected.clone(),
                    destination: path.clone(),
                    desired: desired.clone(),
                    manifest_digest: manifest_digest.clone(),
                    sensitive: false,
                    confirm: false,
                },
                vec![
                    Precondition::ArtifactSourceAtV3 {
                        rule: format!("rule-{index}"),
                        source_root: repository.primary_root.clone(),
                        source: source.clone(),
                        expectation: expected.clone(),
                        manifest_digest: manifest_digest.clone(),
                    },
                    Precondition::PathAbsent(path.clone()),
                ],
                vec![],
                Some(Compensation::RemoveCreatedArtifactV3(CreatedArtifactV3 {
                    path,
                    expected: ArtifactStateV3::Symlink(desired),
                    staging: None,
                })),
                false,
            )
            .unwrap(),
        );
    }
    if let OperationIntent::Create(value) = &mut intent {
        value.current_worktree_root = Some(repository.primary_root.clone());
        for step in &steps {
            if let StepAction::CreateSymlinkV3 {
                rule,
                manifest_digest,
                ..
            } = step.action()
            {
                let source_root = step.preconditions().iter().find_map(|guard| match guard {
                    Precondition::ArtifactSourceAtV3 { source_root, .. } => {
                        Some(source_root.clone())
                    }
                    _ => None,
                });
                if let Some(source_root) = source_root {
                    value.artifact_rule_contracts.insert(
                        rule.clone(),
                        ArtifactRuleContract {
                            provenance: ArtifactSourceProvenance::Primary,
                            source_root,
                            manifest_digest: manifest_digest.clone(),
                        },
                    );
                }
            }
        }
    }
    OperationPlan::new(OperationPlanDraft {
        operation_id: OperationId::new(Uuid::new_v4()),
        kind: OperationKind::Create,
        repository,
        intent,
        preconditions: steps[0].preconditions().to_vec(),
        steps,
        risks: Vec::new(),
        required_consents: Vec::new(),
        granted_consents: BTreeSet::new(),
    })
    .unwrap()
}

#[cfg(test)]
pub(crate) fn test_plan(step_count: usize) -> OperationPlan {
    let mut plan = v3_test_plan(step_count);
    for step in plan.steps.iter_mut().skip(1) {
        let StepAction::CreateSymlinkV3 {
            rule,
            source,
            destination,
            manifest_digest,
            ..
        } = step.action.clone()
        else {
            continue;
        };
        step.action = StepAction::FileArtifact {
            rule: rule.clone(),
            kind: crate::planner::FileArtifactKind::CopyFile,
            source: source.clone(),
            destination: destination.clone(),
            bytes: 1,
            digest: ObjectId::new("0000000000000000000000000000000000000000").unwrap(),
            fingerprint: ObjectId::new("0000000000000000000000000000000000000000").unwrap(),
            link_target: None,
            manifest_digest,
            sensitive: false,
            confirm: false,
            mode_policy: crate::planner::FileModePolicy::PreserveSafe,
        };
        step.preconditions = vec![
            Precondition::ArtifactSourceAt {
                rule: rule.clone(),
                source_root: StoredPath::from(std::path::PathBuf::from("/r")),
                source: source.clone(),
                destination: destination.clone(),
                bytes: 1,
                digest: ObjectId::new("0000000000000000000000000000000000000000").unwrap(),
                manifest_digest: ObjectId::new("0000000000000000000000000000000000000000").unwrap(),
            },
            Precondition::PathAbsent(destination.clone()),
        ];
        step.compensation = Some(Compensation::RemoveCreatedArtifact(CreatedArtifact {
            path: destination,
            fingerprint: ObjectId::new("0000000000000000000000000000000000000000").unwrap(),
        }));
    }
    if let OperationIntent::Create(intent) = &mut plan.intent {
        intent.artifact_rule_contracts.clear();
    }
    plan
}

#[cfg(test)]
pub(crate) fn assert_shape_valid_but_not_executable(value: serde_json::Value) {
    let plan: OperationPlan = serde_json::from_value(value).expect("mutation must deserialize");
    assert!(
        plan.validate_persisted().is_ok(),
        "crafted mutation must retain persisted shape"
    );
    assert!(
        plan.validate_executable_plan().is_err(),
        "crafted mutation unexpectedly remained executable"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use serde_json::json;
    use std::{collections::BTreeSet, path::PathBuf};

    fn oid() -> ObjectId {
        ObjectId::new("0123456789012345678901234567890123456789").unwrap()
    }
    fn repo() -> RepositoryIdentity {
        RepositoryIdentity {
            common_dir: StoredPath::from(PathBuf::from("/repo/.git")),
            primary_root: StoredPath::from(PathBuf::from("/repo")),
            repository_oid: oid(),
        }
    }
    fn intent() -> CreateIntent {
        CreateIntent {
            repository: repo(),
            source: CreateSource::NewBranch {
                branch: BranchName::new("feature").unwrap(),
                base: None,
            },
            destination: None,
            selected_tasks: BTreeSet::new(),
            skipped_rules: BTreeSet::new(),
            granted_consents: BTreeSet::new(),
            task_contracts: BTreeMap::new(),
            current_worktree_root: None,
            artifact_rule_contracts: BTreeMap::new(),
        }
    }
    fn step(id: &str, irreversible: bool, compensation: Option<Compensation>) -> PlanStep {
        PlanStep::new(
            StepId::new(id).unwrap(),
            id.into(),
            StepAction::CreateWorktree {
                destination: StoredPath::from(PathBuf::from("/tmp/w")),
                source: intent().source,
            },
            vec![],
            vec![],
            compensation,
            irreversible,
        )
        .unwrap_or_else(|_| panic!("valid step"))
    }

    #[test]
    fn plan_has_exact_persisted_shape_and_roundtrips() {
        let plan = OperationPlan::new(OperationPlanDraft {
            operation_id: OperationId::new(Uuid::nil()),
            kind: OperationKind::Create,
            repository: repo(),
            intent: OperationIntent::Create(intent()),
            preconditions: vec![],
            steps: vec![step("create.worktree", false, None)],
            risks: vec![],
            required_consents: vec![],
            granted_consents: BTreeSet::new(),
        })
        .unwrap();
        let value = serde_json::to_value(&plan).unwrap();
        assert_eq!(value["plan_schema_version"], 3);
        assert!(value.get("status").is_none());
        assert_eq!(plan, serde_json::from_value(value).unwrap());
    }

    #[test]
    fn archived_schema_one_file_artifact_is_readable_but_not_executable() {
        let mut wire = serde_json::to_value(test_plan(2)).unwrap();
        wire["plan_schema_version"] = serde_json::json!(1);
        wire["steps"][1]["action"] = serde_json::json!({"FileArtifact": {
            "rule":"legacy", "kind":"copy_file", "source":"/r/source/1",
            "destination":"/r/w/1", "bytes":1,
            "digest":"0000000000000000000000000000000000000000",
            "fingerprint":"0000000000000000000000000000000000000000",
            "link_target":null, "manifest_digest":"0000000000000000000000000000000000000000"
        }});
        wire["steps"][1]["preconditions"] = serde_json::json!([
            {"ArtifactSourceAt":{"rule":"legacy","source_root":"/r","source":"/r/source/1","destination":"/r/w/1","bytes":1,"digest":"0000000000000000000000000000000000000000","manifest_digest":"0000000000000000000000000000000000000000"}},
            {"PathAbsent":"/r/w/1"}
        ]);
        wire["steps"][1]["compensation"] = serde_json::json!({"RemoveCreatedArtifact":{"path":"/r/w/1","fingerprint":"0000000000000000000000000000000000000000"}});
        let restored: OperationPlan = serde_json::from_value(wire).unwrap();
        assert_eq!(restored.plan_schema_version(), 1);
        assert!(restored.validate_persisted().is_ok());
        assert!(restored.validate_executable_plan().is_err());
    }

    #[test]
    fn schema_three_legacy_file_artifact_is_readable_but_not_executable() {
        let mut wire = serde_json::to_value(test_plan(2)).unwrap();
        wire["plan_schema_version"] = serde_json::json!(3);
        wire["steps"][1]["action"] = serde_json::json!({"FileArtifact": {
            "rule":"legacy", "kind":"copy_file", "source":"/r/source/1",
            "destination":"/r/w/1", "bytes":1,
            "digest":"0000000000000000000000000000000000000000",
            "fingerprint":"0000000000000000000000000000000000000000",
            "link_target":null, "manifest_digest":"0000000000000000000000000000000000000000"
        }});
        wire["steps"][1]["preconditions"] = serde_json::json!([
            {"ArtifactSourceAt":{"rule":"legacy","source_root":"/r","source":"/r/source/1","destination":"/r/w/1","bytes":1,"digest":"0000000000000000000000000000000000000000","manifest_digest":"0000000000000000000000000000000000000000"}},
            {"PathAbsent":"/r/w/1"}
        ]);
        wire["steps"][1]["compensation"] = serde_json::json!({"RemoveCreatedArtifact":{"path":"/r/w/1","fingerprint":"0000000000000000000000000000000000000000"}});
        let restored: OperationPlan = serde_json::from_value(wire).unwrap();
        assert!(restored.validate_persisted().is_ok());
        assert!(restored.validate_executable_plan().is_err());
    }

    #[test]
    fn schema_two_git_only_plan_remains_executable() {
        let mut wire = serde_json::to_value(v3_test_plan(1)).unwrap();
        wire["plan_schema_version"] = serde_json::json!(2);
        let restored: OperationPlan = serde_json::from_value(wire).unwrap();
        assert!(restored.validate_persisted().is_ok());
        assert!(restored.validate_executable_plan().is_ok());
    }

    #[test]
    fn executable_path_policy_is_component_safe() {
        assert!(contained_strict(
            std::path::Path::new("/repo/ewtm/work/file"),
            std::path::Path::new("/repo/ewtm/work"),
        ));
        assert!(!contained_strict(
            std::path::Path::new("/repo/work/../secret"),
            std::path::Path::new("/repo/work"),
        ));
        assert!(!contained_strict(
            std::path::Path::new("/repo/workshop/file"),
            std::path::Path::new("/repo/work"),
        ));
        assert!(!contained_strict(
            std::path::Path::new("relative/file"),
            std::path::Path::new("/repo"),
        ));
        assert!(!contained_strict(
            std::path::Path::new("/repo/work/.git/config"),
            std::path::Path::new("/repo/work"),
        ));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let component = std::ffi::OsStr::from_bytes(b"valid-\xff");
            let path = std::path::Path::new("/repo/work")
                .join(component)
                .join("file");
            assert!(contained_strict(&path, std::path::Path::new("/repo/work")));
        }
    }

    #[test]
    fn constructor_rejects_duplicate_ids_and_unclaimed_grants() {
        let duplicate = vec![step("same", false, None), step("same", false, None)];
        assert!(
            OperationPlan::new(OperationPlanDraft {
                operation_id: OperationId::new(Uuid::nil()),
                kind: OperationKind::Create,
                repository: repo(),
                intent: OperationIntent::Create(intent()),
                preconditions: vec![],
                steps: duplicate,
                risks: vec![],
                required_consents: vec![],
                granted_consents: BTreeSet::new()
            })
            .is_err()
        );
        let mut granted = BTreeSet::new();
        granted.insert(ConsentId::new("not-required").unwrap());
        assert!(
            OperationPlan::new(OperationPlanDraft {
                operation_id: OperationId::new(Uuid::nil()),
                kind: OperationKind::Create,
                repository: repo(),
                intent: OperationIntent::Create(intent()),
                preconditions: vec![],
                steps: vec![step("one", false, None)],
                risks: vec![],
                required_consents: vec![],
                granted_consents: granted
            })
            .is_err()
        );
    }

    #[test]
    fn irreversible_steps_cannot_have_compensation() {
        let compensation = Compensation::RemoveCreatedArtifact(CreatedArtifact {
            path: StoredPath::from(PathBuf::from("x")),
            fingerprint: oid(),
        });
        assert!(
            PlanStep::new(
                StepId::new("remote").unwrap(),
                "remote".into(),
                StepAction::DeleteRemoteBranch {
                    target: RemoteBranch {
                        remote: RemoteName::new("origin").unwrap(),
                        branch: BranchName::new("main").unwrap()
                    },
                    expected_oid: None,
                },
                vec![],
                vec![],
                Some(compensation),
                true
            )
            .is_err()
        );
    }

    #[test]
    fn typed_ids_reject_invalid_values() {
        assert!(BranchName::new("a\n").is_err());
        assert!(ObjectId::new("abc").is_err());
        assert!(ObjectId::new("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
        assert!(
            ObjectId::new("0123456789012345678901234567890123456789012345678901234567890123")
                .is_ok()
        );
    }

    #[test]
    fn remove_intent_requires_local_delete_for_force() {
        assert!(
            RemoveIntent::new(
                repo(),
                StoredPath::from(PathBuf::from("/w")),
                false,
                false,
                true,
                None,
                BTreeSet::new()
            )
            .is_err()
        );
    }

    #[test]
    fn command_argv_is_nonempty_and_nul_safe() {
        assert!(CommandArgv::new(Vec::new()).is_err());
        assert!(CommandArgv::new(vec!["ok\0bad".into()]).is_err());
        let argv = CommandArgv::new(vec!["tool".into(), "--check".into()]).unwrap();
        assert_eq!(
            serde_json::from_value::<CommandArgv>(serde_json::to_value(argv).unwrap())
                .unwrap()
                .as_slice()
                .len(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn stored_path_non_utf8_json_roundtrip_is_lossless() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let path = StoredPath::from(PathBuf::from(OsString::from_vec(vec![b'x', 0xff])));
        let encoded = serde_json::to_value(&path).unwrap();
        assert_eq!(encoded["kind"], json!("bytes"));
        let decoded: StoredPath = serde_json::from_value(encoded).unwrap();
        assert_eq!(path, decoded);
    }

    #[test]
    fn create_destination_under_external_common_dir_is_not_executable() {
        fn rewrite(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::String(text) if text == "/r/.git" => {
                    *text = "/external/common".into()
                }
                serde_json::Value::String(text) if text == "/r/w" => {
                    *text = "/external/common/child".into()
                }
                serde_json::Value::String(text) if text == "/r" => {
                    *text = "/external/common".into()
                }
                serde_json::Value::Array(values) => values.iter_mut().for_each(rewrite),
                serde_json::Value::Object(values) => values.values_mut().for_each(rewrite),
                _ => {}
            }
        }
        let mut value = serde_json::to_value(test_plan(1)).unwrap();
        rewrite(&mut value);
        assert_shape_valid_but_not_executable(value);
    }

    fn other_oid() -> ObjectId {
        ObjectId::new("fedcba9876543210fedcba9876543210fedcba98").unwrap()
    }

    #[test]
    fn crafted_create_mutations_are_persisted_but_not_executable() {
        type Mutation = (&'static str, fn(&mut OperationPlan));
        let mutations: Vec<Mutation> = vec![
            ("action source mode", |p| {
                if let StepAction::CreateWorktree { source, .. } = &mut p.steps[0].action {
                    *source = CreateSource::ExistingLocal {
                        branch: BranchName::new("other").unwrap(),
                    };
                }
            }),
            ("action source branch", |p| {
                if let StepAction::CreateWorktree {
                    source: CreateSource::NewBranch { branch, .. },
                    ..
                } = &mut p.steps[0].action
                {
                    *branch = BranchName::new("other").unwrap();
                }
            }),
            ("action source base", |p| {
                if let StepAction::CreateWorktree {
                    source: CreateSource::NewBranch { base, .. },
                    ..
                } = &mut p.steps[0].action
                {
                    *base = Some(RefName::new("other").unwrap());
                }
            }),
            ("authoritative RefAt OID", |p| {
                if let Precondition::RefAt { oid, .. } = p.steps[0]
                    .preconditions
                    .iter_mut()
                    .find(|x| matches!(x, Precondition::RefAt { .. }))
                    .unwrap()
                {
                    *oid = other_oid();
                }
            }),
            ("ParentSafe", |p| {
                p.steps[0]
                    .preconditions
                    .retain(|x| !matches!(x, Precondition::ParentSafe(_)))
            }),
            ("WorktreeCreated path", |p| {
                if let Postcondition::WorktreeCreated { path, .. } = p.steps[0]
                    .postconditions
                    .iter_mut()
                    .find(|x| matches!(x, Postcondition::WorktreeCreated { .. }))
                    .unwrap()
                {
                    *path = StoredPath::from(PathBuf::from("/r/other"));
                }
            }),
            ("WorktreeCreated OID", |p| {
                if let Postcondition::WorktreeCreated { oid, .. } = p.steps[0]
                    .postconditions
                    .iter_mut()
                    .find(|x| matches!(x, Postcondition::WorktreeCreated { .. }))
                    .unwrap()
                {
                    *oid = other_oid();
                }
            }),
            ("BranchCreated missing", |p| {
                p.steps[0]
                    .postconditions
                    .retain(|x| !matches!(x, Postcondition::BranchCreated { .. }))
            }),
            ("BranchCreated wrong", |p| {
                if let Postcondition::BranchCreated { branch, .. } = p.steps[0]
                    .postconditions
                    .iter_mut()
                    .find(|x| matches!(x, Postcondition::BranchCreated { .. }))
                    .unwrap()
                {
                    *branch = BranchName::new("other").unwrap();
                }
            }),
            ("BranchCreated duplicate", |p| {
                let x = p.steps[0]
                    .postconditions
                    .iter()
                    .find(|x| matches!(x, Postcondition::BranchCreated { .. }))
                    .unwrap()
                    .clone();
                p.steps[0].postconditions.push(x);
            }),
            ("compensation path", |p| {
                if let Some(Compensation::RemoveCreatedWorktree(x)) = &mut p.steps[0].compensation {
                    x.path = StoredPath::from(PathBuf::from("/r/other"));
                }
            }),
            ("compensation branch", |p| {
                if let Some(Compensation::RemoveCreatedWorktree(x)) = &mut p.steps[0].compensation {
                    x.branch = BranchName::new("other").unwrap();
                }
            }),
            ("compensation OID", |p| {
                if let Some(Compensation::RemoveCreatedWorktree(x)) = &mut p.steps[0].compensation {
                    x.expected_oid = other_oid();
                }
            }),
            ("compensation created flag", |p| {
                if let Some(Compensation::RemoveCreatedWorktree(x)) = &mut p.steps[0].compensation {
                    x.branch_was_created = false;
                }
            }),
            ("destination parent", |p| {
                if let OperationIntent::Create(x) = &mut p.intent {
                    x.destination = Some(StoredPath::from(PathBuf::from("/elsewhere/w")));
                }
            }),
            ("destination outside", |p| {
                if let StepAction::CreateWorktree { destination, .. } = &mut p.steps[0].action {
                    *destination = StoredPath::from(PathBuf::from("/outside"));
                }
            }),
            ("destination ..", |p| {
                if let StepAction::CreateWorktree { destination, .. } = &mut p.steps[0].action {
                    *destination = StoredPath::from(PathBuf::from("/r/w/../x"));
                }
            }),
            ("destination reserved", |p| {
                if let StepAction::CreateWorktree { destination, .. } = &mut p.steps[0].action {
                    *destination = StoredPath::from(PathBuf::from("/r/.git"));
                }
            }),
            ("injected Remove action", |p| {
                p.steps[1].action = StepAction::RemoveWorktree {
                    path: StoredPath::from(PathBuf::from("/r/w")),
                }
            }),
            ("artifact source_root", |p| {
                if let Precondition::ArtifactSourceAt { source_root, .. } = p.steps[1]
                    .preconditions
                    .iter_mut()
                    .find(|x| matches!(x, Precondition::ArtifactSourceAt { .. }))
                    .unwrap()
                {
                    *source_root = StoredPath::from(PathBuf::from("/outside"));
                }
            }),
            ("artifact source outside", |p| {
                if let StepAction::FileArtifact { source, .. } = &mut p.steps[1].action {
                    *source = StoredPath::from(PathBuf::from("/outside/file"));
                }
            }),
            ("artifact destination outside", |p| {
                if let StepAction::FileArtifact { destination, .. } = &mut p.steps[1].action {
                    *destination = StoredPath::from(PathBuf::from("/outside/file"));
                }
            }),
            ("artifact destination reserved", |p| {
                if let StepAction::FileArtifact { destination, .. } = &mut p.steps[1].action {
                    *destination = StoredPath::from(PathBuf::from("/r/w/.git"));
                }
            }),
            ("ArtifactSourceAt bytes", |p| {
                if let Precondition::ArtifactSourceAt { bytes, .. } = p.steps[1]
                    .preconditions
                    .iter_mut()
                    .find(|x| matches!(x, Precondition::ArtifactSourceAt { .. }))
                    .unwrap()
                {
                    *bytes = 99;
                }
            }),
            ("ArtifactSourceAt content", |p| {
                if let StepAction::FileArtifact { digest, .. } = &mut p.steps[1].action {
                    *digest = other_oid();
                }
            }),
            ("ArtifactSourceAt manifest digest", |p| {
                if let Precondition::ArtifactSourceAt {
                    manifest_digest, ..
                } = p.steps[1]
                    .preconditions
                    .iter_mut()
                    .find(|x| matches!(x, Precondition::ArtifactSourceAt { .. }))
                    .unwrap()
                {
                    *manifest_digest = other_oid();
                }
            }),
            ("copy link_target", |p| {
                if let StepAction::FileArtifact { link_target, .. } = &mut p.steps[1].action {
                    *link_target = Some(StoredPath::from(PathBuf::from("target")));
                }
            }),
            ("copy fingerprint", |p| {
                if let StepAction::FileArtifact { fingerprint, .. } = &mut p.steps[1].action {
                    *fingerprint = other_oid();
                }
            }),
            ("copy mode", |p| {
                if let StepAction::FileArtifact { mode_policy, .. } = &mut p.steps[1].action {
                    *mode_policy = crate::planner::FileModePolicy::Private;
                }
            }),
            ("selected-task mismatch", |p| {
                if let OperationIntent::Create(x) = &mut p.intent {
                    x.selected_tasks.insert("missing".into());
                }
            }),
            ("compensation present", |p| p.steps[1].compensation = None),
        ];
        for (name, mutate) in mutations {
            let mut plan = test_plan(2);
            mutate(&mut plan);
            let value = serde_json::to_value(plan).unwrap();
            let restored: OperationPlan = serde_json::from_value(value.clone()).unwrap();
            assert!(
                restored.validate_persisted().is_ok(),
                "{name}: persisted shape"
            );
            assert!(
                restored.validate_executable_plan().is_err(),
                "{name}: remained executable"
            );
            assert_shape_valid_but_not_executable(value);
            assert!(!name.is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_serialized_executable_plan_roundtrips() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let component = OsString::from_vec(vec![b'n', 0xff]);
        let worktree = StoredPath::from(PathBuf::from("/r/w").join(&component));
        let artifact = StoredPath::from(PathBuf::from("/r/source").join(&component));
        let mut plan = v3_test_plan(2);
        if let OperationIntent::Create(i) = &mut plan.intent {
            i.destination = Some(worktree.clone());
        }
        if let StepAction::CreateWorktree { destination, .. } = &mut plan.steps[0].action {
            *destination = worktree.clone();
        }
        for guard in &mut plan.steps[0].preconditions {
            match guard {
                Precondition::PathAbsent(path) => *path = worktree.clone(),
                Precondition::ParentSafe(path) => {
                    *path = worktree.as_path().parent().unwrap().to_owned().into()
                }
                _ => {}
            }
        }
        plan.preconditions = plan.steps[0].preconditions.clone();
        for post in &mut plan.steps[0].postconditions {
            if let Postcondition::WorktreeCreated { path, .. } = post {
                *path = worktree.clone();
            }
        }
        if let Some(Compensation::RemoveCreatedWorktree(x)) = &mut plan.steps[0].compensation {
            x.path = worktree.clone();
        }
        let artifact_destination = worktree.as_path().join("1");
        let target_digest =
            crate::planner::artifact_digest(artifact.as_path().as_os_str().as_encoded_bytes());
        let desired = SymlinkStateV3 {
            target: artifact.clone(),
            target_digest,
        };
        let manifest_digest = crate::planner::canonical_manifest_digest_v3(
            &[crate::planner::ManifestDescriptorV3::CreateSymlinkV3 {
                source_root: StoredPath::from(PathBuf::from("/r")),
                source: artifact.clone(),
                expected_source: ArtifactSourceExpectationV3::Regular(RegularFileStateV3 {
                    bytes: 1,
                    digest: ObjectId::new("0000000000000000000000000000000000000000").unwrap(),
                    mode: 0o644,
                }),
                destination: artifact_destination.clone().into(),
                desired: desired.clone(),
                sensitive: false,
                confirm: false,
            }],
            worktree.as_path(),
        );
        plan.steps[1].action = StepAction::CreateSymlinkV3 {
            rule: "rule-1".into(),
            source_root: StoredPath::from(PathBuf::from("/r")),
            source: artifact.clone(),
            expected_source: ArtifactSourceExpectationV3::Regular(RegularFileStateV3 {
                bytes: 1,
                digest: ObjectId::new("0000000000000000000000000000000000000000").unwrap(),
                mode: 0o644,
            }),
            destination: artifact_destination.clone().into(),
            desired: desired.clone(),
            manifest_digest: manifest_digest.clone(),
            sensitive: false,
            confirm: false,
        };
        for guard in &mut plan.steps[1].preconditions {
            if let Precondition::ArtifactSourceAtV3 {
                source,
                manifest_digest: guard_digest,
                ..
            } = guard
            {
                *source = artifact.clone();
                *guard_digest = manifest_digest.clone();
            }
            if let Precondition::PathAbsent(path) = guard {
                *path = artifact_destination.clone().into();
            }
        }
        plan.steps[1].compensation =
            Some(Compensation::RemoveCreatedArtifactV3(CreatedArtifactV3 {
                path: artifact_destination.clone().into(),
                expected: ArtifactStateV3::Symlink(desired),
                staging: None,
            }));
        if let OperationIntent::Create(intent) = &mut plan.intent
            && let Some(contract) = intent.artifact_rule_contracts.get_mut("rule-1")
        {
            contract.manifest_digest = manifest_digest.clone();
        }
        let restored: OperationPlan =
            serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
        assert_eq!(restored, plan);
        restored
            .validate_executable_plan()
            .unwrap_or_else(|error| panic!("non-UTF-8 plan: {error}"));
        let mut escaped = plan.clone();
        if let StepAction::CreateSymlinkV3 { destination, .. } = &mut escaped.steps[1].action {
            *destination = StoredPath::from(PathBuf::from("/r/w/../x"));
        }
        assert!(escaped.validate_executable_plan().is_err());
        if let StepAction::CreateSymlinkV3 { destination, .. } = &mut escaped.steps[1].action {
            *destination = StoredPath::from(PathBuf::from("/r/w/.git"));
        }
        assert!(escaped.validate_executable_plan().is_err());
    }

    fn relink_evidence_plan() -> OperationPlan {
        let mut plan = v3_test_plan(2);
        if let Some(Postcondition::WorktreeCreated { oid: value, .. }) = plan.steps[0]
            .postconditions
            .iter_mut()
            .find(|post| matches!(post, Postcondition::WorktreeCreated { .. }))
        {
            *value = oid();
        }
        for guard in &mut plan.steps[0].preconditions {
            if let Precondition::RefAt { oid: value, .. } = guard {
                *value = oid();
            }
        }
        for post in &mut plan.steps[0].postconditions {
            if let Postcondition::BranchCreated { oid: value, .. } = post {
                *value = oid();
            }
        }
        if let Some(Compensation::RemoveCreatedWorktree(value)) = &mut plan.steps[0].compensation {
            value.expected_oid = oid();
        }
        plan.preconditions = plan.steps[0].preconditions.clone();
        let checkout = StoredPath::from(PathBuf::from("/r/w"));
        let source_root = StoredPath::from(PathBuf::from("/r"));
        let source = StoredPath::from(PathBuf::from("/r/source/new"));
        let destination = StoredPath::from(PathBuf::from("/r/w/link"));
        let desired_new = SymlinkStateV3 {
            target: source.clone(),
            target_digest: crate::planner::artifact_digest(
                source.as_path().as_os_str().as_encoded_bytes(),
            ),
        };
        let old_target = StoredPath::from(PathBuf::from("/r/source/old"));
        let expected_old = SymlinkStateV3 {
            target: old_target.clone(),
            target_digest: crate::planner::artifact_digest(
                old_target.as_path().as_os_str().as_encoded_bytes(),
            ),
        };
        let replacement = crate::planner::artifact_staging_v3(
            plan.operation_id(),
            plan.steps[1].id(),
            crate::planner::ArtifactStagingRoleV3::RelinkReplacement,
            destination.as_path(),
        )
        .unwrap();
        let backup = crate::planner::artifact_staging_v3(
            plan.operation_id(),
            plan.steps[1].id(),
            crate::planner::ArtifactStagingRoleV3::RelinkBackup,
            destination.as_path(),
        )
        .unwrap();
        let descriptor = crate::planner::ManifestDescriptorV3::RelinkSymlinkV3 {
            source_root: source_root.clone(),
            source: source.clone(),
            expected_source: desired_new.clone(),
            checkout_oid: oid(),
            checkout_relative_path: StoredPath::from(PathBuf::from("link")),
            destination: destination.clone(),
            expected_old: expected_old.clone(),
            desired_new: desired_new.clone(),
            replacement_staging: replacement.clone(),
            backup_staging: backup.clone(),
            sensitive: false,
            confirm: false,
        };
        let digest =
            crate::planner::canonical_manifest_digest_v3(&[descriptor], checkout.as_path());
        plan.steps[1] = PlanStep::new(
            StepId::new("step-1").unwrap(),
            "step-1".into(),
            StepAction::RelinkSymlinkV3 {
                rule: "rule-1".into(),
                source_root: source_root.clone(),
                source: source.clone(),
                expected_source: desired_new.clone(),
                checkout_oid: oid(),
                checkout_relative_path: StoredPath::from(PathBuf::from("link")),
                destination: destination.clone(),
                expected_old: expected_old.clone(),
                desired_new: desired_new.clone(),
                replacement_staging: replacement.clone(),
                backup_staging: backup.clone(),
                manifest_digest: digest.clone(),
                sensitive: false,
                confirm: false,
            },
            vec![
                Precondition::ArtifactSourceAtV3 {
                    rule: "rule-1".into(),
                    source_root: source_root.clone(),
                    source: source.clone(),
                    expectation: ArtifactSourceExpectationV3::Symlink(desired_new.clone()),
                    manifest_digest: digest.clone(),
                },
                Precondition::TreeSymlinkAtV3 {
                    commit_oid: oid(),
                    checkout_relative_path: StoredPath::from(PathBuf::from("link")),
                    expected: expected_old.clone(),
                },
                Precondition::SymlinkAtV3 {
                    path: destination.clone(),
                    expected: expected_old.clone(),
                },
                Precondition::PathAbsent(replacement.path.clone()),
                Precondition::PathAbsent(backup.path.clone()),
            ],
            vec![],
            Some(Compensation::RestoreReplacedSymlinkV3(ReplacedSymlinkV3 {
                path: destination,
                expected_current: desired_new.clone(),
                restore: expected_old,
                replacement_staging: replacement,
                backup_staging: backup,
            })),
            false,
        )
        .unwrap();
        if let OperationIntent::Create(intent) = &mut plan.intent {
            intent.destination = Some(checkout);
            intent.artifact_rule_contracts.insert(
                "rule-1".into(),
                ArtifactRuleContract {
                    provenance: ArtifactSourceProvenance::Primary,
                    source_root,
                    manifest_digest: digest,
                },
            );
        }
        let risk = Risk {
            kind: RiskKind::ReplaceExistingSymlink,
            message: "replace existing symlink".into(),
        };
        plan.risks = vec![risk.clone()];
        plan.required_consents = vec![ConsentRequirement {
            id: ConsentId::new("file-rule:rule-1").unwrap(),
            risks: vec![risk],
        }];
        let consent = ConsentId::new("file-rule:rule-1").unwrap();
        plan.granted_consents.insert(consent.clone());
        if let OperationIntent::Create(intent) = &mut plan.intent {
            intent.granted_consents.insert(consent);
        }
        plan
    }

    fn refresh_relink_manifest(plan: &mut OperationPlan) {
        let (rule, digest) = {
            let StepAction::RelinkSymlinkV3 {
                rule,
                source_root,
                source,
                expected_source,
                checkout_oid,
                checkout_relative_path,
                destination,
                expected_old,
                desired_new,
                replacement_staging,
                backup_staging,
                sensitive,
                confirm,
                manifest_digest,
            } = plan.steps[1].action_mut()
            else {
                panic!("relink fixture")
            };
            let digest = crate::planner::canonical_manifest_digest_v3(
                &[crate::planner::ManifestDescriptorV3::RelinkSymlinkV3 {
                    source_root: source_root.clone(),
                    source: source.clone(),
                    expected_source: expected_source.clone(),
                    checkout_oid: checkout_oid.clone(),
                    checkout_relative_path: checkout_relative_path.clone(),
                    destination: destination.clone(),
                    expected_old: expected_old.clone(),
                    desired_new: desired_new.clone(),
                    replacement_staging: replacement_staging.clone(),
                    backup_staging: backup_staging.clone(),
                    sensitive: *sensitive,
                    confirm: *confirm,
                }],
                PathBuf::from("/r/w").as_path(),
            );
            *manifest_digest = digest.clone();
            (rule.clone(), digest)
        };
        for guard in plan.steps[1].preconditions_mut() {
            if let Precondition::ArtifactSourceAtV3 {
                manifest_digest: value,
                ..
            } = guard
            {
                *value = digest.clone();
            }
        }
        if let OperationIntent::Create(intent) = &mut plan.intent {
            intent
                .artifact_rule_contracts
                .get_mut(&rule)
                .unwrap()
                .manifest_digest = digest;
        }
    }

    #[test]
    fn relink_schema3_evidence_is_persisted_and_contract_executable() {
        let plan = relink_evidence_plan();
        assert!(plan.validate_persisted().is_ok());
        plan.validate_executable_plan()
            .unwrap_or_else(|error| panic!("{error}"));
    }

    #[test]
    fn relink_schema3_executable_mutation_matrix() {
        type PlanMutation = Box<dyn Fn(&mut OperationPlan)>;
        let cases: Vec<(&str, PlanMutation)> = vec![
            (
                "expected_source != desired_new",
                Box::new(|p| {
                    if let StepAction::RelinkSymlinkV3 {
                        expected_source, ..
                    } = p.steps[1].action_mut()
                    {
                        expected_source.target = StoredPath::from(PathBuf::from("/r/source/other"));
                    }
                }),
            ),
            (
                "checkout_oid differs from create WorktreeCreated OID",
                Box::new(|p| {
                    if let StepAction::RelinkSymlinkV3 { checkout_oid, .. } =
                        p.steps[1].action_mut()
                    {
                        *checkout_oid = other_oid();
                    }
                }),
            ),
            (
                "checkout_relative_path differs",
                Box::new(|p| {
                    if let StepAction::RelinkSymlinkV3 {
                        checkout_relative_path,
                        ..
                    } = p.steps[1].action_mut()
                    {
                        *checkout_relative_path = StoredPath::from(PathBuf::from("wrong"));
                    }
                }),
            ),
            (
                "missing source guard",
                Box::new(|p| {
                    p.steps[1]
                        .preconditions
                        .retain(|x| !matches!(x, Precondition::ArtifactSourceAtV3 { .. }))
                }),
            ),
            (
                "duplicate source guard",
                Box::new(|p| {
                    let x = p.steps[1]
                        .preconditions
                        .iter()
                        .find(|x| matches!(x, Precondition::ArtifactSourceAtV3 { .. }))
                        .unwrap()
                        .clone();
                    p.steps[1].preconditions.push(x);
                }),
            ),
            (
                "missing tree guard",
                Box::new(|p| {
                    p.steps[1]
                        .preconditions
                        .retain(|x| !matches!(x, Precondition::TreeSymlinkAtV3 { .. }))
                }),
            ),
            (
                "duplicate tree guard",
                Box::new(|p| {
                    let x = p.steps[1]
                        .preconditions
                        .iter()
                        .find(|x| matches!(x, Precondition::TreeSymlinkAtV3 { .. }))
                        .unwrap()
                        .clone();
                    p.steps[1].preconditions.push(x);
                }),
            ),
            (
                "missing SymlinkAtV3 raw destination guard",
                Box::new(|p| {
                    p.steps[1]
                        .preconditions
                        .retain(|x| !matches!(x, Precondition::SymlinkAtV3 { .. }))
                }),
            ),
            (
                "duplicate SymlinkAtV3 raw destination guard",
                Box::new(|p| {
                    let x = p.steps[1]
                        .preconditions
                        .iter()
                        .find(|x| matches!(x, Precondition::SymlinkAtV3 { .. }))
                        .unwrap()
                        .clone();
                    p.steps[1].preconditions.push(x);
                }),
            ),
            (
                "missing replacement staging PathAbsent",
                Box::new(|p| {
                    p.steps[1].preconditions.retain(|x| !matches!(x, Precondition::PathAbsent(path) if path.as_path().to_string_lossy().contains("relink-replacement")))
                }),
            ),
            (
                "duplicate replacement staging PathAbsent",
                Box::new(|p| {
                    let x = p.steps[1].preconditions.iter().find(|x| matches!(x, Precondition::PathAbsent(path) if path.as_path().to_string_lossy().contains("relink-replacement"))).unwrap().clone();
                    p.steps[1].preconditions.push(x);
                }),
            ),
            (
                "missing backup staging PathAbsent",
                Box::new(|p| {
                    p.steps[1].preconditions.retain(|x| !matches!(x, Precondition::PathAbsent(path) if path.as_path().to_string_lossy().contains("relink-backup")))
                }),
            ),
            (
                "duplicate backup staging PathAbsent",
                Box::new(|p| {
                    let x = p.steps[1].preconditions.iter().find(|x| matches!(x, Precondition::PathAbsent(path) if path.as_path().to_string_lossy().contains("relink-backup"))).unwrap().clone();
                    p.steps[1].preconditions.push(x);
                }),
            ),
            (
                "old raw target changed while digest is stale",
                Box::new(|p| {
                    if let StepAction::RelinkSymlinkV3 { expected_old, .. } =
                        p.steps[1].action_mut()
                    {
                        expected_old.target = StoredPath::from(PathBuf::from("/r/source/stale"));
                    }
                }),
            ),
            (
                "compensation current/restore/staging mismatch",
                Box::new(|p| {
                    if let Some(Compensation::RestoreReplacedSymlinkV3(c)) =
                        &mut p.steps[1].compensation
                    {
                        c.backup_staging.path = StoredPath::from(PathBuf::from("/r/bad"));
                    }
                }),
            ),
            (
                "create WorktreeCreated missing",
                Box::new(|p| {
                    p.steps[0]
                        .postconditions
                        .retain(|x| !matches!(x, Postcondition::WorktreeCreated { .. }))
                }),
            ),
            (
                "create WorktreeCreated duplicate",
                Box::new(|p| {
                    let x = p.steps[0]
                        .postconditions
                        .iter()
                        .find(|x| matches!(x, Postcondition::WorktreeCreated { .. }))
                        .unwrap()
                        .clone();
                    p.steps[0].postconditions.push(x);
                }),
            ),
            (
                "create WorktreeCreated wrong path",
                Box::new(|p| {
                    if let Some(Postcondition::WorktreeCreated { path, .. }) = p.steps[0]
                        .postconditions
                        .iter_mut()
                        .find(|x| matches!(x, Postcondition::WorktreeCreated { .. }))
                    {
                        *path = StoredPath::from(PathBuf::from("/r/other"));
                    }
                }),
            ),
            (
                "create WorktreeCreated wrong OID",
                Box::new(|p| {
                    if let Some(Postcondition::WorktreeCreated { oid, .. }) = p.steps[0]
                        .postconditions
                        .iter_mut()
                        .find(|x| matches!(x, Postcondition::WorktreeCreated { .. }))
                    {
                        *oid = other_oid();
                    }
                }),
            ),
            (
                "coordinated action/tree fields changed",
                Box::new(|p| {
                    if let StepAction::RelinkSymlinkV3 { checkout_oid, .. } =
                        p.steps[1].action_mut()
                    {
                        *checkout_oid = other_oid();
                    }
                    for x in p.steps[1].preconditions_mut() {
                        if let Precondition::TreeSymlinkAtV3 { commit_oid, .. } = x {
                            *commit_oid = other_oid();
                        }
                    }
                    refresh_relink_manifest(p);
                }),
            ),
        ];
        for (name, mutate) in cases {
            let mut plan = relink_evidence_plan();
            mutate(&mut plan);
            let wire = serde_json::to_value(plan).unwrap();
            let restored: OperationPlan = serde_json::from_value(wire).unwrap();
            assert!(restored.validate_persisted().is_ok(), "{name}: persisted");
            assert!(
                restored.validate_executable_plan().is_err(),
                "{name}: executable"
            );
        }
    }

    #[test]
    fn source_manifest_is_readable_but_not_executable_in_schema2_or_schema3() {
        for version in [2, 3] {
            let mut plan = v3_test_plan(1);
            plan.plan_schema_version = version;
            assert!(plan.validate_executable_plan().is_ok(), "schema {version}");
            let guard = Precondition::SourceManifest {
                rule: "legacy".into(),
                source: StoredPath::from(PathBuf::from("/r/source")),
                destination: StoredPath::from(PathBuf::from("/r/w/link")),
                digest: oid(),
            };
            plan.preconditions.push(guard.clone());
            plan.steps[0].preconditions.push(guard);
            let restored: OperationPlan =
                serde_json::from_value(serde_json::to_value(plan).unwrap()).unwrap();
            assert!(restored.validate_persisted().is_ok(), "schema {version}");
            assert!(
                restored.validate_executable_plan().is_err(),
                "schema {version}"
            );
        }
    }

    #[test]
    fn create_symlink_rejects_coherent_symlink_source_expectation() {
        let mut plan = v3_test_plan(2);
        let desired = match &plan.steps[1].action {
            StepAction::CreateSymlinkV3 { desired, .. } => desired.clone(),
            _ => panic!("fixture"),
        };
        let expectation = ArtifactSourceExpectationV3::Symlink(desired.clone());
        if let StepAction::CreateSymlinkV3 {
            expected_source,
            manifest_digest,
            source_root,
            source,
            destination,
            sensitive,
            confirm,
            ..
        } = plan.steps[1].action_mut()
        {
            *expected_source = expectation.clone();
            *manifest_digest = crate::planner::canonical_manifest_digest_v3(
                &[crate::planner::ManifestDescriptorV3::CreateSymlinkV3 {
                    source_root: source_root.clone(),
                    source: source.clone(),
                    expected_source: expectation.clone(),
                    destination: destination.clone(),
                    desired,
                    sensitive: *sensitive,
                    confirm: *confirm,
                }],
                PathBuf::from("/r/w").as_path(),
            );
        }
        let digest = match &plan.steps[1].action {
            StepAction::CreateSymlinkV3 {
                manifest_digest, ..
            } => manifest_digest.clone(),
            _ => unreachable!(),
        };
        for guard in &mut plan.steps[1].preconditions {
            if let Precondition::ArtifactSourceAtV3 {
                expectation: value,
                manifest_digest: md,
                ..
            } = guard
            {
                *value = expectation.clone();
                *md = digest.clone();
            }
        }
        if let OperationIntent::Create(intent) = &mut plan.intent {
            intent
                .artifact_rule_contracts
                .get_mut("rule-1")
                .unwrap()
                .manifest_digest = digest;
        }
        assert!(plan.validate_persisted().is_ok());
        let error = plan.validate_executable_plan().unwrap_err();
        assert!(error.contains("only") || error.contains("source expectation"));
    }
}
