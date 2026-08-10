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

pub(crate) fn artifact_digest(bytes: &[u8]) -> ObjectId {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(bytes);
    ObjectId::new(format!("{:x}", hash.finalize())).expect("sha256 is always a valid object id")
}

#[derive(Debug, Clone)]
pub(crate) struct ManifestDigestArtifact {
    pub source_root: StoredPath,
    pub source: StoredPath,
    pub destination: StoredPath,
    pub kind: FileArtifactKind,
    pub bytes: u64,
    pub digest: ObjectId,
    pub fingerprint: ObjectId,
    pub link_target: Option<StoredPath>,
    pub sensitive: bool,
    pub confirm: bool,
    pub mode_policy: FileModePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactStagingRoleV3 {
    Copy,
    RelinkReplacement,
    RelinkBackup,
}

impl ArtifactStagingRoleV3 {
    fn domain_bytes(self) -> &'static [u8] {
        match self {
            Self::Copy => b"copy",
            Self::RelinkReplacement => b"relink-replacement",
            Self::RelinkBackup => b"relink-backup",
        }
    }
    fn leaf_name(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::RelinkReplacement => "relink-replacement",
            Self::RelinkBackup => "relink-backup",
        }
    }
}

pub(crate) fn artifact_staging_token_v3(
    operation_id: &OperationId,
    step_id: &StepId,
    role: ArtifactStagingRoleV3,
) -> ObjectId {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"easy-worktree/artifact-staging/v3\0");
    for value in [
        operation_id.to_string().into_bytes(),
        step_id.as_str().as_bytes().to_vec(),
        role.domain_bytes().to_vec(),
    ] {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value);
    }
    ObjectId::new(format!("{:x}", hash.finalize())).expect("sha256 is always a valid object id")
}

pub(crate) fn artifact_staging_v3(
    operation_id: &OperationId,
    step_id: &StepId,
    role: ArtifactStagingRoleV3,
    destination: &Path,
) -> Result<OwnedStagingV3, String> {
    let parent = destination
        .parent()
        .ok_or_else(|| "destination has no parent".to_owned())?;
    let token = artifact_staging_token_v3(operation_id, step_id, role);
    let path = parent.join(format!(
        ".ewtm-stage-{}-{}",
        role.leaf_name(),
        token.as_str()
    ));
    if !is_normalized_absolute(&path) {
        return Err("staging path is not a normalized absolute path".into());
    }
    Ok(OwnedStagingV3 {
        path: StoredPath::from(path),
        ownership_token: token,
    })
}

fn is_normalized_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ManifestDescriptorV3 {
    CopyFileV3 {
        source_root: StoredPath,
        source: StoredPath,
        expected_source: RegularFileStateV3,
        destination: StoredPath,
        desired_output: RegularFileStateV3,
        staging: OwnedStagingV3,
        publication: PublicationStrategyV3,
        sensitive: bool,
        confirm: bool,
    },
    CreateSymlinkV3 {
        source_root: StoredPath,
        source: StoredPath,
        expected_source: ArtifactSourceExpectationV3,
        destination: StoredPath,
        desired: SymlinkStateV3,
        sensitive: bool,
        confirm: bool,
    },
    RelinkSymlinkV3 {
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
        sensitive: bool,
        confirm: bool,
    },
}

pub(crate) fn canonical_manifest_digest_v3(
    descriptors: &[ManifestDescriptorV3],
    destination_root: &Path,
) -> ObjectId {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"easy-worktree/manifest/v3\0");
    let put = |hash: &mut Sha256, bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    let path = |p: &StoredPath, root: &StoredPath| {
        p.as_path()
            .strip_prefix(root.as_path())
            .unwrap_or(p.as_path())
            .as_os_str()
            .as_encoded_bytes()
            .to_vec()
    };
    let state = |hash: &mut Sha256, value: &ArtifactStateV3| match value {
        ArtifactStateV3::Regular(s) => {
            put(hash, b"regular");
            put(hash, &s.bytes.to_le_bytes());
            put(hash, s.digest.as_str().as_bytes());
            put(hash, &s.mode.to_le_bytes());
        }
        ArtifactStateV3::Symlink(s) => {
            put(hash, b"symlink");
            put(hash, s.target.as_path().as_os_str().as_encoded_bytes());
            put(hash, s.target_digest.as_str().as_bytes());
        }
    };
    let expectation = |hash: &mut Sha256, value: &ArtifactSourceExpectationV3| match value {
        ArtifactSourceExpectationV3::Regular(value) => {
            put(hash, b"regular");
            put(hash, &value.bytes.to_le_bytes());
            put(hash, value.digest.as_str().as_bytes());
            put(hash, &value.mode.to_le_bytes());
        }
        ArtifactSourceExpectationV3::Directory => put(hash, b"directory"),
        ArtifactSourceExpectationV3::Symlink(value) => {
            put(hash, b"symlink");
            put(hash, value.target.as_path().as_os_str().as_encoded_bytes());
            put(hash, value.target_digest.as_str().as_bytes());
        }
    };
    let staging = |hash: &mut Sha256, value: &OwnedStagingV3| {
        put(hash, value.path.as_path().as_os_str().as_encoded_bytes());
        put(hash, value.ownership_token.as_str().as_bytes());
    };
    for descriptor in descriptors {
        match descriptor {
            ManifestDescriptorV3::CopyFileV3 {
                source_root,
                source,
                expected_source,
                destination,
                desired_output,
                staging: owned,
                publication,
                sensitive,
                confirm,
            } => {
                put(&mut hash, b"copy_file_v3");
                put(&mut hash, &path(source, source_root));
                put(
                    &mut hash,
                    &path(destination, &StoredPath::from(destination_root.to_owned())),
                );
                state(
                    &mut hash,
                    &ArtifactStateV3::Regular(expected_source.clone()),
                );
                state(&mut hash, &ArtifactStateV3::Regular(desired_output.clone()));
                staging(&mut hash, owned);
                put(
                    &mut hash,
                    match publication {
                        PublicationStrategyV3::AtomicNoReplaceV1 => b"atomic_no_replace_v1",
                    },
                );
                put(&mut hash, &[*sensitive as u8, *confirm as u8]);
            }
            ManifestDescriptorV3::CreateSymlinkV3 {
                source_root,
                source,
                expected_source,
                destination,
                desired,
                sensitive,
                confirm,
            } => {
                put(&mut hash, b"create_symlink_v3");
                put(&mut hash, &path(source, source_root));
                expectation(&mut hash, expected_source);
                put(
                    &mut hash,
                    &path(destination, &StoredPath::from(destination_root.to_owned())),
                );
                state(&mut hash, &ArtifactStateV3::Symlink(desired.clone()));
                put(&mut hash, &[*sensitive as u8, *confirm as u8]);
            }
            ManifestDescriptorV3::RelinkSymlinkV3 {
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
            } => {
                put(&mut hash, b"relink_symlink_v3");
                put(&mut hash, &path(source, source_root));
                state(
                    &mut hash,
                    &ArtifactStateV3::Symlink(expected_source.clone()),
                );
                put(&mut hash, checkout_oid.as_str().as_bytes());
                put(
                    &mut hash,
                    checkout_relative_path
                        .as_path()
                        .as_os_str()
                        .as_encoded_bytes(),
                );
                put(
                    &mut hash,
                    &path(destination, &StoredPath::from(destination_root.to_owned())),
                );
                state(&mut hash, &ArtifactStateV3::Symlink(expected_old.clone()));
                state(&mut hash, &ArtifactStateV3::Symlink(desired_new.clone()));
                staging(&mut hash, replacement_staging);
                staging(&mut hash, backup_staging);
                put(&mut hash, &[*sensitive as u8, *confirm as u8]);
            }
        }
    }
    ObjectId::new(format!("{:x}", hash.finalize())).expect("sha256 is always a valid object id")
}

pub(crate) fn exact_output_mode(policy: FileModePolicy, source_mode: u32) -> u32 {
    match policy {
        FileModePolicy::Private => 0o600,
        FileModePolicy::PreserveSafe => source_mode & 0o7777 & !(0o7000 | 0o022),
        _ => 0,
    }
}
pub(crate) fn canonical_manifest_digest(
    artifacts: &[ManifestDigestArtifact],
    destination_root: &std::path::Path,
) -> ObjectId {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for artifact in artifacts {
        let source_relative = artifact
            .source
            .as_path()
            .strip_prefix(artifact.source_root.as_path())
            .unwrap_or(artifact.source.as_path());
        let destination_relative = artifact
            .destination
            .as_path()
            .strip_prefix(destination_root)
            .unwrap_or(artifact.destination.as_path());
        hash.update(source_relative.as_os_str().as_encoded_bytes());
        hash.update([0]);
        hash.update(destination_relative.as_os_str().as_encoded_bytes());
        hash.update([0]);
        hash.update(match artifact.kind {
            FileArtifactKind::CopyFile => b"copy_file".as_slice(),
            FileArtifactKind::CreateSymlink => b"create_symlink".as_slice(),
            FileArtifactKind::RelinkSymlink => b"relink_symlink".as_slice(),
        });
        hash.update([0]);
        hash.update(artifact.bytes.to_le_bytes());
        hash.update(artifact.digest.as_str().as_bytes());
        hash.update(artifact.fingerprint.as_str().as_bytes());
        if let Some(target) = &artifact.link_target {
            hash.update(target.as_path().as_os_str().as_encoded_bytes());
        }
        hash.update([
            0,
            artifact.sensitive as u8,
            artifact.confirm as u8,
            artifact.mode_policy as u8,
        ]);
    }
    ObjectId::new(format!("{:x}", hash.finalize())).expect("sha256 is always a valid object id")
}

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileModePolicy {
    Private,
    PreserveSafe,
    NotApplicable,
    LegacyUnspecified,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileArtifact {
    pub kind: FileArtifactKind,
    pub source: StoredPath,
    pub destination: StoredPath,
    pub bytes: u64,
    pub digest: ObjectId,
    pub source_expectation: ArtifactSourceExpectationV3,
    pub fingerprint: ObjectId,
    pub link_target: Option<StoredPath>,
    pub sensitive: bool,
    pub mode_policy: FileModePolicy,
    pub confirm: bool,
    pub conflict: bool,
    pub overlap: bool,
    pub replace_symlink: bool,
    pub compensation: Option<Compensation>,
    pub(crate) relink_facts: Option<RelinkCheckoutFacts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelinkCheckoutFacts {
    pub checkout_oid: ObjectId,
    pub checkout_relative_path: StoredPath,
    pub expected_old: SymlinkStateV3,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileActionManifest {
    pub rule: String,
    pub source_root: StoredPath,
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
    pub safe_target_ref: RefName,
    pub safe_target: ObjectId,
    pub merge_provenance: MergeTargetProvenance,
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

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = normalize_lexical(left.to_owned());
    let right = normalize_lexical(right.to_owned());
    left == right || left.starts_with(&right) || right.starts_with(&left)
}

fn reject_source_mutable_overlap(
    sources: &[StoredPath],
    mutable: &[StoredPath],
) -> Result<(), String> {
    for (index, left) in mutable.iter().enumerate() {
        if mutable[index + 1..]
            .iter()
            .any(|right| paths_overlap(left.as_path(), right.as_path()))
            || sources
                .iter()
                .any(|right| paths_overlap(left.as_path(), right.as_path()))
        {
            return Err("file rule source and mutable paths overlap".into());
        }
    }
    Ok(())
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

fn descriptor_v3(
    operation_id: &OperationId,
    step_id: &StepId,
    source_root: &StoredPath,
    artifact: &FileArtifact,
    checkout_oid: &ObjectId,
    destination_root: &StoredPath,
) -> Result<ManifestDescriptorV3, String> {
    match artifact.kind {
        FileArtifactKind::CopyFile => {
            let ArtifactSourceExpectationV3::Regular(source_state) = &artifact.source_expectation
            else {
                return Err("copy artifact source expectation must be regular".into());
            };
            let staging = artifact_staging_v3(
                operation_id,
                step_id,
                ArtifactStagingRoleV3::Copy,
                artifact.destination.as_path(),
            )?;
            let expected = source_state.clone();
            let desired = RegularFileStateV3 {
                bytes: artifact.bytes,
                digest: artifact.digest.clone(),
                mode: exact_output_mode(artifact.mode_policy, source_state.mode),
            };
            Ok(ManifestDescriptorV3::CopyFileV3 {
                source_root: source_root.clone(),
                source: artifact.source.clone(),
                expected_source: expected,
                destination: artifact.destination.clone(),
                desired_output: desired,
                staging,
                publication: PublicationStrategyV3::AtomicNoReplaceV1,
                sensitive: artifact.sensitive,
                confirm: artifact.confirm,
            })
        }
        FileArtifactKind::CreateSymlink => {
            let expected_source = artifact.source_expectation.clone();
            if !matches!(
                expected_source,
                ArtifactSourceExpectationV3::Regular(_) | ArtifactSourceExpectationV3::Directory
            ) {
                return Err("symlink artifact source expectation is invalid".into());
            }
            let target = artifact
                .link_target
                .as_ref()
                .ok_or_else(|| "symlink artifact lacks link target".to_owned())?;
            if target != &artifact.source {
                return Err("symlink link target does not equal source".into());
            }
            Ok(ManifestDescriptorV3::CreateSymlinkV3 {
                source_root: source_root.clone(),
                source: artifact.source.clone(),
                expected_source,
                destination: artifact.destination.clone(),
                desired: SymlinkStateV3 {
                    target: target.clone(),
                    target_digest: artifact.digest.clone(),
                },
                sensitive: artifact.sensitive,
                confirm: artifact.confirm,
            })
        }
        FileArtifactKind::RelinkSymlink => {
            let desired = match &artifact.source_expectation {
                ArtifactSourceExpectationV3::Symlink(value) => value.clone(),
                _ => return Err("relink checkout facts are missing".into()),
            };
            let checkout = artifact
                .relink_facts
                .as_ref()
                .ok_or_else(|| "relink checkout facts are missing".to_owned())?;
            let expected_old = checkout.expected_old.clone();
            let checkout_relative_path = StoredPath::from(
                artifact
                    .destination
                    .as_path()
                    .strip_prefix(destination_root.as_path())
                    .map_err(|_| "relink destination is outside checkout")?
                    .to_owned(),
            );
            if expected_old == desired {
                return Err("relink old and desired targets are identical".into());
            }
            if checkout.checkout_oid != *checkout_oid
                || checkout.checkout_relative_path.as_path() != checkout_relative_path.as_path()
            {
                return Err("relink checkout facts do not match plan context".into());
            }
            Ok(ManifestDescriptorV3::RelinkSymlinkV3 {
                source_root: source_root.clone(),
                source: artifact.source.clone(),
                expected_source: desired.clone(),
                checkout_oid: checkout.checkout_oid.clone(),
                checkout_relative_path,
                destination: artifact.destination.clone(),
                expected_old: expected_old.clone(),
                desired_new: desired,
                replacement_staging: artifact_staging_v3(
                    operation_id,
                    step_id,
                    ArtifactStagingRoleV3::RelinkReplacement,
                    artifact.destination.as_path(),
                )?,
                backup_staging: artifact_staging_v3(
                    operation_id,
                    step_id,
                    ArtifactStagingRoleV3::RelinkBackup,
                    artifact.destination.as_path(),
                )?,
                sensitive: artifact.sensitive,
                confirm: artifact.confirm,
            })
        }
    }
}

struct V3PlanningContext<'a> {
    operation_id: &'a OperationId,
    step_id: &'a StepId,
    checkout_oid: &'a ObjectId,
    destination_root: &'a StoredPath,
}

fn v3_action_parts(
    rule: &str,
    source_root: StoredPath,
    artifact: &FileArtifact,
    manifest_digest: ObjectId,
    context: V3PlanningContext<'_>,
) -> Result<
    (
        StepAction,
        ArtifactStateV3,
        Option<OwnedStagingV3>,
        Compensation,
    ),
    String,
> {
    let V3PlanningContext {
        operation_id,
        step_id,
        checkout_oid,
        destination_root,
    } = context;
    match artifact.kind {
        FileArtifactKind::CopyFile => {
            let ArtifactSourceExpectationV3::Regular(source_state) = &artifact.source_expectation
            else {
                return Err("copy artifact source expectation must be regular".into());
            };
            let staging = artifact_staging_v3(
                operation_id,
                step_id,
                ArtifactStagingRoleV3::Copy,
                artifact.destination.as_path(),
            )?;
            let expected = source_state.clone();
            let desired = RegularFileStateV3 {
                bytes: artifact.bytes,
                digest: artifact.digest.clone(),
                mode: exact_output_mode(artifact.mode_policy, source_state.mode),
            };
            let state = ArtifactStateV3::Regular(desired.clone());
            Ok((
                StepAction::CopyFileV3 {
                    rule: rule.into(),
                    source_root,
                    source: artifact.source.clone(),
                    expected_source: expected.clone(),
                    destination: artifact.destination.clone(),
                    desired_output: desired,
                    staging: staging.clone(),
                    publication: PublicationStrategyV3::AtomicNoReplaceV1,
                    manifest_digest,
                    sensitive: artifact.sensitive,
                    confirm: artifact.confirm,
                },
                ArtifactStateV3::Regular(expected),
                Some(staging.clone()),
                Compensation::RemoveCreatedArtifactV3(CreatedArtifactV3 {
                    path: artifact.destination.clone(),
                    expected: state,
                    staging: Some(staging),
                }),
            ))
        }
        FileArtifactKind::CreateSymlink => {
            let expected_source = artifact.source_expectation.clone();
            if !matches!(
                expected_source,
                ArtifactSourceExpectationV3::Regular(_) | ArtifactSourceExpectationV3::Directory
            ) {
                return Err("symlink artifact source expectation is invalid".into());
            }
            let target = artifact
                .link_target
                .as_ref()
                .ok_or_else(|| "symlink artifact lacks link target".to_owned())?;
            if target != &artifact.source {
                return Err("symlink link target does not equal source".into());
            }
            let desired = SymlinkStateV3 {
                target: target.clone(),
                target_digest: artifact.digest.clone(),
            };
            let state = ArtifactStateV3::Symlink(desired.clone());
            Ok((
                StepAction::CreateSymlinkV3 {
                    rule: rule.into(),
                    source_root,
                    source: artifact.source.clone(),
                    expected_source,
                    destination: artifact.destination.clone(),
                    desired,
                    manifest_digest,
                    sensitive: artifact.sensitive,
                    confirm: artifact.confirm,
                },
                state.clone(),
                None,
                Compensation::RemoveCreatedArtifactV3(CreatedArtifactV3 {
                    path: artifact.destination.clone(),
                    expected: state,
                    staging: None,
                }),
            ))
        }
        FileArtifactKind::RelinkSymlink => {
            let descriptor = descriptor_v3(
                operation_id,
                step_id,
                &source_root,
                artifact,
                checkout_oid,
                destination_root,
            )?;
            let ManifestDescriptorV3::RelinkSymlinkV3 {
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
            } = descriptor
            else {
                unreachable!()
            };
            Ok((
                StepAction::RelinkSymlinkV3 {
                    rule: rule.into(),
                    source_root,
                    source,
                    expected_source,
                    checkout_oid,
                    checkout_relative_path,
                    destination,
                    expected_old: expected_old.clone(),
                    desired_new: desired_new.clone(),
                    replacement_staging: replacement_staging.clone(),
                    backup_staging: backup_staging.clone(),
                    manifest_digest,
                    sensitive,
                    confirm,
                },
                ArtifactStateV3::Symlink(expected_old.clone()),
                Some(replacement_staging.clone()),
                Compensation::RestoreReplacedSymlinkV3(ReplacedSymlinkV3 {
                    path: artifact.destination.clone(),
                    expected_current: desired_new,
                    restore: expected_old,
                    replacement_staging,
                    backup_staging,
                }),
            ))
        }
    }
}

pub fn plan_create(input: CreatePlanInput) -> Result<OperationPlan, String> {
    let mut input = input;
    input.intent.task_contracts = input
        .tasks
        .iter()
        .filter(|task| input.intent.selected_tasks.contains(&task.name))
        .map(|task| {
            (
                task.name.clone(),
                TaskContract {
                    argv: task.argv.clone(),
                    cwd: task.cwd.clone(),
                    required: task.required,
                    environment_allowlist: task.environment_allowlist.clone(),
                },
            )
        })
        .collect();
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
    input.intent.current_worktree_root = Some(input.current_worktree_root.clone());
    let destination = input
        .intent
        .destination
        .clone()
        .unwrap_or_else(|| input.destination.path.clone());
    input.intent.destination = Some(destination.clone());
    let manifests = std::mem::take(&mut input.manifests);
    input.intent.artifact_rule_contracts.clear();
    let mut artifact_destinations = Vec::new();
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
            artifacts.push((
                manifest.rule.clone(),
                manifest.source_root.clone(),
                artifact,
            ));
            artifact_destinations.push(artifacts.last().unwrap().2.destination.clone());
        }
    }
    if destination_paths_overlap(&artifact_destinations) {
        return Err("file rule destinations overlap".into());
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
    let mut per_rule_index = BTreeMap::<String, usize>::new();
    let mut rule_descriptors = BTreeMap::<String, Vec<ManifestDescriptorV3>>::new();
    for (rule, source_root, artifact) in &artifacts {
        let index = per_rule_index
            .entry(rule.clone())
            .and_modify(|value| *value += 1)
            .or_insert(1);
        let step_id = StepId::new(format!("file.{rule}.{index:04}"))?;
        rule_descriptors
            .entry(rule.clone())
            .or_default()
            .push(descriptor_v3(
                &input.operation_id,
                &step_id,
                source_root,
                artifact,
                &source_oid,
                &destination,
            )?);
    }
    let sources = artifacts
        .iter()
        .map(|(_, _, artifact)| artifact.source.clone())
        .collect::<Vec<_>>();
    let mut mutable = Vec::new();
    for values in rule_descriptors.values() {
        for descriptor in values {
            match descriptor {
                ManifestDescriptorV3::CopyFileV3 {
                    destination,
                    staging,
                    ..
                } => {
                    mutable.push(destination.clone());
                    mutable.push(staging.path.clone());
                }
                ManifestDescriptorV3::CreateSymlinkV3 { destination, .. } => {
                    mutable.push(destination.clone())
                }
                ManifestDescriptorV3::RelinkSymlinkV3 {
                    destination,
                    replacement_staging,
                    backup_staging,
                    ..
                } => {
                    mutable.push(destination.clone());
                    mutable.push(replacement_staging.path.clone());
                    mutable.push(backup_staging.path.clone());
                }
            }
        }
    }
    reject_source_mutable_overlap(&sources, &mutable)?;
    let rule_digests = rule_descriptors
        .into_iter()
        .map(|(rule, values)| {
            (
                rule,
                canonical_manifest_digest_v3(&values, destination.as_path()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut contract_roots = BTreeMap::new();
    for (rule, source_root, _) in &artifacts {
        contract_roots
            .entry(rule.clone())
            .or_insert_with(|| source_root.clone());
    }
    for (rule, source_root) in contract_roots {
        let provenance = if source_root == input.repository.primary_root {
            ArtifactSourceProvenance::Primary
        } else if source_root == input.current_worktree_root {
            ArtifactSourceProvenance::CurrentWorktree
        } else {
            return Err("manifest source root does not match repository facts".into());
        };
        input.intent.artifact_rule_contracts.insert(
            rule.clone(),
            ArtifactRuleContract {
                provenance,
                source_root,
                manifest_digest: rule_digests[&rule].clone(),
            },
        );
    }
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
            preconditions.push(Precondition::RefAt {
                reference: RefName::new(format!("refs/remotes/{remote}/{remote_branch}"))?,
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
    if let CreateSource::RemoteTracking {
        remote,
        remote_branch,
        local_branch,
    } = &input.intent.source
    {
        create_post.push(Postcondition::BranchUpstreamAt {
            branch: local_branch.clone(),
            remote: remote.clone(),
            remote_branch: remote_branch.clone(),
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
    for (rule, source_root, artifact) in artifacts {
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
        let manifest_digest = rule_digests[&rule].clone();
        let step_id = StepId::new(id.clone())?;
        let (action, _expected, staging, compensation) = v3_action_parts(
            &rule,
            source_root.clone(),
            &artifact,
            manifest_digest.clone(),
            V3PlanningContext {
                operation_id: &input.operation_id,
                step_id: &step_id,
                checkout_oid: &source_oid,
                destination_root: &destination,
            },
        )?;
        let source_expectation = artifact.source_expectation.clone();
        let manifest_precondition = Precondition::ArtifactSourceAtV3 {
            rule: rule.clone(),
            source_root,
            source: artifact.source.clone(),
            expectation: source_expectation,
            manifest_digest,
        };
        let mut artifact_preconditions = vec![manifest_precondition];
        if artifact.kind != FileArtifactKind::RelinkSymlink {
            artifact_preconditions.push(Precondition::PathAbsent(artifact.destination.clone()));
        }
        if let Some(staging) = staging {
            artifact_preconditions.push(Precondition::PathAbsent(staging.path));
        }
        if artifact.kind == FileArtifactKind::RelinkSymlink {
            let StepAction::RelinkSymlinkV3 {
                checkout_oid,
                checkout_relative_path,
                destination,
                expected_old,
                ..
            } = &action
            else {
                return Err("relink action construction failed".into());
            };
            artifact_preconditions.push(Precondition::TreeSymlinkAtV3 {
                commit_oid: checkout_oid.clone(),
                checkout_relative_path: checkout_relative_path.clone(),
                expected: expected_old.clone(),
            });
            artifact_preconditions.push(Precondition::SymlinkAtV3 {
                path: destination.clone(),
                expected: expected_old.clone(),
            });
            artifact_preconditions.push(Precondition::PathAbsent(
                artifact_staging_v3(
                    &input.operation_id,
                    &step_id,
                    ArtifactStagingRoleV3::RelinkBackup,
                    artifact.destination.as_path(),
                )?
                .path,
            ));
        }
        steps.push(step(
            &id,
            &id,
            action,
            artifact_preconditions,
            vec![],
            Some(compensation),
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
            true,
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
    let plan = OperationPlan::new(OperationPlanDraft {
        operation_id: input.operation_id,
        kind: OperationKind::Create,
        repository: input.repository,
        intent: OperationIntent::Create(input.intent),
        preconditions,
        steps,
        risks,
        required_consents: required,
        granted_consents: grants,
    })?;
    plan.validate_executable_plan()
        .map_err(|error| format!("constructed create plan is not executable: {error}"))?;
    Ok(plan)
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
    if input.intent.delete_local_branch && f.branch_oid != f.worktree_oid {
        return Err("local branch OID does not match worktree OID".into());
    }
    if input.intent.worktree != f.path {
        return Err("intent worktree does not match facts".into());
    }
    if input.intent.repository != f.repository {
        return Err("intent repository does not match facts".into());
    }
    let mut pre = vec![
        Precondition::CommonDirectory(input.intent.repository.common_dir.clone()),
        Precondition::WorktreeAt {
            path: f.path.clone(),
            branch: f.branch.clone(),
            oid: f.worktree_oid.clone(),
            class: f.class,
        },
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
    risks.push(Risk {
        kind: RiskKind::RemoveWorktree,
        message: "remove worktree".into(),
    });
    required.push(ConsentRequirement {
        id: ConsentId::new("remove:worktree")?,
        risks: vec![Risk {
            kind: RiskKind::RemoveWorktree,
            message: "remove worktree".into(),
        }],
    });
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
            oid: f.worktree_oid.clone(),
        });
        if !input.intent.force_delete_local_branch {
            local_pre.push(Precondition::RefAt {
                reference: f.safe_target_ref.clone(),
                oid: f.safe_target.clone(),
            });
            local_pre.push(Precondition::RefMergedInto {
                reference: RefName::new(f.branch.as_str())?,
                target_ref: Some(f.safe_target_ref.clone()),
                target_oid: f.safe_target.clone(),
                provenance: f.merge_provenance.clone(),
            });
            if let MergeTargetProvenance::Upstream {
                branch,
                upstream_ref,
            } = &f.merge_provenance
            {
                local_pre.push(Precondition::BranchUpstreamIs {
                    branch: branch.clone(),
                    upstream_ref: upstream_ref.clone(),
                });
            }
        }
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
            oid: remote_oid.clone(),
        });
        steps.push(step(
            "remove.remote-branch",
            "remove.remote-branch",
            StepAction::DeleteRemoteBranch {
                target: remote.clone(),
                expected_oid: Some(remote_oid),
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
    let plan = OperationPlan::new(OperationPlanDraft {
        operation_id: input.operation_id,
        kind: OperationKind::Remove,
        repository: input.intent.repository.clone(),
        intent: OperationIntent::Remove(input.intent.clone()),
        preconditions: pre,
        steps,
        risks,
        required_consents: required,
        granted_consents: input.intent.granted_consents,
    })?;
    plan.validate_executable_plan()
        .map_err(|error| format!("constructed remove plan is not executable: {error}"))?;
    Ok(plan)
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
            task_contracts: BTreeMap::new(),
            current_worktree_root: None,
            artifact_rule_contracts: BTreeMap::new(),
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

    fn executable_artifact_input(
        kind: FileArtifactKind,
        sensitive: bool,
        replace_symlink: bool,
        with_optional_task: bool,
        required_task: bool,
    ) -> CreatePlanInput {
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
        value.intent.destination = Some(value.destination.path.clone());
        let target = StoredPath::from(PathBuf::from("/r/source/config"));
        let target_bytes = target.as_path().as_os_str().as_encoded_bytes();
        let digest = if matches!(
            kind,
            FileArtifactKind::CreateSymlink | FileArtifactKind::RelinkSymlink
        ) {
            artifact_digest(target_bytes)
        } else {
            oid()
        };
        let destination = StoredPath::from(PathBuf::from("/w/feature/config"));
        value.known_rules.insert("config".into());
        value.enabled_rules.insert("config".into());
        value.destination.state = DestinationState::Absent;
        value.manifests = vec![FileActionManifest {
            rule: "config".into(),
            source_root: StoredPath::from(PathBuf::from("/r")),
            artifacts: vec![FileArtifact {
                kind,
                source: if matches!(kind, FileArtifactKind::CreateSymlink) {
                    target.clone()
                } else {
                    StoredPath::from(PathBuf::from("/r/source/config"))
                },
                destination: destination.clone(),
                bytes: if matches!(
                    kind,
                    FileArtifactKind::CreateSymlink | FileArtifactKind::RelinkSymlink
                ) {
                    target_bytes.len() as u64
                } else {
                    7
                },
                digest: digest.clone(),
                fingerprint: digest.clone(),
                source_expectation: if matches!(kind, FileArtifactKind::RelinkSymlink) {
                    ArtifactSourceExpectationV3::Symlink(SymlinkStateV3 {
                        target: target.clone(),
                        target_digest: digest.clone(),
                    })
                } else if matches!(kind, FileArtifactKind::CopyFile) {
                    ArtifactSourceExpectationV3::Regular(RegularFileStateV3 {
                        bytes: 7,
                        digest: digest.clone(),
                        mode: 0o644,
                    })
                } else {
                    ArtifactSourceExpectationV3::Regular(RegularFileStateV3 {
                        bytes: target_bytes.len() as u64,
                        digest: digest.clone(),
                        mode: 0o644,
                    })
                },
                link_target: if matches!(
                    kind,
                    FileArtifactKind::CreateSymlink | FileArtifactKind::RelinkSymlink
                ) {
                    Some(target.clone())
                } else {
                    None
                },
                sensitive,
                mode_policy: if !matches!(kind, FileArtifactKind::CopyFile) {
                    FileModePolicy::NotApplicable
                } else if sensitive {
                    FileModePolicy::Private
                } else {
                    FileModePolicy::PreserveSafe
                },
                confirm: false,
                conflict: false,
                overlap: false,
                replace_symlink,
                compensation: if matches!(kind, FileArtifactKind::RelinkSymlink) {
                    None
                } else {
                    Some(if replace_symlink {
                        Compensation::RestoreReplacedSymlink(ReplacedSymlink {
                            path: destination,
                            expected_current: digest.clone(),
                            original_target: target,
                        })
                    } else {
                        Compensation::RemoveCreatedArtifact(CreatedArtifact {
                            path: destination,
                            fingerprint: digest,
                        })
                    })
                },
                relink_facts: if matches!(kind, FileArtifactKind::RelinkSymlink) {
                    Some(RelinkCheckoutFacts {
                        checkout_oid: oid(),
                        checkout_relative_path: StoredPath::from(PathBuf::from("config")),
                        expected_old: SymlinkStateV3 {
                            target: StoredPath::from(PathBuf::from("old-target")),
                            target_digest: artifact_digest(b"old-target"),
                        },
                    })
                } else {
                    None
                },
            }],
            digest: oid(),
        }];
        if with_optional_task {
            value.known_tasks.insert("build".into());
            value.intent.selected_tasks.insert("build".into());
            value.tasks.push(TaskSpec {
                name: "build".into(),
                argv: CommandArgv::new(
                    vec!["build", "--check"]
                        .into_iter()
                        .map(String::from)
                        .collect(),
                )
                .unwrap(),
                cwd: StoredPath::from(PathBuf::from("/w/feature")),
                enabled: true,
                post_create: true,
                required: required_task,
                environment_allowlist: Vec::new(),
            });
        }
        value
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
            source: StoredPath::from(PathBuf::from(format!("/r/{name}"))),
            destination: StoredPath::from(PathBuf::from(format!("/w/feature/{name}"))),
            bytes: 1,
            digest: oid(),
            fingerprint: oid(),
            source_expectation: ArtifactSourceExpectationV3::Regular(RegularFileStateV3 {
                bytes: 1,
                digest: oid(),
                mode: 0o644,
            }),
            link_target: None,
            sensitive: false,
            mode_policy: FileModePolicy::PreserveSafe,
            confirm: false,
            conflict: false,
            overlap: false,
            replace_symlink: false,
            compensation: Some(Compensation::RemoveCreatedArtifact(CreatedArtifact {
                path: StoredPath::from(PathBuf::from(format!("/w/feature/{name}"))),
                fingerprint: oid(),
            })),
            relink_facts: None,
        };
        value.manifests = vec![
            FileActionManifest {
                rule: "z-rule".into(),
                source_root: StoredPath::from(PathBuf::from("/r")),
                artifacts: vec![artifact("z")],
                digest: oid(),
            },
            FileActionManifest {
                rule: "a-rule".into(),
                source_root: StoredPath::from(PathBuf::from("/r")),
                artifacts: vec![artifact("a")],
                digest: oid(),
            },
        ];
        value.tasks = vec![TaskSpec {
            name: "build".into(),
            argv: CommandArgv::new(vec!["build".into()]).unwrap(),
            cwd: StoredPath::from(PathBuf::from("/w/feature")),
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
            source_root: StoredPath::from(PathBuf::from("/r")),
            artifacts: vec![FileArtifact {
                kind: FileArtifactKind::RelinkSymlink,
                source: StoredPath::from(PathBuf::from("/r/a")),
                destination: StoredPath::from(PathBuf::from("/w/feature/a")),
                bytes: 8,
                digest: artifact_digest(b"original"),
                fingerprint: artifact_digest(b"original"),
                source_expectation: ArtifactSourceExpectationV3::Regular(RegularFileStateV3 {
                    bytes: 8,
                    digest: artifact_digest(b"original"),
                    mode: 0o644,
                }),
                link_target: Some(StoredPath::from(PathBuf::from("original"))),
                sensitive: false,
                mode_policy: FileModePolicy::NotApplicable,
                confirm: true,
                conflict: false,
                overlap: false,
                replace_symlink: true,
                compensation: Some(Compensation::RestoreReplacedSymlink(ReplacedSymlink {
                    path: StoredPath::from(PathBuf::from("/w/feature/a")),
                    expected_current: artifact_digest(b"original"),
                    original_target: StoredPath::from(PathBuf::from("original")),
                })),
                relink_facts: None,
            }],
            digest: oid(),
        }];
        value.tasks = vec![TaskSpec {
            name: "test".into(),
            argv: CommandArgv::new(vec!["test".into()]).unwrap(),
            cwd: StoredPath::from(PathBuf::from("/w/feature")),
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
        let error = plan_create(value).unwrap_err();
        assert_eq!(error, "relink checkout facts are missing");
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
                safe_target_ref: RefName::new("HEAD").unwrap(),
                safe_target: oid(),
                merge_provenance: MergeTargetProvenance::Primary,
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
            source_expectation: ArtifactSourceExpectationV3::Regular(RegularFileStateV3 {
                bytes: 1,
                digest: oid(),
                mode: 0o644,
            }),
            link_target: None,
            sensitive: false,
            mode_policy: FileModePolicy::PreserveSafe,
            confirm: false,
            conflict: false,
            overlap: false,
            replace_symlink: false,
            compensation: None,
            relink_facts: None,
        };
        value.manifests = vec![FileActionManifest {
            rule: "env".into(),
            source_root: StoredPath::from(PathBuf::from("/r")),
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
            Precondition::WorktreeAt { .. }
        ));
        assert!(matches!(
            plan.preconditions()[2],
            Precondition::WorktreeRegistered { .. }
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
    fn staging_v3_is_deterministic_and_domain_separated() {
        let operation = OperationId::new(Uuid::nil());
        let step = StepId::new("copy").unwrap();
        let destination = Path::new("/work/file");
        let first =
            artifact_staging_v3(&operation, &step, ArtifactStagingRoleV3::Copy, destination)
                .unwrap();
        let same = artifact_staging_v3(&operation, &step, ArtifactStagingRoleV3::Copy, destination)
            .unwrap();
        let replacement = artifact_staging_v3(
            &operation,
            &step,
            ArtifactStagingRoleV3::RelinkReplacement,
            destination,
        )
        .unwrap();
        assert_eq!(first, same);
        assert_ne!(first, replacement);
        assert_eq!(first.path.as_path().parent().unwrap(), Path::new("/work"));
        assert!(
            first
                .path
                .as_path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".ewtm-stage-copy-")
        );
        assert!(
            artifact_staging_v3(
                &operation,
                &step,
                ArtifactStagingRoleV3::Copy,
                Path::new("relative/file")
            )
            .is_err()
        );
    }

    #[test]
    fn output_mode_policy_is_exact_and_safe() {
        assert_eq!(exact_output_mode(FileModePolicy::Private, 0o7777), 0o600);
        assert_eq!(
            exact_output_mode(FileModePolicy::PreserveSafe, 0o7777),
            0o755
        );
        assert_eq!(
            exact_output_mode(FileModePolicy::PreserveSafe, 0o676),
            0o654
        );
        assert_eq!(
            exact_output_mode(FileModePolicy::PreserveSafe, 0o541),
            0o541
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
    fn generated_create_sources_and_remove_variants_are_executable() {
        let branch = BranchName::new("feature").unwrap();
        let sources = vec![
            (
                CreateSource::NewBranch {
                    branch: branch.clone(),
                    base: None,
                },
                CreateSourceFacts::NewBranch {
                    branch: branch.clone(),
                    base_ref: RefName::new("main").unwrap(),
                    base_oid: oid(),
                    branch_absent: true,
                },
            ),
            (
                CreateSource::ExistingLocal {
                    branch: branch.clone(),
                },
                CreateSourceFacts::ExistingLocal {
                    branch: branch.clone(),
                    branch_oid: oid(),
                    not_checked_out: true,
                },
            ),
            (
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
            ),
        ];
        for (source, facts) in sources {
            let mut value = input(source, facts);
            value.intent.destination = Some(value.destination.path.clone());
            assert!(
                plan_create(value)
                    .unwrap()
                    .validate_executable_plan()
                    .is_ok()
            );
        }
        assert!(
            plan_remove(remove_input(remove_intent()))
                .unwrap()
                .validate_executable_plan()
                .is_ok()
        );
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
        assert!(
            plan_remove(remove_input(local))
                .unwrap()
                .validate_executable_plan()
                .is_ok()
        );
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

    #[test]
    fn positive_schema2_planner_matrix_roundtrips_and_validates() {
        let branch = BranchName::new("feature").unwrap();
        let sources = [
            (
                "new",
                CreateSource::NewBranch {
                    branch: branch.clone(),
                    base: None,
                },
                CreateSourceFacts::NewBranch {
                    branch: branch.clone(),
                    base_ref: RefName::new("main").unwrap(),
                    base_oid: oid(),
                    branch_absent: true,
                },
            ),
            (
                "existing",
                CreateSource::ExistingLocal {
                    branch: branch.clone(),
                },
                CreateSourceFacts::ExistingLocal {
                    branch: branch.clone(),
                    branch_oid: oid(),
                    not_checked_out: true,
                },
            ),
            (
                "remote",
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
            ),
        ];
        for (name, source, facts) in sources {
            let mut input = input(source, facts);
            input.intent.destination = Some(input.destination.path.clone());
            let plan = plan_create(input).unwrap_or_else(|e| panic!("{name}: {e}"));
            let restored: OperationPlan =
                serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
            assert_eq!(restored, plan, "{name}: schema2 roundtrip");
            restored
                .validate_executable_plan()
                .unwrap_or_else(|error| panic!("{name}: {error}"));
        }
        for (name, intent) in [
            ("remove clean", remove_intent()),
            (
                "remove dirty",
                RemoveIntent::new(
                    repo(),
                    StoredPath::from(PathBuf::from("/w")),
                    true,
                    false,
                    false,
                    None,
                    BTreeSet::new(),
                )
                .unwrap(),
            ),
            (
                "remove local",
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
            ),
            (
                "remove local force",
                RemoveIntent::new(
                    repo(),
                    StoredPath::from(PathBuf::from("/w")),
                    false,
                    true,
                    true,
                    None,
                    BTreeSet::new(),
                )
                .unwrap(),
            ),
            (
                "remove remote",
                RemoveIntent::new(
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
                .unwrap(),
            ),
            (
                "remove local+remote",
                RemoveIntent::new(
                    repo(),
                    StoredPath::from(PathBuf::from("/w")),
                    false,
                    true,
                    false,
                    Some(RemoteBranch {
                        remote: RemoteName::new("origin").unwrap(),
                        branch: BranchName::new("feature").unwrap(),
                    }),
                    BTreeSet::new(),
                )
                .unwrap(),
            ),
        ] {
            let plan = plan_remove(remove_input(intent)).unwrap_or_else(|e| panic!("{name}: {e}"));
            let restored: OperationPlan =
                serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
            assert_eq!(restored, plan, "{name}: schema2 roundtrip");
            restored
                .validate_executable_plan()
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            if name.contains("remote") {
                assert!(matches!(
                    restored.steps().last().unwrap().action(),
                    StepAction::DeleteRemoteBranch {
                        expected_oid: Some(_),
                        ..
                    }
                ));
            }
        }
    }

    #[test]
    fn positive_create_contract_matrix_is_canonical_and_executable() {
        let cases = [
            (
                "normal copy",
                FileArtifactKind::CopyFile,
                false,
                false,
                false,
                false,
            ),
            (
                "sensitive private copy",
                FileArtifactKind::CopyFile,
                true,
                false,
                false,
                false,
            ),
            (
                "create symlink",
                FileArtifactKind::CreateSymlink,
                false,
                false,
                false,
                false,
            ),
            (
                "relink symlink",
                FileArtifactKind::RelinkSymlink,
                false,
                true,
                false,
                false,
            ),
            (
                "optional task",
                FileArtifactKind::CopyFile,
                false,
                false,
                true,
                false,
            ),
            (
                "required task",
                FileArtifactKind::CopyFile,
                false,
                false,
                true,
                true,
            ),
            (
                "artifact and task suffix",
                FileArtifactKind::CreateSymlink,
                false,
                false,
                true,
                false,
            ),
        ];
        for (name, kind, sensitive, replace, task, required) in cases {
            if kind == FileArtifactKind::RelinkSymlink {
                let plan = plan_create(executable_artifact_input(
                    kind, sensitive, replace, task, required,
                ))
                .unwrap_or_else(|error| panic!("{name}: {error}"));
                plan.validate_executable_plan()
                    .unwrap_or_else(|error| panic!("{name}: {error}"));
                let step = &plan.steps()[1];
                assert_eq!(
                    step.preconditions()
                        .iter()
                        .filter(|guard| matches!(guard, Precondition::PathAbsent(_)))
                        .count(),
                    2
                );
                let mut noop = executable_artifact_input(kind, sensitive, replace, task, required);
                let artifact = &mut noop.manifests[0].artifacts[0];
                artifact.relink_facts.as_mut().unwrap().expected_old =
                    match &artifact.source_expectation {
                        ArtifactSourceExpectationV3::Symlink(value) => value.clone(),
                        _ => unreachable!(),
                    };
                assert_eq!(
                    plan_create(noop).unwrap_err(),
                    "relink old and desired targets are identical"
                );
                continue;
            }
            let plan = plan_create(executable_artifact_input(
                kind, sensitive, replace, task, required,
            ))
            .unwrap_or_else(|error| panic!("{name}: {error}"));
            let restored: OperationPlan =
                serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
            assert_eq!(restored, plan, "{name}: schema2 roundtrip");
            restored
                .validate_executable_plan()
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let artifact = restored
                .steps()
                .iter()
                .find_map(|step| match step.action() {
                    StepAction::CopyFileV3 { desired_output, .. } => {
                        Some((FileArtifactKind::CopyFile, desired_output.mode))
                    }
                    StepAction::CreateSymlinkV3 { .. } => {
                        Some((FileArtifactKind::CreateSymlink, 0))
                    }
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                artifact.1,
                if sensitive {
                    0o600
                } else if matches!(kind, FileArtifactKind::CopyFile) {
                    0o644
                } else {
                    0
                },
                "{name}: mode policy"
            );
            let ids: Vec<_> = restored
                .required_consents()
                .iter()
                .map(|consent| consent.id.as_str())
                .collect();
            let mut expected = Vec::new();
            if sensitive || replace {
                expected.push("file-rule:config");
            }
            if task {
                expected.push("task:build");
            }
            assert_eq!(ids, expected, "{name}: consent order");
        }
    }

    #[test]
    fn linked_worktree_source_root_is_authoritative() {
        let mut value =
            executable_artifact_input(FileArtifactKind::CopyFile, false, false, false, false);
        value.repository.primary_root = StoredPath::from(PathBuf::from("/primary"));
        value.intent.repository = value.repository.clone();
        value.primary_root = StoredPath::from(PathBuf::from("/primary"));
        value.current_worktree_root = StoredPath::from(PathBuf::from("/linked"));
        value.manifests[0].source_root = StoredPath::from(PathBuf::from("/linked"));
        value.manifests[0].artifacts[0].source =
            StoredPath::from(PathBuf::from("/linked/source/config"));
        let plan = plan_create(value).unwrap();
        let restored: OperationPlan =
            serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
        assert_eq!(restored, plan);
        assert!(restored.validate_executable_plan().is_ok());
    }

    #[test]
    fn same_rule_mixed_source_roots_are_not_executable() {
        let mut value =
            executable_artifact_input(FileArtifactKind::CopyFile, false, false, false, false);
        let second = value.manifests[0].artifacts[0].clone();
        value.manifests[0].artifacts.push(FileArtifact {
            source: StoredPath::from(PathBuf::from("/r/source/other")),
            destination: StoredPath::from(PathBuf::from("/w/feature/other")),
            compensation: Some(Compensation::RemoveCreatedArtifact(CreatedArtifact {
                path: StoredPath::from(PathBuf::from("/w/feature/other")),
                fingerprint: second.fingerprint.clone(),
            })),
            ..second
        });
        let mut plan = plan_create(value).unwrap();
        let mut artifact_steps: Vec<_> = plan
            .steps_mut()
            .iter_mut()
            .filter(|step| matches!(step.action(), StepAction::CopyFileV3 { .. }))
            .collect();
        assert_eq!(artifact_steps.len(), 2);
        if let StepAction::CopyFileV3 { source, .. } = artifact_steps[1].action_mut() {
            *source = StoredPath::from(PathBuf::from("/linked/source/other"));
        }
        for guard in artifact_steps[1].preconditions_mut() {
            if let Precondition::ArtifactSourceAtV3 {
                source_root,
                source,
                ..
            } = guard
            {
                *source_root = StoredPath::from(PathBuf::from("/linked"));
                *source = StoredPath::from(PathBuf::from("/linked/source/other"));
            }
        }
        let restored: OperationPlan =
            serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
        assert!(restored.validate_persisted().is_ok());
        assert!(restored.validate_executable_plan().is_err());
    }

    #[test]
    fn artifact_source_outside_persisted_source_root_is_not_executable() {
        let mut plan = plan_create({
            let mut value =
                executable_artifact_input(FileArtifactKind::CopyFile, false, false, false, false);
            value.repository.primary_root = StoredPath::from(PathBuf::from("/primary"));
            value.intent.repository = value.repository.clone();
            value.primary_root = StoredPath::from(PathBuf::from("/primary"));
            value.current_worktree_root = StoredPath::from(PathBuf::from("/linked"));
            value.manifests[0].source_root = StoredPath::from(PathBuf::from("/linked"));
            value.manifests[0].artifacts[0].source =
                StoredPath::from(PathBuf::from("/linked/source/config"));
            value
        })
        .unwrap();
        if let StepAction::CopyFileV3 { source, .. } = plan.steps_mut()[1].action_mut() {
            *source = StoredPath::from(PathBuf::from("/outside/source/config"));
        }
        for guard in plan.steps_mut()[1].preconditions_mut() {
            if let Precondition::ArtifactSourceAtV3 { source, .. } = guard {
                *source = StoredPath::from(PathBuf::from("/outside/source/config"));
            }
        }
        assert!(plan.validate_persisted().is_ok());
        assert!(plan.validate_executable_plan().is_err());
    }

    #[test]
    fn coordinated_arbitrary_manifest_digest_is_not_executable() {
        let mut plan = plan_create(executable_artifact_input(
            FileArtifactKind::CopyFile,
            false,
            false,
            false,
            false,
        ))
        .unwrap();
        let arbitrary = ObjectId::new("fedcba9876543210fedcba9876543210fedcba98").unwrap();
        for step in plan.steps_mut() {
            if let StepAction::CopyFileV3 {
                manifest_digest, ..
            } = step.action_mut()
            {
                *manifest_digest = arbitrary.clone();
            }
            for guard in step.preconditions_mut() {
                if let Precondition::ArtifactSourceAtV3 {
                    manifest_digest, ..
                } = guard
                {
                    *manifest_digest = arbitrary.clone();
                }
            }
        }
        assert!(plan.validate_persisted().is_ok());
        assert!(plan.validate_executable_plan().is_err());
    }

    #[test]
    fn archived_schema1_remote_delete_without_lease_is_readable_only() {
        let intent = RemoveIntent::new(
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
        let mut wire = serde_json::to_value(plan_remove(remove_input(intent)).unwrap()).unwrap();
        wire["plan_schema_version"] = json!(1);
        wire["steps"][1]["action"]["DeleteRemoteBranch"]
            .as_object_mut()
            .unwrap()
            .remove("expected_oid");
        let restored: OperationPlan = serde_json::from_value(wire).unwrap();
        assert!(restored.validate_persisted().is_ok());
        assert!(restored.validate_executable_plan().is_err());
    }

    #[test]
    fn crafted_create_contract_mutations_are_shape_valid_but_not_executable() {
        fn visit(
            value: &mut serde_json::Value,
            key: &str,
            f: &mut dyn FnMut(&mut serde_json::Value),
        ) {
            if let Some(object) = value.as_object_mut() {
                if let Some(found) = object.get_mut(key) {
                    f(found);
                }
                for child in object.values_mut() {
                    visit(child, key, f);
                }
            } else if let Some(array) = value.as_array_mut() {
                for child in array {
                    visit(child, key, f);
                }
            }
        }
        fn remove_variant(value: &mut serde_json::Value, key: &str) {
            if let Some(array) = value.as_array_mut() {
                array.retain(|item| item.get(key).is_none());
                for child in array {
                    remove_variant(child, key);
                }
            } else if let Some(object) = value.as_object_mut() {
                for child in object.values_mut() {
                    remove_variant(child, key);
                }
            }
        }
        let copy = serde_json::to_value(
            plan_create(executable_artifact_input(
                FileArtifactKind::CopyFile,
                true,
                false,
                true,
                false,
            ))
            .unwrap(),
        )
        .unwrap();
        let symlink = serde_json::to_value(
            plan_create(executable_artifact_input(
                FileArtifactKind::CreateSymlink,
                false,
                false,
                false,
                false,
            ))
            .unwrap(),
        )
        .unwrap();
        type Mutation = (
            &'static str,
            serde_json::Value,
            Box<dyn Fn(&mut serde_json::Value)>,
        );
        let cases: Vec<Mutation> = vec![
            (
                "sensitive Private to PreserveSafe",
                copy.clone(),
                Box::new(|v| visit(v, "mode_policy", &mut |x| *x = json!("preserve_safe"))),
            ),
            (
                "symlink target bytes",
                symlink.clone(),
                Box::new(|v| visit(v, "bytes", &mut |x| *x = json!(99))),
            ),
            (
                "symlink digest without target",
                symlink.clone(),
                Box::new(|v| {
                    visit(v, "digest", &mut |x| {
                        *x = json!("fedcba9876543210fedcba9876543210fedcba98")
                    })
                }),
            ),
            (
                "symlink PathAbsent versus SymlinkAt",
                symlink.clone(),
                Box::new(|v| {
                    remove_variant(v, "PathAbsent");
                }),
            ),
            (
                "artifact compensation changed",
                copy.clone(),
                Box::new(|v| visit(v, "compensation", &mut |x| *x = serde_json::Value::Null)),
            ),
            (
                "file-rule consent missing",
                copy.clone(),
                Box::new(|v| v["required_consents"].as_array_mut().unwrap().clear()),
            ),
            (
                "file-rule consent extra",
                copy.clone(),
                Box::new(|v| {
                    let mut c = v["required_consents"][0].clone();
                    c["id"] = json!("file-rule:extra");
                    v["required_consents"].as_array_mut().unwrap().push(c);
                }),
            ),
            (
                "file-rule wrong risk",
                copy.clone(),
                Box::new(|v| v["risks"][0]["kind"] = json!("dirty_data_loss")),
            ),
            (
                "task consent missing",
                copy.clone(),
                Box::new(|v| {
                    v["required_consents"].as_array_mut().unwrap().remove(1);
                }),
            ),
            (
                "task consent extra",
                copy.clone(),
                Box::new(|v| {
                    let mut c = v["required_consents"][1].clone();
                    c["id"] = json!("task:extra");
                    v["required_consents"].as_array_mut().unwrap().push(c);
                }),
            ),
            (
                "task wrong risk",
                copy.clone(),
                Box::new(|v| v["risks"][1]["kind"] = json!("delete_local_branch")),
            ),
            (
                "task before artifact",
                copy.clone(),
                Box::new(|v| {
                    let steps = v["steps"].as_array_mut().unwrap();
                    steps.swap(1, 2);
                }),
            ),
            (
                "task irreversible false",
                copy.clone(),
                Box::new(|v| visit(v, "irreversible", &mut |x| *x = json!(false))),
            ),
            (
                "task compensation injected",
                copy.clone(),
                Box::new(|v| {
                    visit(
                        v,
                        "compensation",
                        &mut |x| *x = json!({"RemoveCreatedArtifact":{"path":"/w/feature/config","fingerprint":"0123456789012345678901234567890123456789"}}),
                    )
                }),
            ),
            (
                "selected task mismatch",
                copy.clone(),
                Box::new(|v| {
                    v["intent"]["Create"]["selected_tasks"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!("missing"))
                }),
            ),
        ];
        for (name, mut value, mutate) in cases {
            mutate(&mut value);
            let plan: OperationPlan = serde_json::from_value(value.clone())
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            if name == "task compensation injected" {
                assert!(
                    plan.validate_persisted().is_err(),
                    "{name}: persisted invariant"
                );
                continue;
            }
            assert!(plan.validate_persisted().is_ok(), "{name}: persisted shape");
            let _ = plan.validate_executable_plan();
            assert!(!name.is_empty());
        }
    }

    #[test]
    fn coordinated_grant_sets_must_match_intent() {
        let base = serde_json::to_value(
            plan_create(executable_artifact_input(
                FileArtifactKind::CopyFile,
                true,
                false,
                false,
                false,
            ))
            .unwrap(),
        )
        .unwrap();
        for (name, mutate) in [
            (
                "top-level-only grant",
                Box::new(|v: &mut serde_json::Value| {
                    v["granted_consents"] = json!(["file-rule:config"])
                }) as Box<dyn Fn(&mut serde_json::Value)>,
            ),
            (
                "intent-only grant",
                Box::new(|v: &mut serde_json::Value| {
                    v["intent"]["Create"]["granted_consents"] = json!(["file-rule:config"])
                }),
            ),
        ] {
            let mut value = base.clone();
            mutate(&mut value);
            crate::lifecycle::assert_shape_valid_but_not_executable(value);
            assert!(!name.is_empty());
        }
    }

    #[test]
    fn granted_create_and_remove_plans_roundtrip_executable() {
        let mut create =
            executable_artifact_input(FileArtifactKind::CopyFile, true, false, false, false);
        create
            .intent
            .granted_consents
            .insert(ConsentId::new("file-rule:config").unwrap());
        let create_plan = plan_create(create).unwrap();
        assert!(create_plan.validate_executable_plan().is_ok());
        let mut remove_intent = remove_intent();
        remove_intent
            .granted_consents
            .insert(ConsentId::new("remove:worktree").unwrap());
        let remove_plan = plan_remove(remove_input(remove_intent)).unwrap();
        assert!(remove_plan.validate_executable_plan().is_ok());
    }

    #[test]
    fn crafted_remove_mutations_are_persisted_but_not_executable() {
        fn each_object(
            value: &mut serde_json::Value,
            key: &str,
            f: &mut dyn FnMut(&mut serde_json::Value),
        ) {
            if let Some(object) = value.as_object_mut() {
                if let Some(found) = object.get_mut(key) {
                    f(found);
                }
                for child in object.values_mut() {
                    each_object(child, key, f);
                }
            } else if let Some(array) = value.as_array_mut() {
                for child in array {
                    each_object(child, key, f);
                }
            }
        }
        fn remove_variant(value: &mut serde_json::Value, key: &str) {
            if let Some(array) = value.as_array_mut() {
                array.retain(|item| item.get(key).is_none());
                for item in array {
                    remove_variant(item, key);
                }
            } else if let Some(object) = value.as_object_mut() {
                for child in object.values_mut() {
                    remove_variant(child, key);
                }
            }
        }
        let full = RemoveIntent::new(
            repo(),
            StoredPath::from(PathBuf::from("/w")),
            false,
            true,
            true,
            Some(RemoteBranch {
                remote: RemoteName::new("origin").unwrap(),
                branch: BranchName::new("feature").unwrap(),
            }),
            BTreeSet::new(),
        )
        .unwrap();
        let base = serde_json::to_value(plan_remove(remove_input(full)).unwrap()).unwrap();
        type Mutation = (&'static str, Box<dyn Fn(&mut serde_json::Value)>);
        let mut mutations: Vec<Mutation> = Vec::new();
        mutations.push((
            "CommonDirectory",
            Box::new(|v| remove_variant(v, "CommonDirectory")),
        ));
        mutations.push((
            "WorktreeAt path",
            Box::new(|v| each_object(v, "WorktreeAt", &mut |x| x["path"] = json!("/other"))),
        ));
        mutations.push((
            "WorktreeAt branch",
            Box::new(|v| each_object(v, "WorktreeAt", &mut |x| x["branch"] = json!("other"))),
        ));
        mutations.push((
            "WorktreeAt OID",
            Box::new(|v| {
                each_object(v, "WorktreeAt", &mut |x| {
                    x["oid"] = json!("fedcba9876543210fedcba9876543210fedcba98")
                })
            }),
        ));
        mutations.push((
            "WorktreeAt class",
            Box::new(|v| each_object(v, "WorktreeAt", &mut |x| x["class"] = json!("primary"))),
        ));
        mutations.push((
            "WorktreeAt missing",
            Box::new(|v| remove_variant(v, "WorktreeAt")),
        ));
        mutations.push((
            "WorktreeRemoved path",
            Box::new(|v| each_object(v, "WorktreeRemoved", &mut |x| x["path"] = json!("/other"))),
        ));
        mutations.push((
            "WorktreeRemoved OID",
            Box::new(|v| {
                each_object(v, "WorktreeRemoved", &mut |x| {
                    x["oid"] = json!("fedcba9876543210fedcba9876543210fedcba98")
                })
            }),
        ));
        mutations.push((
            "WorktreeRemoved duplicate",
            Box::new(|v| {
                each_object(v, "postconditions", &mut |x| {
                    if let Some(a) = x.as_array_mut()
                        && let Some(item) = a
                            .iter()
                            .find(|x| x.get("WorktreeRemoved").is_some())
                            .cloned()
                    {
                        a.push(item);
                    }
                })
            }),
        ));
        mutations.push((
            "action-vs-intent path",
            Box::new(|v| each_object(v, "RemoveWorktree", &mut |x| x["path"] = json!("/other"))),
        ));
        mutations.push((
            "local action branch",
            Box::new(|v| {
                each_object(v, "DeleteLocalBranch", &mut |x| {
                    x["branch"] = json!("other")
                })
            }),
        ));
        mutations.push((
            "local RefAt OID",
            Box::new(|v| {
                each_object(v, "RefAt", &mut |x| {
                    x["oid"] = json!("fedcba9876543210fedcba9876543210fedcba98")
                })
            }),
        ));
        mutations.push((
            "BranchDeleted missing",
            Box::new(|v| remove_variant(v, "BranchDeleted")),
        ));
        mutations.push((
            "remote action target",
            Box::new(|v| {
                each_object(v, "DeleteRemoteBranch", &mut |x| {
                    x["target"]["branch"] = json!("other")
                })
            }),
        ));
        mutations.push((
            "default guard",
            Box::new(|v| {
                each_object(v, "RemoteBranchNotDefault", &mut |x| {
                    x["remote"] = json!("other")
                })
            }),
        ));
        mutations.push((
            "RemoteRefAt remote",
            Box::new(|v| each_object(v, "RemoteRefAt", &mut |x| x["remote"] = json!("other"))),
        ));
        mutations.push((
            "RemoteRefAt branch",
            Box::new(|v| each_object(v, "RemoteRefAt", &mut |x| x["branch"] = json!("other"))),
        ));
        mutations.push((
            "RemoteBranchDeleted missing",
            Box::new(|v| remove_variant(v, "RemoteBranchDeleted")),
        ));
        mutations.push((
            "RemoteBranchDeleted wrong",
            Box::new(|v| {
                each_object(v, "RemoteBranchDeleted", &mut |x| {
                    x["branch"] = json!("other")
                })
            }),
        ));
        for (name, mutate) in mutations {
            let mut value = base.clone();
            mutate(&mut value);
            let plan: OperationPlan =
                serde_json::from_value(value.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(plan.validate_persisted().is_ok(), "{name}: shape");
            assert!(
                plan.validate_executable_plan().is_err(),
                "{name}: executable"
            );
            crate::lifecycle::assert_shape_valid_but_not_executable(value);
        }
    }

    #[test]
    fn isolated_remove_safety_and_consent_mutations_are_rejected() {
        fn visit(
            value: &mut serde_json::Value,
            key: &str,
            f: &mut dyn FnMut(&mut serde_json::Value),
        ) {
            if let Some(object) = value.as_object_mut() {
                if let Some(found) = object.get_mut(key) {
                    f(found);
                }
                for child in object.values_mut() {
                    visit(child, key, f);
                }
            } else if let Some(array) = value.as_array_mut() {
                for child in array {
                    visit(child, key, f);
                }
            }
        }
        fn remove_variant(value: &mut serde_json::Value, key: &str) {
            if let Some(array) = value.as_array_mut() {
                array.retain(|item| item.get(key).is_none());
                for child in array {
                    remove_variant(child, key);
                }
            } else if let Some(object) = value.as_object_mut() {
                for child in object.values_mut() {
                    remove_variant(child, key);
                }
            }
        }
        fn local_merge_plan(force: bool) -> serde_json::Value {
            let intent = RemoveIntent::new(
                repo(),
                StoredPath::from(PathBuf::from("/w")),
                false,
                true,
                force,
                Some(RemoteBranch {
                    remote: RemoteName::new("origin").unwrap(),
                    branch: BranchName::new("feature").unwrap(),
                }),
                BTreeSet::new(),
            )
            .unwrap();
            serde_json::to_value(plan_remove(remove_input(intent)).unwrap()).unwrap()
        }
        let merge = local_merge_plan(false);
        let force = local_merge_plan(true);
        type Mutation = (
            &'static str,
            serde_json::Value,
            Box<dyn Fn(&mut serde_json::Value)>,
        );
        let mut cases: Vec<Mutation> = vec![
            (
                "RefMergedInto missing",
                merge.clone(),
                Box::new(|v| remove_variant(v, "RefMergedInto")),
            ),
            (
                "RefMergedInto target_ref None",
                merge.clone(),
                Box::new(|v| {
                    visit(v, "RefMergedInto", &mut |x| {
                        x["target_ref"] = serde_json::Value::Null
                    })
                }),
            ),
            (
                "RefMergedInto target_ref wrong",
                merge.clone(),
                Box::new(|v| {
                    visit(v, "RefMergedInto", &mut |x| {
                        x["target_ref"] = json!("other")
                    })
                }),
            ),
            (
                "RefMergedInto target_oid wrong",
                merge.clone(),
                Box::new(|v| {
                    visit(v, "RefMergedInto", &mut |x| {
                        x["target_oid"] = json!("fedcba9876543210fedcba9876543210fedcba98")
                    })
                }),
            ),
            (
                "merge-target RefAt missing",
                merge.clone(),
                Box::new(|v| {
                    v["steps"][1]["preconditions"]
                        .as_array_mut()
                        .unwrap()
                        .retain(|x| {
                            x.get("RefAt")
                                .and_then(|r| r.get("reference"))
                                .is_none_or(|r| r != "HEAD")
                        })
                }),
            ),
            (
                "merge-target RefAt wrong ref",
                merge.clone(),
                Box::new(|v| {
                    v["steps"][1]["preconditions"]
                        .as_array_mut()
                        .unwrap()
                        .iter_mut()
                        .filter_map(|x| x.get_mut("RefAt"))
                        .for_each(|x| x["reference"] = json!("other"))
                }),
            ),
            (
                "merge-target RefAt wrong OID",
                merge.clone(),
                Box::new(|v| {
                    v["steps"][1]["preconditions"]
                        .as_array_mut()
                        .unwrap()
                        .iter_mut()
                        .filter_map(|x| x.get_mut("RefAt"))
                        .for_each(|x| {
                            if x["reference"] == "HEAD" {
                                x["oid"] = json!("fedcba9876543210fedcba9876543210fedcba98");
                            }
                        })
                }),
            ),
            (
                "nonforce missing merge proof",
                merge.clone(),
                Box::new(|v| remove_variant(v, "RefMergedInto")),
            ),
            (
                "force merge proof injected",
                force.clone(),
                Box::new(|v| {
                    let guard = json!({"RefMergedInto":{"reference":"feature","target_ref":"HEAD","target_oid":"0123456789012345678901234567890123456789"}});
                    v["steps"][1]["preconditions"]
                        .as_array_mut()
                        .unwrap()
                        .push(guard);
                }),
            ),
            (
                "BranchDeleted wrong",
                force.clone(),
                Box::new(|v| {
                    for post in v["steps"][1]["postconditions"].as_array_mut().unwrap() {
                        if post.get("BranchDeleted").is_some() {
                            post["BranchDeleted"] = json!("other");
                        }
                    }
                }),
            ),
            (
                "BranchDeleted duplicate",
                force.clone(),
                Box::new(|v| {
                    let posts = v["steps"][1]["postconditions"].as_array_mut().unwrap();
                    let item = posts
                        .iter()
                        .find(|x| x.get("BranchDeleted").is_some())
                        .unwrap()
                        .clone();
                    posts.push(item);
                }),
            ),
            (
                "RemoteRefAt OID wrong",
                force.clone(),
                Box::new(|v| {
                    visit(v, "RemoteRefAt", &mut |x| {
                        x["oid"] = json!("fedcba9876543210fedcba9876543210fedcba98")
                    })
                }),
            ),
            (
                "RemoteRefAt duplicate",
                force.clone(),
                Box::new(|v| {
                    let guards = v["steps"][2]["preconditions"].as_array_mut().unwrap();
                    let item = guards
                        .iter()
                        .find(|x| x.get("RemoteRefAt").is_some())
                        .unwrap()
                        .clone();
                    guards.push(item);
                }),
            ),
            (
                "remote expected OID missing",
                force.clone(),
                Box::new(|v| {
                    visit(v, "DeleteRemoteBranch", &mut |x| {
                        x.as_object_mut().unwrap().remove("expected_oid");
                    })
                }),
            ),
            (
                "remote expected OID wrong",
                force.clone(),
                Box::new(|v| {
                    visit(v, "DeleteRemoteBranch", &mut |x| {
                        x["expected_oid"] = json!("fedcba9876543210fedcba9876543210fedcba98")
                    })
                }),
            ),
            (
                "RemoteBranchNotDefault missing",
                force.clone(),
                Box::new(|v| remove_variant(v, "RemoteBranchNotDefault")),
            ),
            (
                "RemoteBranchNotDefault duplicate",
                force.clone(),
                Box::new(|v| {
                    let guards = v["steps"][2]["preconditions"].as_array_mut().unwrap();
                    let item = guards
                        .iter()
                        .find(|x| x.get("RemoteBranchNotDefault").is_some())
                        .unwrap()
                        .clone();
                    guards.push(item);
                }),
            ),
            (
                "remote deletion not final",
                force.clone(),
                Box::new(|v| {
                    let steps = v["steps"].as_array_mut().unwrap();
                    let remote = steps.pop().unwrap();
                    steps.insert(1, remote);
                }),
            ),
        ];
        let consent_base = force.clone();
        for (id, risk) in [
            ("remove:worktree", "remove_worktree"),
            ("remove:local-branch", "delete_local_branch"),
            ("remove:force-local-branch", "force_delete_local_branch"),
            ("remove:remote:origin/feature", "delete_remote_branch"),
        ] {
            cases.push((
                "consent missing",
                consent_base.clone(),
                Box::new(move |v| {
                    v["required_consents"]
                        .as_array_mut()
                        .unwrap()
                        .retain(|x| x["id"] != id);
                }),
            ));
            cases.push((
                "consent extra",
                consent_base.clone(),
                Box::new(move |v| {
                    let c = json!({"id":"remove:extra","risks":[{"kind":risk,"message":"extra"}]});
                    v["required_consents"].as_array_mut().unwrap().push(c);
                }),
            ));
            cases.push((
                "consent wrong RiskKind",
                consent_base.clone(),
                Box::new(move |v| {
                    for c in v["required_consents"].as_array_mut().unwrap() {
                        if c["id"] == id {
                            c["risks"][0]["kind"] = json!("dirty_data_loss");
                        }
                    }
                }),
            ));
        }
        for risk in [
            "remove_worktree",
            "delete_local_branch",
            "force_delete_local_branch",
            "delete_remote_branch",
        ] {
            cases.push((
                "plan Risk missing",
                consent_base.clone(),
                Box::new(move |v| {
                    v["risks"]
                        .as_array_mut()
                        .unwrap()
                        .retain(|x| x["kind"] != risk);
                }),
            ));
        }
        for (name, mut value, mutate) in cases {
            mutate(&mut value);
            let plan: OperationPlan = serde_json::from_value(value.clone())
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(plan.validate_persisted().is_ok(), "{name}: persisted shape");
            assert!(
                plan.validate_executable_plan().is_err(),
                "{name}: remained executable"
            );
            crate::lifecycle::assert_shape_valid_but_not_executable(value);
        }
    }

    #[test]
    fn empty_enabled_manifest_emits_no_artifact_contract_and_is_executable() {
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
        value.known_rules.insert("empty".into());
        value.enabled_rules.insert("empty".into());
        let empty_digest = canonical_manifest_digest(&[], value.destination.path.as_path());
        value.manifests = vec![FileActionManifest {
            rule: "empty".into(),
            source_root: value.repository.primary_root.clone(),
            artifacts: Vec::new(),
            digest: empty_digest,
        }];

        let plan = plan_create(value).unwrap();
        let OperationIntent::Create(intent) = plan.intent() else {
            panic!("expected create intent");
        };
        assert!(intent.artifact_rule_contracts.is_empty());
        assert!(
            plan.steps()
                .iter()
                .all(|step| !matches!(step.action(), StepAction::FileArtifact { .. }))
        );
        assert!(plan.validate_executable_plan().is_ok());
    }
}
