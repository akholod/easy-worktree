//! Deterministic, read-only lifecycle planning.

use crate::{
    config::CreateConfig,
    domain::{StoredPath, WorktreeClass},
    lifecycle::*,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestinationState {
    Absent,
    Present,
    Dangling,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationFacts {
    pub path: StoredPath,
    pub state: DestinationState,
    pub parent: StoredPath,
    pub parent_safe: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateSourceFacts {
    NewBranch {
        branch: BranchName,
        base_ref: RefName,
        base_oid: ObjectId,
        branch_absent: bool,
    },
    ExistingLocal {
        branch: BranchName,
        branch_oid: ObjectId,
        not_checked_out: bool,
    },
    RemoteTracking {
        remote: RemoteName,
        remote_branch: BranchName,
        remote_oid: ObjectId,
        local_branch: BranchName,
        local_absent: bool,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileArtifactKind {
    CopyFile,
    CreateSymlink,
    RelinkSymlink,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileArtifact {
    pub kind: FileArtifactKind,
    pub source: StoredPath,
    pub destination: StoredPath,
    pub bytes: u64,
    pub digest: ObjectId,
    pub fingerprint: ObjectId,
    pub link_target: Option<StoredPath>,
    pub sensitive: bool,
    pub confirm: bool,
    pub conflict: bool,
    pub overlap: bool,
    pub replace_symlink: bool,
    pub compensation: Option<Compensation>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileActionManifest {
    pub rule: String,
    pub artifacts: Vec<FileArtifact>,
    pub digest: ObjectId,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub name: String,
    pub argv: CommandArgv,
    pub cwd: StoredPath,
    pub enabled: bool,
    pub post_create: bool,
    pub required: bool,
    pub environment_allowlist: Vec<EnvironmentName>,
}
#[derive(Debug, Clone)]
pub struct CreatePlanInput {
    pub operation_id: OperationId,
    pub repository: RepositoryIdentity,
    pub intent: CreateIntent,
    pub bare: bool,
    pub primary_count: usize,
    pub invocation_cwd: StoredPath,
    pub primary_root: StoredPath,
    pub current_worktree_root: StoredPath,
    pub destination: DestinationFacts,
    pub source_facts: CreateSourceFacts,
    pub branch_checked_out: bool,
    pub branch_collision: bool,
    pub known_rules: BTreeSet<String>,
    pub enabled_rules: BTreeSet<String>,
    pub known_tasks: BTreeSet<String>,
    pub manifests: Vec<FileActionManifest>,
    pub tasks: Vec<TaskSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveFacts {
    pub repository: RepositoryIdentity,
    pub class: WorktreeClass,
    pub locked: bool,
    pub prunable: bool,
    pub ongoing: bool,
    pub oid_matches: bool,
    pub branch_elsewhere: bool,
    pub dirty: bool,
    pub local_branch_safe_to_delete: bool,
    pub branch: BranchName,
    pub branch_oid: ObjectId,
    pub worktree_oid: ObjectId,
    pub remote_branch: Option<RemoteBranch>,
    pub remote_branch_oid: Option<ObjectId>,
    pub remote_is_default: bool,
    pub path: StoredPath,
}
#[derive(Debug, Clone)]
pub struct RemovePlanInput {
    pub operation_id: OperationId,
    pub intent: RemoveIntent,
    pub facts: RemoveFacts,
}

pub fn new_operation_id() -> OperationId {
    OperationId::new(Uuid::new_v4())
}

pub fn destination_for(
    config: Option<&CreateConfig>,
    primary: &Path,
    branch: &str,
    cwd: &Path,
) -> PathBuf {
    destination_for_options(
        config.and_then(|c| c.worktree_root.as_deref()),
        config.and_then(|c| c.directory_prefix.as_deref()),
        primary,
        branch,
        cwd,
    )
}

pub fn destination_for_options(
    worktree_root: Option<&str>,
    directory_prefix: Option<&str>,
    primary: &Path,
    branch: &str,
    cwd: &Path,
) -> PathBuf {
    let root = match worktree_root {
        None | Some("") => primary.parent().unwrap_or(primary).to_path_buf(),
        Some(value) => {
            let value = PathBuf::from(value);
            if value.is_absolute() {
                value
            } else {
                primary.parent().unwrap_or(primary).join(value)
            }
        }
    };
    let prefix = directory_prefix.map_or_else(
        || {
            format!(
                "{}-",
                primary
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("repo")
            )
        },
        str::to_owned,
    );
    let result = root.join(format!("{prefix}{}", branch.replace('/', "-")));
    normalize_lexical(if result.is_absolute() {
        result
    } else {
        cwd.join(result)
    })
}

pub fn normalize_lexical(path: PathBuf) -> PathBuf {
    let absolute = path.is_absolute();
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if parts
                    .last()
                    .is_some_and(|part| *part != std::path::Component::RootDir)
                {
                    parts.pop();
                } else if !absolute {
                    parts.push(component);
                }
            }
            other => parts.push(other),
        }
    }
    let mut result = PathBuf::new();
    for part in parts {
        result.push(part.as_os_str());
    }
    result
}

fn step(
    id: &str,
    name: &str,
    action: StepAction,
    pre: Vec<Precondition>,
    post: Vec<Postcondition>,
    compensation: Option<Compensation>,
    irreversible: bool,
) -> Result<PlanStep, String> {
    PlanStep::new(
        StepId::new(id)?,
        name.into(),
        action,
        pre,
        post,
        compensation,
        irreversible,
    )
}
fn branch_of(source: &CreateSource) -> &BranchName {
    match source {
        CreateSource::NewBranch { branch, .. } | CreateSource::ExistingLocal { branch } => branch,
        CreateSource::RemoteTracking { local_branch, .. } => local_branch,
    }
}

fn validate_source(
    intent: &CreateIntent,
    facts: &CreateSourceFacts,
) -> Result<(ObjectId, bool), String> {
    match (&intent.source, facts) {
        (
            CreateSource::NewBranch {
                branch,
                base: Some(base),
            },
            CreateSourceFacts::NewBranch {
                branch: fact_branch,
                base_ref,
                base_oid,
                branch_absent,
            },
        ) if branch == fact_branch && base == base_ref && *branch_absent => {
            Ok((base_oid.clone(), true))
        }
        (
            CreateSource::NewBranch { branch, base: None },
            CreateSourceFacts::NewBranch {
                branch: fact_branch,
                base_ref: _,
                base_oid,
                branch_absent,
            },
        ) if branch == fact_branch && *branch_absent => Ok((base_oid.clone(), true)),
        (
            CreateSource::ExistingLocal { branch },
            CreateSourceFacts::ExistingLocal {
                branch: fact_branch,
                branch_oid,
                not_checked_out,
            },
        ) if branch == fact_branch && *not_checked_out => Ok((branch_oid.clone(), false)),
        (
            CreateSource::RemoteTracking {
                remote,
                remote_branch,
                local_branch,
            },
            CreateSourceFacts::RemoteTracking {
                remote: fact_remote,
                remote_branch: fact_remote_branch,
                remote_oid,
                local_branch: fact_local,
                local_absent,
            },
        ) if remote == fact_remote
            && remote_branch == fact_remote_branch
            && local_branch == fact_local
            && *local_absent =>
        {
            Ok((remote_oid.clone(), true))
        }
        _ => Err("create source does not match resolved source facts".into()),
    }
}

pub fn plan_create(input: CreatePlanInput) -> Result<OperationPlan, String> {
    if input.bare {
        return Err("repository is bare".into());
    }
    if input.primary_count != 1 {
        return Err("repository must have exactly one primary worktree".into());
    }
    if input.destination.state != DestinationState::Absent {
        return Err("destination must be absent, not present or dangling".into());
    }
    if !input.destination.parent_safe {
        return Err("destination parent is unsafe".into());
    }
    if input.branch_checked_out || input.branch_collision {
        return Err("branch is checked out or collides".into());
    }
    let (source_oid, branch_created) = validate_source(&input.intent, &input.source_facts)?;
    if input.intent.repository != input.repository {
        return Err("intent repository identity mismatch".into());
    }
    if input
        .intent
        .destination
        .as_ref()
        .is_some_and(|path| path != &input.destination.path)
    {
        return Err("destination facts do not match create intent".into());
    }
    for rule in &input.intent.skipped_rules {
        if !input.known_rules.contains(rule) {
            return Err(format!("unknown skipped rule: {rule}"));
        }
    }
    for task in &input.intent.selected_tasks {
        if !input.known_tasks.contains(task) {
            return Err(format!("unknown selected task: {task}"));
        }
    }
    let selected_specs: Vec<_> = input
        .tasks
        .iter()
        .filter(|task| input.intent.selected_tasks.contains(&task.name))
        .collect();
    if selected_specs.len() != input.intent.selected_tasks.len() {
        return Err("every selected task must have exactly one specification".into());
    }
    if selected_specs
        .iter()
        .any(|task| !task.enabled || !task.post_create)
    {
        return Err("selected tasks must be enabled post_create tasks".into());
    }
    if !input.intent.skipped_rules.is_subset(&input.enabled_rules) {
        return Err("skipped rule must be enabled".into());
    }
    if !input.enabled_rules.is_subset(&input.known_rules) {
        return Err("enabled rule is unknown".into());
    }
    let manifests = input.manifests;
    let mut rule_counts = BTreeMap::<String, usize>::new();
    let mut artifacts = Vec::new();
    for manifest in manifests {
        if !input.known_rules.contains(&manifest.rule) {
            return Err(format!("unknown rule: {}", manifest.rule));
        }
        if input.intent.skipped_rules.contains(&manifest.rule)
            || !input.enabled_rules.contains(&manifest.rule)
        {
            return Err(format!(
                "manifest selected for disabled or skipped rule: {}",
                manifest.rule
            ));
        }
        *rule_counts.entry(manifest.rule.clone()).or_default() += 1;
        for artifact in manifest.artifacts {
            if artifact.conflict || artifact.overlap {
                return Err(format!(
                    "manifest conflict or overlap in rule {}",
                    manifest.rule
                ));
            }
            artifacts.push((manifest.rule.clone(), manifest.digest.clone(), artifact));
        }
    }
    for rule in &input.enabled_rules {
        if !input.intent.skipped_rules.contains(rule) && rule_counts.get(rule) != Some(&1) {
            return Err(format!(
                "enabled rule must have exactly one manifest: {rule}"
            ));
        }
    }
    artifacts.sort_by(|(left_rule, _, left), (right_rule, _, right)| {
        left_rule
            .cmp(right_rule)
            .then_with(|| left.destination.as_path().cmp(right.destination.as_path()))
    });
    let destination = input
        .intent
        .destination
        .clone()
        .unwrap_or_else(|| input.destination.path.clone());
    let mut preconditions = vec![
        Precondition::CommonDirectory(input.repository.common_dir.clone()),
        Precondition::ExactlyOnePrimary,
        Precondition::BareRepositoryFalse,
        Precondition::PathAbsent(destination.clone()),
        Precondition::ParentSafe(input.destination.parent.clone()),
    ];
    match &input.source_facts {
        CreateSourceFacts::NewBranch {
            branch, base_oid, ..
        } => {
            preconditions.push(Precondition::RefAbsent(RefName::new(branch.as_str())?));
            if let CreateSourceFacts::NewBranch { base_ref, .. } = &input.source_facts {
                preconditions.push(Precondition::RefAt {
                    reference: base_ref.clone(),
                    oid: base_oid.clone(),
                });
            }
        }
        CreateSourceFacts::ExistingLocal {
            branch, branch_oid, ..
        } => preconditions.push(Precondition::RefAt {
            reference: RefName::new(branch.as_str())?,
            oid: branch_oid.clone(),
        }),
        CreateSourceFacts::RemoteTracking {
            remote,
            remote_branch,
            remote_oid,
            local_branch,
            ..
        } => {
            preconditions.push(Precondition::RemoteRefAt {
                remote: remote.clone(),
                branch: remote_branch.clone(),
                oid: remote_oid.clone(),
            });
            preconditions.push(Precondition::RefAbsent(RefName::new(
                local_branch.as_str(),
            )?));
        }
    }
    preconditions.push(Precondition::BranchNotElsewhere(
        branch_of(&input.intent.source).clone(),
    ));
    preconditions.push(Precondition::BranchNotCheckedOut(
        branch_of(&input.intent.source).clone(),
    ));
    let mut steps = Vec::new();
    let create_comp = Compensation::RemoveCreatedWorktree(CreatedWorktree {
        path: destination.clone(),
        branch: branch_of(&input.intent.source).clone(),
        expected_oid: source_oid.clone(),
        branch_was_created: branch_created,
    });
    let source_action = match (&input.intent.source, &input.source_facts) {
        (
            CreateSource::NewBranch { branch, base: None },
            CreateSourceFacts::NewBranch { base_ref, .. },
        ) => CreateSource::NewBranch {
            branch: branch.clone(),
            base: Some(base_ref.clone()),
        },
        _ => input.intent.source.clone(),
    };
    let mut create_post = vec![Postcondition::WorktreeCreated {
        path: destination.clone(),
        oid: source_oid.clone(),
    }];
    if branch_created {
        create_post.push(Postcondition::BranchCreated {
            branch: branch_of(&input.intent.source).clone(),
            oid: source_oid.clone(),
        });
    }
    steps.push(step(
        "create.worktree",
        "create.worktree",
        StepAction::CreateWorktree {
            destination: destination.clone(),
            source: source_action,
        },
        preconditions.clone(),
        create_post,
        Some(create_comp),
        false,
    )?);
    let mut per_rule_index = BTreeMap::<String, usize>::new();
    let mut risks = Vec::new();
    let mut consent_risks = BTreeMap::<ConsentId, Vec<Risk>>::new();
    for (rule, manifest_digest, artifact) in artifacts {
        let index = per_rule_index
            .entry(rule.clone())
            .and_modify(|v| *v += 1)
            .or_insert(1);
        let id = format!("file.{rule}.{index:04}");
        let mut artifact_risks = Vec::new();
        if artifact.sensitive || artifact.confirm {
            artifact_risks.push((
                RiskKind::SensitiveMaterialization,
                "materialize sensitive material",
            ));
        }
        if artifact.replace_symlink {
            artifact_risks.push((
                RiskKind::ReplaceExistingSymlink,
                "replace an existing symlink",
            ));
        }
        let mut rule_risks = Vec::new();
        for (kind, message) in artifact_risks {
            let risk = Risk {
                kind,
                message: message.into(),
            };
            risks.push(risk.clone());
            rule_risks.push(risk);
        }
        if !rule_risks.is_empty() {
            consent_risks
                .entry(ConsentId::new(format!("file-rule:{rule}"))?)
                .or_default()
                .extend(rule_risks);
        }
        let Some(compensation) = &artifact.compensation else {
            return Err(format!(
                "file artifact in rule {rule} lacks safe compensation"
            ));
        };
        match compensation {
            Compensation::RemoveCreatedArtifact(value)
                if value.path == artifact.destination
                    && value.fingerprint == artifact.fingerprint
                    && !artifact.replace_symlink => {}
            Compensation::RestoreReplacedSymlink(value)
                if value.path == artifact.destination
                    && value.expected_current == artifact.fingerprint
                    && artifact.link_target.as_ref() == Some(&value.original_target)
                    && artifact.replace_symlink => {}
            _ => {
                return Err(format!(
                    "file artifact in rule {rule} has unsafe compensation"
                ));
            }
        }
        let action = StepAction::FileArtifact {
            rule: rule.clone(),
            kind: artifact.kind,
            source: artifact.source,
            destination: artifact.destination,
            bytes: artifact.bytes,
            digest: artifact.digest,
            fingerprint: artifact.fingerprint,
            link_target: artifact.link_target,
            manifest_digest,
        };
        let manifest_precondition = match &action {
            StepAction::FileArtifact {
                source,
                destination,
                digest,
                ..
            } => Precondition::SourceManifest {
                rule: rule.clone(),
                source: source.clone(),
                destination: destination.clone(),
                digest: digest.clone(),
            },
            _ => unreachable!(),
        };
        steps.push(step(
            &id,
            &id,
            action,
            vec![manifest_precondition],
            vec![],
            artifact.compensation,
            false,
        )?);
    }
    let mut tasks = input.tasks;
    tasks.sort_by(|a, b| a.name.cmp(&b.name));
    for task in tasks.into_iter().filter(|task| {
        task.enabled && task.post_create && input.intent.selected_tasks.contains(&task.name)
    }) {
        let task_risk = Risk {
            kind: RiskKind::ExecuteTask,
            message: format!("execute task {}", task.name),
        };
        risks.push(task_risk.clone());
        consent_risks.insert(
            ConsentId::new(format!("task:{}", task.name))?,
            vec![task_risk],
        );
        let id = format!("task.{}", task.name);
        steps.push(step(
            &id,
            &id,
            StepAction::RunTask {
                name: task.name,
                argv: task.argv,
                cwd: task.cwd,
                required: task.required,
                environment_allowlist: task.environment_allowlist,
            },
            vec![],
            vec![],
            None,
            false,
        )?);
    }
    let required: Vec<_> = consent_risks
        .into_iter()
        .map(|(id, mut values)| {
            values.sort_by_key(|risk| risk.kind as u8);
            values.dedup_by(|a, b| a.kind == b.kind && a.message == b.message);
            ConsentRequirement { id, risks: values }
        })
        .collect();
    let required_ids: BTreeSet<_> = required.iter().map(|c| c.id.clone()).collect();
    if !input.intent.granted_consents.is_subset(&required_ids) {
        return Err("intent contains unknown or unrequired granted consent".into());
    }
    let grants = input.intent.granted_consents.clone();
    OperationPlan::new(OperationPlanDraft {
        operation_id: input.operation_id,
        kind: OperationKind::Create,
        repository: input.repository,
        intent: OperationIntent::Create(input.intent),
        preconditions,
        steps,
        risks,
        required_consents: required,
        granted_consents: grants,
    })
}

pub fn plan_remove(input: RemovePlanInput) -> Result<OperationPlan, String> {
    let f = &input.facts;
    if f.class != WorktreeClass::Linked {
        return Err("only linked worktrees can be removed".into());
    }
    if f.locked || f.prunable || f.ongoing || !f.oid_matches || f.branch_elsewhere {
        return Err("worktree safety precondition failed".into());
    }
    if f.dirty && !input.intent.allow_dirty_removal {
        return Err("dirty worktree requires allow-dirty-removal".into());
    }
    if input.intent.worktree != f.path {
        return Err("intent worktree does not match facts".into());
    }
    if input.intent.repository != f.repository {
        return Err("intent repository does not match facts".into());
    }
    let mut pre = vec![
        Precondition::CommonDirectory(input.intent.repository.common_dir.clone()),
        Precondition::WorktreeRegistered {
            path: f.path.clone(),
            oid: f.worktree_oid.clone(),
        },
        Precondition::WorktreeClass {
            path: f.path.clone(),
            class: f.class,
        },
        Precondition::WorktreeUnlocked {
            path: f.path.clone(),
        },
        Precondition::WorktreeNotPrunable {
            path: f.path.clone(),
        },
        Precondition::NoOngoingGitOperation {
            path: f.path.clone(),
        },
        Precondition::BranchNotElsewhere(f.branch.clone()),
    ];
    if !f.dirty {
        pre.push(Precondition::WorktreeClean {
            path: f.path.clone(),
        });
    }
    let mut steps = vec![step(
        "remove.worktree",
        "remove.worktree",
        StepAction::RemoveWorktree {
            path: f.path.clone(),
        },
        pre.clone(),
        vec![Postcondition::WorktreeRemoved {
            path: f.path.clone(),
            oid: f.worktree_oid.clone(),
        }],
        None,
        true,
    )?];
    let mut risks = vec![Risk {
        kind: RiskKind::IrreversibleStep,
        message: "remove worktree cannot be automatically recreated".into(),
    }];
    let mut required = Vec::new();
    if f.dirty {
        risks.push(Risk {
            kind: RiskKind::DirtyDataLoss,
            message: "remove dirty worktree".into(),
        });
    }
    if input.intent.delete_local_branch {
        if !f.local_branch_safe_to_delete && !input.intent.force_delete_local_branch {
            return Err("local branch is not safe to delete".into());
        }
        let local_risk = Risk {
            kind: RiskKind::DeleteLocalBranch,
            message: "delete local branch".into(),
        };
        risks.push(local_risk);
        if input.intent.force_delete_local_branch {
            risks.push(Risk {
                kind: RiskKind::ForceDeleteLocalBranch,
                message: "force delete local branch".into(),
            });
        }
        let mut local_pre = vec![
            Precondition::CommonDirectory(input.intent.repository.common_dir.clone()),
            Precondition::BranchNotElsewhere(f.branch.clone()),
            Precondition::BranchNotCheckedOut(f.branch.clone()),
        ];
        local_pre.push(Precondition::RefAt {
            reference: RefName::new(f.branch.as_str())?,
            oid: f.branch_oid.clone(),
        });
        steps.push(step(
            "remove.local-branch",
            "remove.local-branch",
            StepAction::DeleteLocalBranch {
                branch: f.branch.clone(),
            },
            local_pre,
            vec![Postcondition::BranchDeleted(f.branch.clone())],
            None,
            true,
        )?);
    }
    if let Some(remote) = &input.intent.delete_remote_branch {
        if f.remote_branch.as_ref() != Some(remote) {
            return Err("remote branch facts do not match requested target".into());
        }
        if f.remote_is_default {
            return Err("cannot delete the default remote branch".into());
        }
        let remote_oid = f
            .remote_branch_oid
            .clone()
            .ok_or("remote deletion requires an exact remote ref OID")?;
        risks.push(Risk {
            kind: RiskKind::DeleteRemoteBranch,
            message: "delete remote branch".into(),
        });
        risks.push(Risk {
            kind: RiskKind::IrreversibleStep,
            message: "delete remote branch is irreversible".into(),
        });
        let mut remote_pre = vec![
            Precondition::CommonDirectory(input.intent.repository.common_dir.clone()),
            Precondition::BranchNotElsewhere(f.branch.clone()),
            Precondition::RemoteBranchNotDefault(remote.clone()),
        ];
        remote_pre.push(Precondition::RemoteRefAt {
            remote: remote.remote.clone(),
            branch: remote.branch.clone(),
            oid: remote_oid,
        });
        steps.push(step(
            "remove.remote-branch",
            "remove.remote-branch",
            StepAction::DeleteRemoteBranch {
                target: remote.clone(),
            },
            remote_pre,
            vec![Postcondition::RemoteBranchDeleted(remote.clone())],
            None,
            true,
        )?);
    }
    if f.dirty {
        required.push(ConsentRequirement {
            id: ConsentId::new("remove:dirty")?,
            risks: vec![Risk {
                kind: RiskKind::DirtyDataLoss,
                message: "remove dirty worktree".into(),
            }],
        });
    }
    if input.intent.delete_local_branch {
        let mut local_risks = vec![Risk {
            kind: RiskKind::DeleteLocalBranch,
            message: "delete local branch".into(),
        }];
        if input.intent.force_delete_local_branch {
            local_risks.push(Risk {
                kind: RiskKind::ForceDeleteLocalBranch,
                message: "force delete local branch".into(),
            });
        }
        required.push(ConsentRequirement {
            id: ConsentId::new("remove:local-branch")?,
            risks: vec![local_risks[0].clone()],
        });
        if input.intent.force_delete_local_branch {
            required.push(ConsentRequirement {
                id: ConsentId::new("remove:force-local-branch")?,
                risks: vec![local_risks[1].clone()],
            });
        }
    }
    if let Some(remote) = &input.intent.delete_remote_branch {
        required.push(ConsentRequirement {
            id: ConsentId::new(format!("remove:remote:{}/{}", remote.remote, remote.branch))?,
            risks: vec![Risk {
                kind: RiskKind::DeleteRemoteBranch,
                message: "delete remote branch".into(),
            }],
        });
    }
    let required_ids: BTreeSet<_> = required.iter().map(|c| c.id.clone()).collect();
    if !input.intent.granted_consents.is_subset(&required_ids) {
        return Err("intent contains unknown or unrequired granted consent".into());
    }
    OperationPlan::new(OperationPlanDraft {
        operation_id: input.operation_id,
        kind: OperationKind::Remove,
        repository: input.intent.repository.clone(),
        intent: OperationIntent::Remove(input.intent.clone()),
        preconditions: pre,
        steps,
        risks,
        required_consents: required,
        granted_consents: input.intent.granted_consents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeSet, path::PathBuf};

    fn oid() -> ObjectId {
        ObjectId::new("0123456789012345678901234567890123456789").unwrap()
    }
    fn repo() -> RepositoryIdentity {
        RepositoryIdentity {
            common_dir: StoredPath::from(PathBuf::from("/r/.git")),
            primary_root: StoredPath::from(PathBuf::from("/r")),
            repository_oid: oid(),
        }
    }
    fn destination() -> DestinationFacts {
        DestinationFacts {
            path: StoredPath::from(PathBuf::from("/w/feature")),
            state: DestinationState::Absent,
            parent: StoredPath::from(PathBuf::from("/w")),
            parent_safe: true,
        }
    }
    fn base_intent(source: CreateSource) -> CreateIntent {
        CreateIntent {
            repository: repo(),
            source,
            destination: None,
            selected_tasks: BTreeSet::new(),
            skipped_rules: BTreeSet::new(),
            granted_consents: BTreeSet::new(),
        }
    }
    fn input(source: CreateSource, facts: CreateSourceFacts) -> CreatePlanInput {
        CreatePlanInput {
            operation_id: new_operation_id(),
            repository: repo(),
            intent: base_intent(source),
            bare: false,
            primary_count: 1,
            invocation_cwd: StoredPath::from(PathBuf::from("/home/me")),
            primary_root: StoredPath::from(PathBuf::from("/r")),
            current_worktree_root: StoredPath::from(PathBuf::from("/r")),
            destination: destination(),
            source_facts: facts,
            branch_checked_out: false,
            branch_collision: false,
            known_rules: BTreeSet::new(),
            enabled_rules: BTreeSet::new(),
            known_tasks: BTreeSet::new(),
            manifests: Vec::new(),
            tasks: Vec::new(),
        }
    }

    #[test]
    fn create_accepts_new_existing_and_remote_sources() {
        let branch = BranchName::new("feature").unwrap();
        let new = input(
            CreateSource::NewBranch {
                branch: branch.clone(),
                base: None,
            },
            CreateSourceFacts::NewBranch {
                branch: branch.clone(),
                base_ref: RefName::new("origin/main").unwrap(),
                base_oid: oid(),
                branch_absent: true,
            },
        );
        assert!(plan_create(new).is_ok());
        let local = input(
            CreateSource::ExistingLocal {
                branch: branch.clone(),
            },
            CreateSourceFacts::ExistingLocal {
                branch: branch.clone(),
                branch_oid: oid(),
                not_checked_out: true,
            },
        );
        assert!(plan_create(local).is_ok());
        let remote = input(
            CreateSource::RemoteTracking {
                remote: RemoteName::new("origin").unwrap(),
                remote_branch: branch.clone(),
                local_branch: branch.clone(),
            },
            CreateSourceFacts::RemoteTracking {
                remote: RemoteName::new("origin").unwrap(),
                remote_branch: branch.clone(),
                remote_oid: oid(),
                local_branch: branch,
                local_absent: true,
            },
        );
        assert!(plan_create(remote).is_ok());
    }

    #[test]
    fn create_rejects_source_mismatch_and_unsafe_facts() {
        let branch = BranchName::new("feature").unwrap();
        let mut value = input(
            CreateSource::ExistingLocal {
                branch: branch.clone(),
            },
            CreateSourceFacts::NewBranch {
                branch,
                base_ref: RefName::new("main").unwrap(),
                base_oid: oid(),
                branch_absent: true,
            },
        );
        assert!(plan_create(value.clone()).is_err());
        value.source_facts = CreateSourceFacts::ExistingLocal {
            branch: BranchName::new("feature").unwrap(),
            branch_oid: oid(),
            not_checked_out: true,
        };
        value.destination.state = DestinationState::Dangling;
        assert!(plan_create(value).is_err());
    }

    #[test]
    fn create_steps_are_stable_per_artifact_and_task() {
        let branch = BranchName::new("feature/topic").unwrap();
        let mut value = input(
            CreateSource::NewBranch {
                branch: branch.clone(),
                base: None,
            },
            CreateSourceFacts::NewBranch {
                branch,
                base_ref: RefName::new("main").unwrap(),
                base_oid: oid(),
                branch_absent: true,
            },
        );
        value.known_rules.insert("z-rule".into());
        value.known_rules.insert("a-rule".into());
        value.enabled_rules.insert("z-rule".into());
        value.enabled_rules.insert("a-rule".into());
        value.known_tasks.insert("build".into());
        value.intent.selected_tasks.insert("build".into());
        let artifact = |name: &str| FileArtifact {
            kind: FileArtifactKind::CopyFile,
            source: StoredPath::from(PathBuf::from(name)),
            destination: StoredPath::from(PathBuf::from(name)),
            bytes: 1,
            digest: oid(),
            fingerprint: oid(),
            link_target: None,
            sensitive: false,
            confirm: false,
            conflict: false,
            overlap: false,
            replace_symlink: false,
            compensation: Some(Compensation::RemoveCreatedArtifact(CreatedArtifact {
                path: StoredPath::from(PathBuf::from(name)),
                fingerprint: oid(),
            })),
        };
        value.manifests = vec![
            FileActionManifest {
                rule: "z-rule".into(),
                artifacts: vec![artifact("z")],
                digest: oid(),
            },
            FileActionManifest {
                rule: "a-rule".into(),
                artifacts: vec![artifact("a")],
                digest: oid(),
            },
        ];
        value.tasks = vec![TaskSpec {
            name: "build".into(),
            argv: CommandArgv::new(vec!["build".into()]).unwrap(),
            cwd: StoredPath::from(PathBuf::from("/r")),
            enabled: true,
            post_create: true,
            required: false,
            environment_allowlist: Vec::new(),
        }];
        let plan = plan_create(value).unwrap();
        let names: Vec<_> = plan.steps().iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            vec![
                "create.worktree",
                "file.a-rule.0001",
                "file.z-rule.0001",
                "task.build"
            ]
        );
    }

    #[test]
    fn create_consents_cover_sensitive_confirm_replace_and_task() {
        let branch = BranchName::new("feature").unwrap();
        let mut value = input(
            CreateSource::NewBranch {
                branch: branch.clone(),
                base: None,
            },
            CreateSourceFacts::NewBranch {
                branch,
                base_ref: RefName::new("main").unwrap(),
                base_oid: oid(),
                branch_absent: true,
            },
        );
        value.known_rules.insert("secret".into());
        value.enabled_rules.insert("secret".into());
        value.known_tasks.insert("test".into());
        value.intent.selected_tasks.insert("test".into());
        value.manifests = vec![FileActionManifest {
            rule: "secret".into(),
            artifacts: vec![FileArtifact {
                kind: FileArtifactKind::RelinkSymlink,
                source: StoredPath::from(PathBuf::from("a")),
                destination: StoredPath::from(PathBuf::from("a")),
                bytes: 1,
                digest: oid(),
                fingerprint: oid(),
                link_target: Some(StoredPath::from(PathBuf::from("original"))),
                sensitive: false,
                confirm: true,
                conflict: false,
                overlap: false,
                replace_symlink: true,
                compensation: Some(Compensation::RestoreReplacedSymlink(ReplacedSymlink {
                    path: StoredPath::from(PathBuf::from("a")),
                    expected_current: oid(),
                    original_target: StoredPath::from(PathBuf::from("original")),
                })),
            }],
            digest: oid(),
        }];
        value.tasks = vec![TaskSpec {
            name: "test".into(),
            argv: CommandArgv::new(vec!["test".into()]).unwrap(),
            cwd: StoredPath::from(PathBuf::from("/r")),
            enabled: true,
            post_create: true,
            required: false,
            environment_allowlist: Vec::new(),
        }];
        let mut mismatch = value.clone();
        if let Some(artifact) = mismatch.manifests[0].artifacts.first_mut()
            && let Some(Compensation::RestoreReplacedSymlink(compensation)) =
                artifact.compensation.as_mut()
        {
            compensation.expected_current =
                ObjectId::new("ffffffffffffffffffffffffffffffffffffffff").unwrap();
        }
        assert!(plan_create(mismatch).is_err());
        let plan = plan_create(value).unwrap();
        let ids: BTreeSet<_> = plan
            .required_consents()
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert!(ids.contains("file-rule:secret"));
        assert!(ids.contains("task:test"));
        assert_eq!(plan.granted_consents().len(), 0);
    }

    #[test]
    fn destination_helper_is_pure_and_converts_slashes() {
        let config = CreateConfig {
            default_base: None,
            slug_max_bytes: 60,
            worktree_root: Some("../trees".into()),
            directory_prefix: None,
        };
        assert_eq!(
            destination_for(
                Some(&config),
                Path::new("/repo/main"),
                "feature/topic",
                Path::new("/cwd")
            ),
            PathBuf::from("/trees/main-feature-topic")
        );
        assert_eq!(
            destination_for(None, Path::new("/repo/main"), "topic", Path::new("/cwd")),
            PathBuf::from("/repo/main-topic")
        );
    }

    fn remove_input(intent: RemoveIntent) -> RemovePlanInput {
        RemovePlanInput {
            operation_id: new_operation_id(),
            intent,
            facts: RemoveFacts {
                repository: repo(),
                class: WorktreeClass::Linked,
                locked: false,
                prunable: false,
                ongoing: false,
                oid_matches: true,
                branch_elsewhere: false,
                dirty: false,
                local_branch_safe_to_delete: true,
                branch: BranchName::new("feature").unwrap(),
                branch_oid: oid(),
                worktree_oid: oid(),
                remote_branch: Some(RemoteBranch {
                    remote: RemoteName::new("origin").unwrap(),
                    branch: BranchName::new("feature").unwrap(),
                }),
                remote_branch_oid: Some(oid()),
                remote_is_default: false,
                path: StoredPath::from(PathBuf::from("/w")),
            },
        }
    }
    fn remove_intent() -> RemoveIntent {
        RemoveIntent::new(
            repo(),
            StoredPath::from(PathBuf::from("/w")),
            false,
            false,
            false,
            None,
            BTreeSet::new(),
        )
        .unwrap()
    }

    #[test]
    fn remove_retains_branch_and_rejects_unsafe_classifications() {
        let plan = plan_remove(remove_input(remove_intent())).unwrap();
        assert_eq!(plan.steps().len(), 1);
        for class in [
            WorktreeClass::Primary,
            WorktreeClass::Bare,
            WorktreeClass::Unknown,
        ] {
            let mut value = remove_input(remove_intent());
            value.facts.class = class;
            assert!(plan_remove(value).is_err());
        }
    }

    #[test]
    fn remove_dirty_and_force_are_specific_and_remote_is_last() {
        let mut dirty = remove_input(remove_intent());
        dirty.facts.dirty = true;
        assert!(plan_remove(dirty).is_err());
        let local = RemoveIntent::new(
            repo(),
            StoredPath::from(PathBuf::from("/w")),
            true,
            true,
            true,
            Some(RemoteBranch {
                remote: RemoteName::new("origin").unwrap(),
                branch: BranchName::new("feature").unwrap(),
            }),
            BTreeSet::new(),
        )
        .unwrap();
        let plan = plan_remove(remove_input(local)).unwrap();
        assert_eq!(plan.steps().last().unwrap().name(), "remove.remote-branch");
        assert!(plan.steps().last().unwrap().irreversible());
    }

    #[test]
    fn create_rejects_base_ref_mismatch_and_unknown_grant() {
        let branch = BranchName::new("feature").unwrap();
        let mut value = input(
            CreateSource::NewBranch {
                branch: branch.clone(),
                base: Some(RefName::new("main").unwrap()),
            },
            CreateSourceFacts::NewBranch {
                branch,
                base_ref: RefName::new("origin/main").unwrap(),
                base_oid: oid(),
                branch_absent: true,
            },
        );
        assert!(plan_create(value.clone()).is_err());
        value.intent.source = CreateSource::NewBranch {
            branch: BranchName::new("feature").unwrap(),
            base: None,
        };
        value
            .intent
            .granted_consents
            .insert(ConsentId::new("file-rule:unknown").unwrap());
        assert!(plan_create(value).is_err());
    }

    #[test]
    fn selected_tasks_require_one_enabled_post_create_spec_and_valid_argv() {
        let branch = BranchName::new("feature").unwrap();
        let mut value = input(
            CreateSource::NewBranch {
                branch: branch.clone(),
                base: None,
            },
            CreateSourceFacts::NewBranch {
                branch,
                base_ref: RefName::new("main").unwrap(),
                base_oid: oid(),
                branch_absent: true,
            },
        );
        value.known_tasks.insert("build".into());
        value.intent.selected_tasks.insert("build".into());
        assert!(plan_create(value.clone()).is_err());
        value.tasks.push(TaskSpec {
            name: "build".into(),
            argv: CommandArgv::new(vec!["build".into()]).unwrap(),
            cwd: StoredPath::from(PathBuf::from("/r")),
            enabled: false,
            post_create: true,
            required: false,
            environment_allowlist: Vec::new(),
        });
        assert!(plan_create(value.clone()).is_err());
        value.tasks[0].enabled = true;
        value.tasks[0].post_create = false;
        assert!(plan_create(value).is_err());
        assert!(CommandArgv::new(Vec::new()).is_err());
    }

    #[test]
    fn enabled_rules_need_one_manifest_and_artifact_compensation() {
        let branch = BranchName::new("feature").unwrap();
        let mut value = input(
            CreateSource::NewBranch {
                branch: branch.clone(),
                base: None,
            },
            CreateSourceFacts::NewBranch {
                branch,
                base_ref: RefName::new("main").unwrap(),
                base_oid: oid(),
                branch_absent: true,
            },
        );
        value.known_rules.insert("env".into());
        value.enabled_rules.insert("env".into());
        assert!(plan_create(value.clone()).is_err());
        let artifact = FileArtifact {
            kind: FileArtifactKind::CopyFile,
            source: StoredPath::from(PathBuf::from("a")),
            destination: StoredPath::from(PathBuf::from("a")),
            bytes: 1,
            digest: oid(),
            fingerprint: oid(),
            link_target: None,
            sensitive: false,
            confirm: false,
            conflict: false,
            overlap: false,
            replace_symlink: false,
            compensation: None,
        };
        value.manifests = vec![FileActionManifest {
            rule: "env".into(),
            artifacts: vec![artifact],
            digest: oid(),
        }];
        assert!(plan_create(value).is_err());
    }

    #[test]
    fn remove_plan_has_exact_guards_and_irreversible_worktree_step() {
        let plan = plan_remove(remove_input(remove_intent())).unwrap();
        assert!(matches!(
            plan.preconditions()[0],
            Precondition::CommonDirectory(_)
        ));
        assert!(matches!(
            plan.preconditions()[1],
            Precondition::WorktreeRegistered { .. }
        ));
        assert!(matches!(
            plan.preconditions()[2],
            Precondition::WorktreeClass { .. }
        ));
        assert!(
            plan.preconditions()
                .iter()
                .any(|p| matches!(p, Precondition::NoOngoingGitOperation { .. }))
        );
        assert!(plan.steps()[0].irreversible());
        assert!(plan.steps()[0].compensation().is_none());
        assert!(
            plan.risks()
                .iter()
                .any(|risk| risk.kind == RiskKind::IrreversibleStep)
        );
    }

    #[test]
    fn remove_safe_local_guard_can_only_be_bypassed_by_force() {
        let mut value = remove_input(
            RemoveIntent::new(
                repo(),
                StoredPath::from(PathBuf::from("/w")),
                false,
                true,
                false,
                None,
                BTreeSet::new(),
            )
            .unwrap(),
        );
        value.facts.local_branch_safe_to_delete = false;
        assert!(plan_remove(value.clone()).is_err());
        value.intent.force_delete_local_branch = true;
        assert!(plan_remove(value).is_ok());
    }

    #[test]
    fn remove_requires_matching_non_default_remote_ref() {
        let target = RemoteBranch {
            remote: RemoteName::new("origin").unwrap(),
            branch: BranchName::new("other").unwrap(),
        };
        let intent = RemoveIntent::new(
            repo(),
            StoredPath::from(PathBuf::from("/w")),
            false,
            false,
            false,
            Some(target),
            BTreeSet::new(),
        )
        .unwrap();
        assert!(plan_remove(remove_input(intent)).is_err());
        let target = RemoteBranch {
            remote: RemoteName::new("origin").unwrap(),
            branch: BranchName::new("feature").unwrap(),
        };
        let intent = RemoveIntent::new(
            repo(),
            StoredPath::from(PathBuf::from("/w")),
            false,
            false,
            false,
            Some(target),
            BTreeSet::new(),
        )
        .unwrap();
        let mut value = remove_input(intent);
        value.facts.remote_is_default = true;
        assert!(plan_remove(value).is_err());
    }

    #[test]
    fn create_worktree_step_contains_complete_snapshot_guards() {
        let branch = BranchName::new("feature").unwrap();
        let plan = plan_create(input(
            CreateSource::NewBranch {
                branch: branch.clone(),
                base: None,
            },
            CreateSourceFacts::NewBranch {
                branch,
                base_ref: RefName::new("main").unwrap(),
                base_oid: oid(),
                branch_absent: true,
            },
        ))
        .unwrap();
        let guards = plan.steps()[0].preconditions();
        assert!(guards.iter().any(|guard| matches!(
            guard,
            Precondition::BranchNotElsewhere(branch) if branch.as_str() == "feature"
        )));
        assert!(guards.iter().any(|guard| matches!(
            guard,
            Precondition::BranchNotCheckedOut(branch) if branch.as_str() == "feature"
        )));
        assert!(guards.iter().any(|guard| matches!(
            guard,
            Precondition::RefAbsent(reference) if reference.as_str() == "feature"
        )));
        assert!(guards.iter().any(|guard| matches!(
            guard,
            Precondition::RefAt { reference, .. } if reference.as_str() == "main"
        )));
    }

    #[test]
    fn remove_deletion_steps_do_not_reuse_removed_worktree_guards() {
        let intent = RemoveIntent::new(
            repo(),
            StoredPath::from(PathBuf::from("/w")),
            true,
            true,
            true,
            Some(RemoteBranch {
                remote: RemoteName::new("origin").unwrap(),
                branch: BranchName::new("feature").unwrap(),
            }),
            BTreeSet::new(),
        )
        .unwrap();
        let plan = plan_remove(remove_input(intent)).unwrap();
        let worktree_guards = plan.steps()[0].preconditions();
        assert!(
            plan.preconditions()
                .iter()
                .any(|guard| matches!(guard, Precondition::WorktreeUnlocked { .. }))
        );
        assert!(
            plan.preconditions()
                .iter()
                .any(|guard| matches!(guard, Precondition::WorktreeNotPrunable { .. }))
        );
        assert!(
            worktree_guards
                .iter()
                .any(|guard| matches!(guard, Precondition::WorktreeRegistered { .. }))
        );
        assert!(
            worktree_guards
                .iter()
                .any(|guard| matches!(guard, Precondition::WorktreeClass { .. }))
        );
        assert!(
            worktree_guards
                .iter()
                .any(|guard| matches!(guard, Precondition::WorktreeUnlocked { .. }))
        );
        assert!(
            worktree_guards
                .iter()
                .any(|guard| matches!(guard, Precondition::WorktreeNotPrunable { .. }))
        );
        assert!(
            worktree_guards
                .iter()
                .any(|guard| matches!(guard, Precondition::WorktreeClean { .. }))
        );
        assert!(
            worktree_guards
                .iter()
                .any(|guard| matches!(guard, Precondition::NoOngoingGitOperation { .. }))
        );

        for step in &plan.steps()[1..] {
            assert!(step.preconditions().iter().all(|guard| {
                !matches!(
                    guard,
                    Precondition::WorktreeRegistered { .. }
                        | Precondition::WorktreeClass { .. }
                        | Precondition::WorktreeUnlocked { .. }
                        | Precondition::WorktreeNotPrunable { .. }
                        | Precondition::WorktreeClean { .. }
                        | Precondition::NoOngoingGitOperation { .. }
                )
            }));
            assert!(
                step.preconditions()
                    .iter()
                    .any(|guard| matches!(guard, Precondition::CommonDirectory(_)))
            );
            assert!(
                step.preconditions()
                    .iter()
                    .any(|guard| matches!(guard, Precondition::BranchNotElsewhere(_)))
            );
        }
        assert!(
            plan.steps()[1]
                .preconditions()
                .iter()
                .any(|guard| matches!(guard, Precondition::RefAt { .. }))
        );
        assert!(plan.steps()[1].preconditions().iter().any(|guard| matches!(
            guard,
            Precondition::BranchNotCheckedOut(branch) if branch.as_str() == "feature"
        )));
        assert!(
            plan.steps()[2]
                .preconditions()
                .iter()
                .any(|guard| matches!(guard, Precondition::RemoteRefAt { .. }))
        );
        assert!(plan.steps()[2].preconditions().iter().any(|guard| matches!(
            guard,
            Precondition::RemoteBranchNotDefault(target)
                if target.remote.as_str() == "origin"
                    && target.branch.as_str() == "feature"
        )));
    }

    #[test]
    fn remove_preconditions_roundtrip_with_canonical_new_guards() {
        let intent = RemoveIntent::new(
            repo(),
            StoredPath::from(PathBuf::from("/w")),
            true,
            true,
            true,
            Some(RemoteBranch {
                remote: RemoteName::new("origin").unwrap(),
                branch: BranchName::new("feature").unwrap(),
            }),
            BTreeSet::new(),
        )
        .unwrap();
        let plan = plan_remove(remove_input(intent)).unwrap();
        let wire = serde_json::to_value(&plan).unwrap();
        let restored: OperationPlan = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(restored, plan);
        assert!(
            wire["preconditions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|guard| guard.get("WorktreeUnlocked").is_some())
        );
        assert!(
            wire["preconditions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|guard| guard.get("WorktreeNotPrunable").is_some())
        );
        assert!(
            wire["steps"][1]["preconditions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|guard| guard.get("BranchNotCheckedOut").is_some())
        );
        assert!(
            wire["steps"][2]["preconditions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|guard| guard.get("RemoteBranchNotDefault").is_some())
        );
    }

    #[test]
    fn remove_branch_retention_variants_keep_the_same_step_guards() {
        let retained = plan_remove(remove_input(remove_intent())).unwrap();
        assert_eq!(retained.steps().len(), 1);

        let local = RemoveIntent::new(
            repo(),
            StoredPath::from(PathBuf::from("/w")),
            false,
            true,
            false,
            None,
            BTreeSet::new(),
        )
        .unwrap();
        let local_plan = plan_remove(remove_input(local)).unwrap();
        assert_eq!(
            local_plan
                .steps()
                .iter()
                .map(|step| step.name())
                .collect::<Vec<_>>(),
            vec!["remove.worktree", "remove.local-branch"]
        );

        let remote = RemoveIntent::new(
            repo(),
            StoredPath::from(PathBuf::from("/w")),
            false,
            false,
            false,
            Some(RemoteBranch {
                remote: RemoteName::new("origin").unwrap(),
                branch: BranchName::new("feature").unwrap(),
            }),
            BTreeSet::new(),
        )
        .unwrap();
        let remote_plan = plan_remove(remove_input(remote)).unwrap();
        assert_eq!(
            remote_plan
                .steps()
                .iter()
                .map(|step| step.name())
                .collect::<Vec<_>>(),
            vec!["remove.worktree", "remove.remote-branch"]
        );
    }
}
