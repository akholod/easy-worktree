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
            plan_schema_version: 2,
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
        if !matches!(self.plan_schema_version, 1 | 2) {
            return Err("unsupported operation plan schema".into());
        }
        self.validate_shape()?;
        if self.plan_schema_version == 2 && self.grants_match_intent() {
            let restored = Self::new(OperationPlanDraft {
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
        if self.plan_schema_version != 2 {
            return Err("schema-1 plans are read-only and not executable".into());
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
fn relative_is_safe(path: &std::path::Path) -> bool {
    path.components().all(|component| match component {
        std::path::Component::Normal(value) => {
            value != ".git" && value != ".ewtm" && value != "ewtm"
        }
        _ => false,
    })
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
pub(crate) fn test_plan(step_count: usize) -> OperationPlan {
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
        let manifest_digest = crate::planner::canonical_manifest_digest(
            &[crate::planner::ManifestDigestArtifact {
                source_root: repository.primary_root.clone(),
                source: StoredPath::from(std::path::PathBuf::from(format!("/r/source/{index}"))),
                destination: path.clone(),
                kind: crate::planner::FileArtifactKind::CopyFile,
                bytes: 1,
                digest: zero.clone(),
                fingerprint: zero.clone(),
                link_target: None,
                sensitive: false,
                confirm: false,
                mode_policy: crate::planner::FileModePolicy::PreserveSafe,
            }],
            std::path::Path::new("/r/w"),
        );
        steps.push(
            PlanStep::new(
                StepId::new(format!("step-{index}")).unwrap(),
                format!("step-{index}"),
                StepAction::FileArtifact {
                    rule: format!("rule-{index}"),
                    kind: crate::planner::FileArtifactKind::CopyFile,
                    source: StoredPath::from(std::path::PathBuf::from(format!(
                        "/r/source/{index}"
                    ))),
                    destination: path.clone(),
                    bytes: 1,
                    digest: zero.clone(),
                    fingerprint: zero.clone(),
                    link_target: None,
                    manifest_digest: manifest_digest.clone(),
                    sensitive: false,
                    confirm: false,
                    mode_policy: crate::planner::FileModePolicy::PreserveSafe,
                },
                vec![
                    Precondition::ArtifactSourceAt {
                        rule: format!("rule-{index}"),
                        source_root: repository.primary_root.clone(),
                        source: StoredPath::from(std::path::PathBuf::from(format!(
                            "/r/source/{index}"
                        ))),
                        destination: path.clone(),
                        bytes: 1,
                        digest: zero.clone(),
                        manifest_digest: manifest_digest.clone(),
                    },
                    Precondition::PathAbsent(path.clone()),
                ],
                vec![],
                Some(Compensation::RemoveCreatedArtifact(CreatedArtifact {
                    path,
                    fingerprint: zero.clone(),
                })),
                false,
            )
            .unwrap(),
        );
    }
    if let OperationIntent::Create(value) = &mut intent {
        value.current_worktree_root = Some(repository.primary_root.clone());
        for step in &steps {
            if let StepAction::FileArtifact {
                rule,
                manifest_digest,
                ..
            } = step.action()
            {
                let source_root = step.preconditions().iter().find_map(|guard| match guard {
                    Precondition::ArtifactSourceAt { source_root, .. } => Some(source_root.clone()),
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
        assert_eq!(value["plan_schema_version"], 2);
        assert!(value.get("status").is_none());
        assert_eq!(plan, serde_json::from_value(value).unwrap());
    }

    #[test]
    fn archived_schema_one_file_artifact_is_readable_but_not_executable() {
        let mut wire = serde_json::to_value(test_plan(2)).unwrap();
        wire["plan_schema_version"] = serde_json::json!(1);
        let action = &mut wire["steps"][1]["action"]["FileArtifact"];
        action.as_object_mut().unwrap().remove("sensitive");
        action.as_object_mut().unwrap().remove("confirm");
        action.as_object_mut().unwrap().remove("mode_policy");
        let restored: OperationPlan = serde_json::from_value(wire).unwrap();
        assert_eq!(restored.plan_schema_version(), 1);
        assert!(restored.validate_persisted().is_ok());
        assert!(restored.validate_executable_plan().is_err());
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
        let mut plan = test_plan(2);
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
        if let StepAction::FileArtifact {
            source,
            destination,
            ..
        } = &mut plan.steps[1].action
        {
            *source = artifact.clone();
            *destination = artifact_destination.clone().into();
        }
        for guard in &mut plan.steps[1].preconditions {
            if let Precondition::ArtifactSourceAt {
                source,
                destination,
                ..
            } = guard
            {
                *source = artifact.clone();
                *destination = artifact_destination.clone().into();
            }
            if let Precondition::PathAbsent(path) = guard {
                *path = artifact_destination.clone().into();
            }
        }
        if let Some(Compensation::RemoveCreatedArtifact(x)) = &mut plan.steps[1].compensation {
            x.path = artifact_destination.into();
        }
        let digest = if let StepAction::FileArtifact {
            source,
            destination,
            kind,
            bytes,
            digest,
            fingerprint,
            link_target,
            sensitive,
            confirm,
            mode_policy,
            ..
        } = plan.steps[1].action()
        {
            crate::planner::canonical_manifest_digest(
                &[crate::planner::ManifestDigestArtifact {
                    source_root: StoredPath::from(PathBuf::from("/r")),
                    source: source.clone(),
                    destination: destination.clone(),
                    kind: *kind,
                    bytes: *bytes,
                    digest: digest.clone(),
                    fingerprint: fingerprint.clone(),
                    link_target: link_target.clone(),
                    sensitive: *sensitive,
                    confirm: *confirm,
                    mode_policy: *mode_policy,
                }],
                worktree.as_path(),
            )
        } else {
            unreachable!()
        };
        if let StepAction::FileArtifact {
            manifest_digest, ..
        } = &mut plan.steps[1].action
        {
            *manifest_digest = digest.clone();
        }
        for guard in &mut plan.steps[1].preconditions {
            if let Precondition::ArtifactSourceAt {
                manifest_digest, ..
            } = guard
            {
                *manifest_digest = digest.clone();
            }
        }
        if let OperationIntent::Create(intent) = &mut plan.intent
            && let Some(contract) = intent.artifact_rule_contracts.get_mut("rule-1")
        {
            contract.manifest_digest = digest.clone();
        }
        let restored: OperationPlan =
            serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
        assert_eq!(restored, plan);
        restored
            .validate_executable_plan()
            .unwrap_or_else(|error| panic!("non-UTF-8 plan: {error}"));
        let mut escaped = plan.clone();
        if let StepAction::FileArtifact { destination, .. } = &mut escaped.steps[1].action {
            *destination = StoredPath::from(PathBuf::from("/r/w/../x"));
        }
        assert!(escaped.validate_executable_plan().is_err());
        if let StepAction::FileArtifact { destination, .. } = &mut escaped.steps[1].action {
            *destination = StoredPath::from(PathBuf::from("/r/w/.git"));
        }
        assert!(escaped.validate_executable_plan().is_err());
    }
}
