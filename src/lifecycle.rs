//! Pure, serializable lifecycle vocabulary. No Git or filesystem operations live here.

use crate::domain::StoredPath;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, str::FromStr};
use uuid::Uuid;

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
    WorktreeClean {
        path: StoredPath,
    },
    NoOngoingGitOperation {
        path: StoredPath,
    },
    BranchNotElsewhere(BranchName),
    BranchNotCheckedOut(BranchName),
    SourceManifest {
        rule: String,
        source: StoredPath,
        destination: StoredPath,
        digest: ObjectId,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Postcondition {
    WorktreeCreated { path: StoredPath, oid: ObjectId },
    WorktreeRemoved { path: StoredPath, oid: ObjectId },
    BranchCreated { branch: BranchName, oid: ObjectId },
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
    pub fn new(
        id: StepId,
        name: String,
        action: StepAction,
        preconditions: Vec<Precondition>,
        postconditions: Vec<Postcondition>,
        compensation: Option<Compensation>,
        irreversible: bool,
    ) -> Result<Self, String> {
        if name.is_empty() {
            return Err("step name cannot be empty".into());
        }
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
        let intent_repository = match &intent {
            OperationIntent::Create(value) => &value.repository,
            OperationIntent::Remove(value) => &value.repository,
        };
        if intent_repository != &repository {
            return Err("plan repository and intent repository mismatch".into());
        }
        Ok(Self {
            plan_schema_version: 1,
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
    pub fn risks(&self) -> &[Risk] {
        &self.risks
    }
    pub fn required_consents(&self) -> &[ConsentRequirement] {
        &self.required_consents
    }
    pub fn granted_consents(&self) -> &BTreeSet<ConsentId> {
        &self.granted_consents
    }
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
        assert_eq!(value["plan_schema_version"], 1);
        assert!(value.get("status").is_none());
        assert_eq!(plan, serde_json::from_value(value).unwrap());
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
                    }
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
}
