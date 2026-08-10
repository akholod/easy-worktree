#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use globset::GlobBuilder;
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::{
    application::{
        CreatePlanRequest, CreatePlanningFacts, CreateSourceRequest, LifecyclePlanningPort,
        ManifestPlanningPort, ManifestRuleSpec, PlanningError, RecoveryPort, RemovePlanRequest,
        RemovePlanningFacts, RepositoryPort,
    },
    domain::{
        CheckoutStatus, ListResult, Reason, RepositorySummary, StoredPath, Warning, Worktree,
        WorktreeClass,
    },
    lifecycle::{
        BranchName, Compensation, CreateSource, CreatedArtifact, ObjectId, RefName, RemoteBranch,
        RemoteName, RepositoryIdentity,
    },
    planner::{
        self, CreateSourceFacts, DestinationFacts, DestinationState, FileActionManifest,
        FileArtifact, FileArtifactKind, RemoveFacts,
    },
};

#[derive(Debug, Error)]
pub enum GitError {
    #[error("git discovery failed: {0}")]
    Discovery(String),
    #[error("git command failed: {0}")]
    Command(String),
    #[error("malformed git output: {0}")]
    Parse(String),
}

pub struct GitCli;

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("repository discovery failed: {0}")]
    Repository(String),
    #[error(transparent)]
    Journal(#[from] crate::journal_store::JournalError),
}

pub fn repository_roots(path: &Path) -> Result<(PathBuf, PathBuf), GitError> {
    let root = match git(path, ["rev-parse", "--show-toplevel"]) {
        Ok(output) => parse_path_line(&output.stdout)?,
        Err(_) => path.to_owned(),
    };
    let common = git(
        path,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok((root, parse_path_line(&common.stdout)?))
}

struct Discovery {
    common: PathBuf,
    bare: bool,
}

impl RepositoryPort for GitCli {
    type Error = GitError;

    fn list(&self, path: &Path) -> Result<ListResult, GitError> {
        let discovery = discover(path)?;
        let records =
            parse_worktrees(&git(path, ["worktree", "list", "--porcelain", "-z"])?.stdout)?;
        let mut worktrees = Vec::new();
        let mut warnings = Vec::new();
        for record in records {
            let bare = record.bare;
            let mut item = Worktree::new(record.path.clone());
            item.head_oid = record.head;
            item.branch = record
                .branch
                .map(|b| b.strip_prefix("refs/heads/").unwrap_or(&b).to_owned());
            item.detached = record.detached;
            item.classification = if bare {
                WorktreeClass::Bare
            } else {
                WorktreeClass::Unknown
            };
            item.locked = record.locked.map(|reason| Reason { reason });
            item.prunable = record.prunable.map(|reason| Reason { reason });
            if !bare && item.prunable.is_none() {
                match git(
                    &record.path,
                    ["rev-parse", "--path-format=absolute", "--absolute-git-dir"],
                ) {
                    Ok(output) => {
                        let git_dir = parse_path_line(&output.stdout)?;
                        item.classification = if same_path(&git_dir, &discovery.common) {
                            WorktreeClass::Primary
                        } else {
                            WorktreeClass::Linked
                        };
                    }
                    Err(error) => {
                        item.classification = WorktreeClass::Unknown;
                        warnings.push(warning(
                            "worktree_unavailable",
                            &error.to_string(),
                            &record.path,
                        ));
                    }
                }
                if !bare {
                    match git(&record.path, ["status", "--porcelain=v2", "--branch", "-z"]) {
                        Ok(output) => {
                            apply_status(&mut item, &output.stdout)?;
                            if let Some(branch) = item.branch.as_deref() {
                                item.upstream = readonly_branch_upstream(&record.path, branch)?;
                            }
                        }
                        Err(error) => {
                            item.status = CheckoutStatus::Unknown;
                            warnings.push(warning(
                                "status_unavailable",
                                &error.to_string(),
                                &record.path,
                            ));
                        }
                    }
                }
            } else if item.prunable.is_some() {
                warnings.push(warning(
                    "worktree_prunable",
                    "worktree is prunable",
                    &record.path,
                ));
            }
            worktrees.push(item);
        }
        Ok(ListResult {
            data: crate::domain::ListData {
                repository: RepositorySummary {
                    common_dir: discovery.common,
                    bare: discovery.bare,
                },
                worktrees,
            },
            warnings,
        })
    }
}

impl RecoveryPort for GitCli {
    type Error = RecoveryError;
    fn recover_list(&self, repo: &Path) -> Result<Vec<crate::journal::Journal>, Self::Error> {
        let common = discover(repo)
            .map_err(|e| RecoveryError::Repository(e.to_string()))?
            .common;
        crate::journal_store::JournalStore::new(&common)
            .list()
            .map_err(RecoveryError::Journal)
    }
    fn recover_show(
        &self,
        repo: &Path,
        id: &crate::lifecycle::OperationId,
    ) -> Result<crate::journal::Journal, Self::Error> {
        let common = discover(repo)
            .map_err(|e| RecoveryError::Repository(e.to_string()))?
            .common;
        crate::journal_store::JournalStore::new(&common)
            .read(id)
            .map_err(RecoveryError::Journal)
    }
    fn recovery_error_code(error: &Self::Error) -> &'static str {
        match error {
            RecoveryError::Repository(_) => "repository_error",
            RecoveryError::Journal(crate::journal_store::JournalError::NotFound) => {
                "journal_not_found"
            }
            RecoveryError::Journal(crate::journal_store::JournalError::InvalidId) => {
                "invalid_operation_id"
            }
            RecoveryError::Journal(crate::journal_store::JournalError::Corrupt(_)) => {
                "journal_corrupt"
            }
            RecoveryError::Journal(crate::journal_store::JournalError::RepositoryBusy) => {
                "repository_busy"
            }
            RecoveryError::Journal(crate::journal_store::JournalError::RevisionConflict) => {
                "journal_revision_conflict"
            }
            RecoveryError::Journal(crate::journal_store::JournalError::ImmutableMismatch) => {
                "journal_immutable_mismatch"
            }
            RecoveryError::Journal(crate::journal_store::JournalError::InvalidTransition) => {
                "journal_invalid_transition"
            }
            RecoveryError::Journal(crate::journal_store::JournalError::Io(_)) => "journal_io",
        }
    }
}

impl LifecyclePlanningPort for GitCli {
    fn create_facts(
        &self,
        request: &CreatePlanRequest,
        default_base: Option<&str>,
        remote: &str,
        worktree_root: Option<&str>,
        directory_prefix: Option<&str>,
    ) -> Result<CreatePlanningFacts, PlanningError> {
        let listing = self.list(&request.repo).map_err(plan_error)?;
        let primary = listing
            .data
            .worktrees
            .iter()
            .find(|item| item.classification == WorktreeClass::Primary)
            .ok_or_else(|| {
                planning("no_primary", "repository has no confirmed primary worktree")
            })?;
        if listing.data.repository.bare {
            return Err(planning(
                "bare_repository",
                "bare repositories cannot create worktrees",
            ));
        }
        let primary_oid = parse_oid(
            primary
                .head_oid
                .as_deref()
                .ok_or_else(|| planning("missing_oid", "primary HEAD has no object id"))?,
        )?;
        let identity = RepositoryIdentity {
            common_dir: primary_path(&listing.data.repository.common_dir),
            primary_root: primary_path(&primary.path),
            repository_oid: primary_oid.clone(),
        };
        let (source, facts) = resolve_create_source(
            &request.repo,
            request,
            default_base,
            remote,
            &listing.data.worktrees,
        )?;
        let branch = match &source {
            CreateSource::NewBranch { branch, .. } | CreateSource::ExistingLocal { branch } => {
                branch.clone()
            }
            CreateSource::RemoteTracking { local_branch, .. } => local_branch.clone(),
        };
        let current_root = listing
            .data
            .worktrees
            .iter()
            .filter_map(|item| {
                path_is_within(&item.path, &request.invocation_cwd).then_some((
                    planner::normalize_lexical(item.path.clone())
                        .components()
                        .count(),
                    item.path.clone(),
                ))
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, path)| path)
            .unwrap_or_else(|| primary.path.clone());
        let destination_path = request
            .custom_path
            .clone()
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    request.invocation_cwd.join(path)
                }
            })
            .unwrap_or_else(|| {
                planner::destination_for_options(
                    worktree_root,
                    directory_prefix,
                    primary.path.as_path(),
                    branch.as_str(),
                    request.invocation_cwd.as_path(),
                )
            });
        let destination = destination_facts(destination_path)?;
        let branch_collision = matches!(
            source,
            CreateSource::NewBranch { .. } | CreateSource::RemoteTracking { .. }
        ) && ref_exists(&request.repo, &format!("refs/heads/{branch}"))?;
        Ok(CreatePlanningFacts {
            repository: identity,
            source,
            source_facts: facts,
            bare: listing.data.repository.bare,
            primary_count: listing
                .data
                .worktrees
                .iter()
                .filter(|item| item.classification == WorktreeClass::Primary)
                .count(),
            invocation_cwd: request.invocation_cwd.clone(),
            primary_root: primary_path(&primary.path),
            current_worktree_root: primary_path(&current_root),
            destination,
            branch_checked_out: listing
                .data
                .worktrees
                .iter()
                .any(|item| item.branch.as_deref() == Some(branch.as_str())),
            branch_collision,
        })
    }

    fn remove_facts(
        &self,
        request: &RemovePlanRequest,
    ) -> Result<RemovePlanningFacts, PlanningError> {
        let listing = self.list(&request.repo).map_err(plan_error)?;
        let primary = listing
            .data
            .worktrees
            .iter()
            .find(|item| item.classification == WorktreeClass::Primary)
            .ok_or_else(|| {
                planning("no_primary", "repository has no confirmed primary worktree")
            })?;
        if listing.data.repository.bare {
            return Err(planning(
                "bare_repository",
                "bare repositories cannot remove worktrees",
            ));
        }
        let primary_oid = parse_oid(
            primary
                .head_oid
                .as_deref()
                .ok_or_else(|| planning("missing_oid", "primary HEAD has no object id"))?,
        )?;
        let identity = RepositoryIdentity {
            common_dir: primary_path(&listing.data.repository.common_dir),
            primary_root: primary_path(&primary.path),
            repository_oid: primary_oid.clone(),
        };
        let target = resolve_worktree_target(
            &request.target,
            &request.invocation_cwd,
            &listing.data.worktrees,
        )?;
        if target.status == CheckoutStatus::Unknown {
            return Err(planning(
                "status_unavailable",
                "worktree status is unavailable",
            ));
        }
        let worktree_oid = parse_oid(
            target
                .head_oid
                .as_deref()
                .ok_or_else(|| planning("missing_oid", "worktree has no HEAD object id"))?,
        )?;
        let current_oid = git_oid(&target.path, "HEAD")?;
        let branch = target
            .branch
            .as_deref()
            .ok_or_else(|| planning("detached_worktree", "target worktree is detached"))?;
        let branch = BranchName::new(branch.to_owned())
            .map_err(|message| planning("invalid_branch", &message))?;
        let branch_oid = git_oid(&request.repo, &format!("refs/heads/{branch}"))?;
        let remote_branch = request.delete_remote_branch.clone();
        let (remote_branch_oid, remote_is_default) = if let Some(remote) = &remote_branch {
            remote_facts(&request.repo, remote)?
        } else {
            (None, false)
        };
        let (safe_target_ref, safe_target, merge_provenance) = safety_target(
            &request.repo,
            target.branch.as_deref().unwrap_or(branch.as_str()),
            target.upstream.as_deref(),
            &primary_oid,
        )?;
        let local_safe = is_ancestor(&request.repo, &branch_oid, &safe_target)?;
        let ongoing = ongoing_git_operation(&target.path)?;
        let branch_elsewhere = listing.data.worktrees.iter().any(|item| {
            item.path != target.path && item.branch.as_deref() == Some(branch.as_str())
        });
        let facts = RemoveFacts {
            repository: identity.clone(),
            class: target.classification,
            locked: target.locked.is_some(),
            prunable: target.prunable.is_some(),
            ongoing,
            oid_matches: current_oid == worktree_oid,
            branch_elsewhere,
            dirty: target.status == CheckoutStatus::Dirty,
            local_branch_safe_to_delete: local_safe,
            safe_target_ref,
            safe_target,
            merge_provenance,
            branch,
            branch_oid,
            worktree_oid,
            remote_branch: remote_branch.clone(),
            remote_branch_oid,
            remote_is_default,
            path: StoredPath::from(target.path.clone()),
        };
        Ok(RemovePlanningFacts {
            repository: identity,
            facts,
        })
    }
}

impl ManifestPlanningPort for GitCli {
    fn plan_manifests(
        &self,
        _request: &CreatePlanRequest,
        facts: &CreatePlanningFacts,
        rules: Vec<ManifestRuleSpec>,
    ) -> Result<Vec<FileActionManifest>, PlanningError> {
        let mut manifests = Vec::new();
        for spec in rules {
            let source_root = match spec.source_root {
                crate::config::SourceRoot::CurrentWorktree => {
                    facts.current_worktree_root.as_path().to_owned()
                }
                crate::config::SourceRoot::PrimaryWorktree => {
                    facts.primary_root.as_path().to_owned()
                }
            };
            validate_destination(&spec.destination)?;
            let candidates = manifest_candidates(&source_root, &spec)?;
            let mut artifacts = Vec::new();
            for (relative, source) in candidates {
                let output_relative = if spec.match_mode == crate::config::MatchMode::Glob {
                    relative.clone()
                } else if spec.kind == crate::config::FileRuleKind::CopyTree {
                    relative
                        .strip_prefix(&spec.source)
                        .unwrap_or(&relative)
                        .to_owned()
                } else {
                    PathBuf::new()
                };
                let destination = if spec.match_mode == crate::config::MatchMode::Glob
                    || spec.kind == crate::config::FileRuleKind::CopyTree
                {
                    facts
                        .destination
                        .path
                        .as_path()
                        .join(&spec.destination)
                        .join(&output_relative)
                } else {
                    facts.destination.path.as_path().join(&spec.destination)
                };
                if is_forbidden_destination(&destination, facts.destination.path.as_path()) {
                    return Err(planning(
                        "forbidden_destination",
                        "file rule destination enters reserved state area",
                    ));
                }
                artifacts.push(make_artifact(
                    &spec,
                    source_root.clone(),
                    relative,
                    source,
                    destination,
                )?);
            }
            artifacts
                .sort_by(|left, right| left.destination.as_path().cmp(right.destination.as_path()));
            let digest =
                manifest_digest(&artifacts, facts.destination.path.as_path(), &source_root);
            manifests.push(FileActionManifest {
                rule: spec.name,
                source_root: StoredPath::from(source_root),
                artifacts,
                digest,
            });
        }
        manifests.sort_by(|left, right| left.rule.cmp(&right.rule));
        let mut destinations = Vec::new();
        for manifest in &manifests {
            for artifact in &manifest.artifacts {
                destinations.push(artifact.destination.as_path().to_owned());
            }
        }
        if crate::lifecycle::destination_paths_overlap(
            &destinations
                .into_iter()
                .map(StoredPath::from)
                .collect::<Vec<_>>(),
        ) {
            return Err(planning(
                "manifest_overlap",
                "file rule destinations overlap",
            ));
        }
        Ok(manifests)
    }
}

fn validate_destination(value: &str) -> Result<(), PlanningError> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(planning(
            "forbidden_destination",
            "destination must remain under the future worktree",
        ));
    }
    if path.components().any(|component| matches!(component, std::path::Component::Normal(part) if part == ".git" || part == ".ewtm" || part == "ewtm")) { return Err(planning("forbidden_destination", "destination enters reserved state area")); }
    Ok(())
}

fn is_forbidden_destination(path: &Path, root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    relative.components().any(|component| matches!(component, std::path::Component::Normal(part) if part == ".git" || part == ".ewtm" || part == "ewtm"))
}

fn manifest_candidates(
    root: &Path,
    rule: &ManifestRuleSpec,
) -> Result<Vec<(PathBuf, PathBuf)>, PlanningError> {
    let root = root
        .canonicalize()
        .map_err(|error| planning("source_read", &error.to_string()))?;
    ensure_no_symlink_components(&root)?;
    let mut paths = if rule.ignored_only {
        ignored_candidates(&root)?
    } else {
        Vec::new()
    };
    if !rule.ignored_only {
        if rule.match_mode == crate::config::MatchMode::Path {
            let source = PathBuf::from(&rule.source);
            let source_path = root.join(&source);
            ensure_no_symlink_parent_components(&source_path)?;
            let metadata = std::fs::symlink_metadata(&source_path)
                .map_err(|error| planning("source_read", &error.to_string()))?;
            if metadata.file_type().is_symlink()
                || metadata.is_file()
                || (metadata.is_dir() && rule.kind == crate::config::FileRuleKind::Symlink)
            {
                paths.push((source, source_path));
            } else if metadata.is_dir() && rule.kind == crate::config::FileRuleKind::CopyTree {
                walk_candidates(&root, &source, &mut paths)?;
            } else if metadata.is_dir() {
                return Err(planning(
                    "source_type",
                    "source directory is invalid for this rule kind",
                ));
            } else {
                return Err(planning(
                    "special_source",
                    "special source file is not allowed",
                ));
            }
        } else {
            walk_candidates(&root, Path::new(""), &mut paths)?;
        }
    }
    let matcher = GlobBuilder::new(&rule.source)
        .literal_separator(true)
        .case_insensitive(false)
        .build()
        .map_err(|error| planning("invalid_glob", &error.to_string()))?
        .compile_matcher();
    let mut result = Vec::new();
    for (relative, source) in paths {
        let matches = if rule.match_mode == crate::config::MatchMode::Path {
            relative == Path::new(&rule.source)
                || (rule.kind == crate::config::FileRuleKind::CopyTree
                    && relative.starts_with(Path::new(&rule.source)))
        } else {
            matcher.is_match(&relative)
        };
        if !matches || excluded(&relative, &rule.excludes)? {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&source)
            .map_err(|error| planning("source_read", &error.to_string()))?;
        if metadata.file_type().is_symlink() {
            if rule.kind == crate::config::FileRuleKind::Relink {
                result.push((relative, source));
            } else if rule.kind == crate::config::FileRuleKind::Symlink {
                return Err(planning(
                    "source_type",
                    "symlink source must not itself be a symlink",
                ));
            } else {
                return Err(planning(
                    "special_source",
                    "copy source symlink is not allowed",
                ));
            }
            continue;
        }
        if rule.kind == crate::config::FileRuleKind::CopyTree && metadata.is_dir() {
            continue;
        }
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(planning(
                "special_source",
                "special source file is not allowed",
            ));
        }
        result.push((relative, source));
    }
    if rule.match_mode == crate::config::MatchMode::Path
        && rule.kind == crate::config::FileRuleKind::CopyTree
    {
        let source = root.join(&rule.source);
        result.retain(|(_, path)| path == &source || path.starts_with(&source));
    }
    Ok(result)
}

fn ensure_no_symlink_components(path: &Path) -> Result<(), PlanningError> {
    let mut cursor = if path.is_absolute() {
        PathBuf::from(std::path::MAIN_SEPARATOR.to_string())
    } else {
        PathBuf::new()
    };
    for component in path.components() {
        cursor.push(component.as_os_str());
        if let Ok(metadata) = std::fs::symlink_metadata(&cursor)
            && metadata.file_type().is_symlink()
        {
            return Err(planning(
                "unsafe_source",
                "source path contains a symlink component",
            ));
        }
    }
    Ok(())
}

fn ensure_no_symlink_parent_components(path: &Path) -> Result<(), PlanningError> {
    let parent = path.parent().unwrap_or(Path::new(""));
    ensure_no_symlink_components(parent)
}

fn walk_candidates(
    root: &Path,
    relative: &Path,
    output: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), PlanningError> {
    let current = root.join(relative);
    let metadata = std::fs::symlink_metadata(&current)
        .map_err(|error| planning("source_read", &error.to_string()))?;
    if metadata.file_type().is_symlink() {
        output.push((relative.to_owned(), current));
        return Ok(());
    }
    if metadata.is_file() {
        output.push((relative.to_owned(), current));
        return Ok(());
    }
    if metadata.is_dir() {
        let mut entries = std::fs::read_dir(&current)
            .map_err(|error| planning("source_read", &error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| planning("source_read", &error.to_string()))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let child = relative.join(entry.file_name());
            walk_candidates(root, &child, output)?;
        }
    }
    if !metadata.is_file() && !metadata.is_dir() {
        output.push((relative.to_owned(), current));
    }
    Ok(())
}

fn ignored_candidates(root: &Path) -> Result<Vec<(PathBuf, PathBuf)>, PlanningError> {
    let output = git(
        root,
        [
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
    )
    .map_err(plan_error)?;
    let mut result = Vec::new();
    for value in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
    {
        let relative = PathBuf::from(std::ffi::OsString::from_vec(value.to_vec()));
        result.push((relative.clone(), root.join(relative)));
    }
    Ok(result)
}

fn excluded(relative: &Path, excludes: &[String]) -> Result<bool, PlanningError> {
    for exclude in excludes {
        let pattern = exclude.as_str();
        let matcher = GlobBuilder::new(pattern)
            .literal_separator(true)
            .case_insensitive(false)
            .build()
            .map_err(|error| planning("invalid_glob", &error.to_string()))?
            .compile_matcher();
        if pattern.contains('/') {
            if matcher.is_match(relative) {
                return Ok(true);
            }
        } else if relative
            .file_name()
            .is_some_and(|name| matcher.is_match(name))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn digest_bytes(bytes: &[u8]) -> ObjectId {
    crate::planner::artifact_digest(bytes)
}

fn manifest_digest(artifacts: &[FileArtifact], root: &Path, source_root: &Path) -> ObjectId {
    let contracts = artifacts
        .iter()
        .map(|artifact| crate::planner::ManifestDigestArtifact {
            source_root: StoredPath::from(source_root.to_owned()),
            source: artifact.source.clone(),
            destination: artifact.destination.clone(),
            kind: artifact.kind,
            bytes: artifact.bytes,
            digest: artifact.digest.clone(),
            fingerprint: artifact.fingerprint.clone(),
            link_target: artifact.link_target.clone(),
            sensitive: artifact.sensitive,
            confirm: artifact.confirm,
            mode_policy: artifact.mode_policy,
        })
        .collect::<Vec<_>>();
    crate::planner::canonical_manifest_digest(&contracts, root)
}

fn make_artifact(
    spec: &ManifestRuleSpec,
    root: PathBuf,
    _relative: PathBuf,
    source: PathBuf,
    destination: PathBuf,
) -> Result<FileArtifact, PlanningError> {
    if spec.kind == crate::config::FileRuleKind::Relink {
        return Err(planning(
            "relink_v3_unavailable",
            "relink planning is unavailable in v3",
        ));
    }
    let observed = readonly_observe_absolute_node(&source)
        .map_err(|error| planning("source_read", &error.to_string()))?
        .ok_or_else(|| planning("source_read", "source disappeared or is not observable"))?;
    let (kind, bytes, digest, fingerprint, link_target, compensation, source_expectation) =
        match spec.kind {
            crate::config::FileRuleKind::Copy | crate::config::FileRuleKind::CopyTree => {
                let ObservedNode::Regular { bytes: data, mode } = observed else {
                    return Err(planning(
                        "source_type",
                        "copy source must be a regular file",
                    ));
                };
                let digest = digest_bytes(&data);
                (
                    FileArtifactKind::CopyFile,
                    data.len() as u64,
                    digest.clone(),
                    digest.clone(),
                    None,
                    Some(Compensation::RemoveCreatedArtifact(CreatedArtifact {
                        path: StoredPath::from(destination.clone()),
                        fingerprint: digest.clone(),
                    })),
                    crate::lifecycle::ArtifactSourceExpectationV3::Regular(
                        crate::lifecycle::RegularFileStateV3 {
                            bytes: data.len() as u64,
                            digest: digest.clone(),
                            mode,
                        },
                    ),
                )
            }
            crate::config::FileRuleKind::Symlink => {
                let source_expectation = match observed {
                    ObservedNode::Regular { bytes, mode } => {
                        crate::lifecycle::ArtifactSourceExpectationV3::Regular(
                            crate::lifecycle::RegularFileStateV3 {
                                bytes: bytes.len() as u64,
                                digest: digest_bytes(&bytes),
                                mode,
                            },
                        )
                    }
                    ObservedNode::Directory => {
                        crate::lifecycle::ArtifactSourceExpectationV3::Directory
                    }
                    ObservedNode::Symlink { .. } => {
                        return Err(planning(
                            "source_type",
                            "symlink source must be a regular file or directory",
                        ));
                    }
                };
                let target = StoredPath::from(source.clone());
                let bytes = target.as_path().as_os_str().as_encoded_bytes();
                let digest = digest_bytes(bytes);
                (
                    FileArtifactKind::CreateSymlink,
                    bytes.len() as u64,
                    digest.clone(),
                    digest.clone(),
                    Some(target.clone()),
                    Some(Compensation::RemoveCreatedArtifact(CreatedArtifact {
                        path: StoredPath::from(destination.clone()),
                        fingerprint: digest.clone(),
                    })),
                    source_expectation,
                )
            }
            crate::config::FileRuleKind::Relink => unreachable!(),
        };
    let _ = root;
    Ok(FileArtifact {
        kind,
        source: StoredPath::from(source),
        destination: StoredPath::from(destination),
        bytes,
        digest,
        source_expectation,
        fingerprint,
        link_target,
        sensitive: spec.sensitive,
        mode_policy: match kind {
            FileArtifactKind::CopyFile if spec.sensitive => crate::planner::FileModePolicy::Private,
            FileArtifactKind::CopyFile => crate::planner::FileModePolicy::PreserveSafe,
            FileArtifactKind::CreateSymlink | FileArtifactKind::RelinkSymlink => {
                crate::planner::FileModePolicy::NotApplicable
            }
        },
        confirm: spec.confirm,
        conflict: false,
        overlap: false,
        replace_symlink: spec.kind == crate::config::FileRuleKind::Relink,
        compensation,
    })
}

fn planning(code: &str, message: &str) -> PlanningError {
    PlanningError {
        code: code.into(),
        message: message.into(),
    }
}
fn plan_error(error: GitError) -> PlanningError {
    planning("git_facts", &error.to_string())
}
fn primary_path(path: &Path) -> StoredPath {
    StoredPath::from(path.to_owned())
}
fn parse_oid(value: &str) -> Result<ObjectId, PlanningError> {
    ObjectId::new(value.trim().to_owned()).map_err(|message| planning("invalid_oid", &message))
}
fn git_oid(cwd: &Path, reference: &str) -> Result<ObjectId, PlanningError> {
    let output = git(
        cwd,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("--end-of-options"),
            OsString::from(format!("{reference}^{{commit}}")),
        ],
    )
    .map_err(plan_error)?;
    parse_oid(&String::from_utf8_lossy(&output.stdout))
}
fn validate_branch(cwd: &Path, branch: &str) -> Result<BranchName, PlanningError> {
    if branch.starts_with("@{-") {
        return Err(planning(
            "invalid_branch",
            "branch shorthand is not allowed",
        ));
    }
    git(
        cwd,
        vec![
            OsString::from("check-ref-format"),
            OsString::from("--branch"),
            OsString::from(branch),
        ],
    )
    .map_err(plan_error)?;
    BranchName::new(branch.to_owned()).map_err(|message| planning("invalid_branch", &message))
}
fn ref_exists(cwd: &Path, reference: &str) -> Result<bool, PlanningError> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args([
            OsStr::new("show-ref"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new(reference),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| planning("git_facts", &error.to_string()))?;
    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    Err(planning(
        "git_ref_failure",
        &String::from_utf8_lossy(&output.stderr),
    ))
}

fn remote_head(repo: &Path, remote: &str) -> Result<(String, ObjectId), PlanningError> {
    let output = git(
        repo,
        vec![
            OsString::from("ls-remote"),
            OsString::from("--symref"),
            OsString::from(remote),
            OsString::from("HEAD"),
        ],
    )
    .map_err(plan_error)?;
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| planning("malformed_remote_head", "remote HEAD response is not UTF-8"))?;
    let mut reference = None;
    let mut oid = None;
    for line in text.lines().filter(|line| !line.is_empty()) {
        if let Some(value) = line.strip_prefix("ref: ") {
            let (value, name) = value.split_once('\t').ok_or_else(|| {
                planning("malformed_remote_head", "malformed remote symbolic HEAD")
            })?;
            if name != "HEAD" || reference.replace(value.to_owned()).is_some() {
                return Err(planning(
                    "malformed_remote_head",
                    "duplicate or invalid remote symbolic HEAD",
                ));
            }
        } else {
            let (value, name) = line
                .split_once('\t')
                .ok_or_else(|| planning("malformed_remote_head", "malformed remote HEAD object"))?;
            if name != "HEAD" || oid.replace(parse_oid(value)?).is_some() {
                return Err(planning(
                    "malformed_remote_head",
                    "duplicate or invalid remote HEAD object",
                ));
            }
        }
    }
    let reference = reference.ok_or_else(|| {
        planning(
            "no_default_remote_head",
            "configured remote has no symbolic HEAD",
        )
    })?;
    RefName::new(reference.clone())
        .map_err(|message| planning("malformed_remote_head", &message))?;
    Ok((
        reference,
        oid.ok_or_else(|| {
            planning(
                "no_default_remote_head",
                "configured remote HEAD has no object id",
            )
        })?,
    ))
}

fn destination_facts(path: PathBuf) -> Result<DestinationFacts, PlanningError> {
    let path = planner::normalize_lexical(path);
    if !path.is_absolute() {
        return Err(planning(
            "destination_not_absolute",
            "destination must be absolute",
        ));
    }
    let state = match std::fs::symlink_metadata(&path) {
        Ok(_) => match std::fs::metadata(&path) {
            Ok(_) => DestinationState::Present,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                DestinationState::Dangling
            }
            Err(error) => return Err(planning("destination_io", &error.to_string())),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DestinationState::Absent,
        Err(error) => return Err(planning("destination_io", &error.to_string())),
    };
    let parent = path
        .parent()
        .ok_or_else(|| planning("unsafe_parent", "destination has no parent"))?
        .to_owned();
    let mut cursor = PathBuf::new();
    let mut passed_normal_component = false;
    if parent.is_absolute() {
        cursor.push(std::path::MAIN_SEPARATOR.to_string());
    }
    for component in parent.components() {
        cursor.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&cursor)
            .map_err(|error| planning("unsafe_parent", &error.to_string()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            if metadata.file_type().is_symlink() && !passed_normal_component {
                continue;
            }
            return Ok(DestinationFacts {
                path: StoredPath::from(path),
                state,
                parent: StoredPath::from(parent),
                parent_safe: false,
            });
        }
        if matches!(component, std::path::Component::Normal(_)) {
            passed_normal_component = true;
        }
    }
    Ok(DestinationFacts {
        path: StoredPath::from(path),
        state,
        parent: StoredPath::from(parent),
        parent_safe: true,
    })
}

fn resolve_worktree_target(
    target: &Path,
    cwd: &Path,
    worktrees: &[Worktree],
) -> Result<Worktree, PlanningError> {
    let path = planner::normalize_lexical(if target.is_absolute() {
        target.to_owned()
    } else {
        cwd.join(target)
    });
    if let Some(item) = worktrees.iter().find(|item| same_path(&item.path, &path)) {
        return Ok(item.clone());
    }
    let value = target.to_str().ok_or_else(|| {
        planning(
            "target_not_found",
            "target is neither a registered path nor UTF-8 branch",
        )
    })?;
    let matches: Vec<_> = worktrees
        .iter()
        .filter(|item| item.branch.as_deref() == Some(value))
        .cloned()
        .collect();
    match matches.as_slice() {
        [item] => Ok(item.clone()),
        [] => Err(planning("target_not_found", "worktree target not found")),
        _ => Err(planning(
            "target_ambiguous",
            "branch target matches multiple worktrees",
        )),
    }
}

fn ongoing_git_operation(path: &Path) -> Result<bool, PlanningError> {
    for name in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
        "rebase-merge",
        "rebase-apply",
    ] {
        let output = git(
            path,
            vec![
                OsString::from("rev-parse"),
                OsString::from("--git-path"),
                OsString::from(name),
            ],
        )
        .map_err(plan_error)?;
        let state_path = parse_path_line(&output.stdout).map_err(plan_error)?;
        let state_path = if state_path.is_absolute() {
            state_path
        } else {
            path.join(state_path)
        };
        match std::fs::symlink_metadata(state_path) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(planning("ongoing_probe", &error.to_string())),
        }
    }
    Ok(false)
}

fn is_ancestor(
    repo: &Path,
    ancestor: &ObjectId,
    descendant: &ObjectId,
) -> Result<bool, PlanningError> {
    let output = Command::new("git")
        .current_dir(repo)
        .args([
            OsStr::new("merge-base"),
            OsStr::new("--is-ancestor"),
            OsStr::new(ancestor.as_str()),
            OsStr::new(descendant.as_str()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .map_err(|error| planning("ancestor_probe_failure", &error.to_string()))?;
    match output.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(planning(
            "ancestor_probe_failure",
            "git merge-base ancestor probe failed",
        )),
    }
}

fn safety_target(
    repo: &Path,
    branch: &str,
    upstream: Option<&str>,
    primary_oid: &ObjectId,
) -> Result<(RefName, ObjectId, crate::lifecycle::MergeTargetProvenance), PlanningError> {
    match upstream {
        Some(reference) => Ok((
            RefName::new(reference.to_owned())
                .map_err(|message| planning("invalid_upstream", &message))?,
            git_oid(repo, reference)?,
            crate::lifecycle::MergeTargetProvenance::Upstream {
                branch: crate::lifecycle::BranchName::new(branch.to_owned())
                    .map_err(|message| planning("invalid_upstream", &message))?,
                upstream_ref: RefName::new(reference.to_owned())
                    .map_err(|message| planning("invalid_upstream", &message))?,
            },
        )),
        None => Ok((
            RefName::new("HEAD").expect("HEAD is a valid ref name"),
            primary_oid.clone(),
            crate::lifecycle::MergeTargetProvenance::Primary,
        )),
    }
}

fn remote_facts(
    repo: &Path,
    target: &RemoteBranch,
) -> Result<(Option<ObjectId>, bool), PlanningError> {
    let reference = format!("refs/heads/{}", target.branch);
    let output = git(
        repo,
        vec![
            OsString::from("ls-remote"),
            OsString::from("--refs"),
            OsString::from(target.remote.as_str()),
            OsString::from(&reference),
        ],
    )
    .map_err(plan_error)?;
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| planning("malformed_remote_ref", "remote ref response is not UTF-8"))?;
    let mut found = None;
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (value, name) = line
            .split_once('\t')
            .ok_or_else(|| planning("malformed_remote_ref", "malformed remote ref response"))?;
        if name != reference || found.replace(parse_oid(value)?).is_some() {
            return Err(planning(
                "malformed_remote_ref",
                "duplicate or mismatched remote ref response",
            ));
        }
    }
    let (default_ref, _) = remote_head(repo, target.remote.as_str())?;
    Ok((
        Some(
            found.ok_or_else(|| {
                planning("remote_ref_missing", "requested remote branch is missing")
            })?,
        ),
        default_ref == reference,
    ))
}
fn resolve_create_source(
    repo: &Path,
    request: &CreatePlanRequest,
    default_base: Option<&str>,
    remote: &str,
    worktrees: &[Worktree],
) -> Result<(CreateSource, CreateSourceFacts), PlanningError> {
    match &request.source {
        CreateSourceRequest::New { branch, base } => {
            let branch = validate_branch(repo, branch)?;
            let base_ref = base
                .clone()
                .or_else(|| default_base.map(str::to_owned))
                .unwrap_or_else(|| format!("refs/remotes/{remote}/HEAD"));
            let (resolved_ref, base_oid) = if base_ref == format!("refs/remotes/{remote}/HEAD") {
                remote_head(repo, remote)?
            } else {
                (base_ref.clone(), git_oid(repo, &base_ref)?)
            };
            if ref_exists(repo, &format!("refs/heads/{branch}"))? {
                return Err(planning("branch_collision", "local branch already exists"));
            }
            Ok((
                CreateSource::NewBranch {
                    branch: branch.clone(),
                    base: Some(
                        RefName::new(resolved_ref.clone())
                            .map_err(|message| planning("invalid_ref", &message))?,
                    ),
                },
                CreateSourceFacts::NewBranch {
                    branch,
                    base_ref: RefName::new(resolved_ref)
                        .map_err(|message| planning("invalid_ref", &message))?,
                    base_oid,
                    branch_absent: true,
                },
            ))
        }
        CreateSourceRequest::ExistingLocal { branch } => {
            let branch = validate_branch(repo, branch)?;
            let oid = git_oid(repo, &format!("refs/heads/{branch}"))?;
            let checked = worktrees
                .iter()
                .any(|item| item.branch.as_deref() == Some(branch.as_str()));
            if checked {
                return Err(planning(
                    "branch_checked_out",
                    "branch is checked out in a worktree",
                ));
            }
            Ok((
                CreateSource::ExistingLocal {
                    branch: branch.clone(),
                },
                CreateSourceFacts::ExistingLocal {
                    branch,
                    branch_oid: oid,
                    not_checked_out: true,
                },
            ))
        }
        CreateSourceRequest::RemoteTracking {
            remote,
            remote_branch,
            local_branch,
        } => {
            let remote_name = RemoteName::new(remote.clone())
                .map_err(|message| planning("invalid_remote", &message))?;
            let remote_branch = validate_branch(repo, remote_branch)?;
            let local_branch = validate_branch(repo, local_branch)?;
            let oid = git_oid(repo, &format!("refs/remotes/{remote}/{remote_branch}"))?;
            if ref_exists(repo, &format!("refs/heads/{local_branch}"))? {
                return Err(planning("branch_collision", "local branch already exists"));
            }
            Ok((
                CreateSource::RemoteTracking {
                    remote: remote_name.clone(),
                    remote_branch: remote_branch.clone(),
                    local_branch: local_branch.clone(),
                },
                CreateSourceFacts::RemoteTracking {
                    remote: remote_name,
                    remote_branch,
                    remote_oid: oid,
                    local_branch,
                    local_absent: true,
                },
            ))
        }
    }
}

fn discover(path: &Path) -> Result<Discovery, GitError> {
    let common = git(
        path,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = parse_path_line(&common.stdout)?;
    let _git_dir = parse_path_line(
        &git(
            path,
            ["rev-parse", "--path-format=absolute", "--absolute-git-dir"],
        )?
        .stdout,
    )?;
    let bare = parse_line(&git(path, ["rev-parse", "--is-bare-repository"])?.stdout)? == "true";
    Ok(Discovery { common, bare })
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn path_is_within(root: &Path, path: &Path) -> bool {
    let root = root
        .canonicalize()
        .unwrap_or_else(|_| planner::normalize_lexical(root.to_owned()));
    let path = path
        .canonicalize()
        .unwrap_or_else(|_| planner::normalize_lexical(path.to_owned()));
    path == root || path.starts_with(&root)
}

fn parse_line(bytes: &[u8]) -> Result<&str, GitError> {
    std::str::from_utf8(bytes)
        .map(|s| s.trim_end_matches(['\n', '\r']))
        .map_err(|_| GitError::Discovery("non-UTF-8 metadata".into()))
}

fn parse_path_line(bytes: &[u8]) -> Result<PathBuf, GitError> {
    let mut value = bytes.to_vec();
    while matches!(value.last(), Some(b'\n' | b'\r')) {
        value.pop();
    }
    if value.is_empty() {
        return Err(GitError::Discovery("empty path output".into()));
    }
    Ok(PathBuf::from(os_string(&value)))
}

fn warning(code: &str, message: &str, path: &Path) -> Warning {
    Warning {
        code: code.to_owned(),
        message: message.to_owned(),
        path: Some(path.to_owned()),
    }
}

struct WorktreeRecord {
    path: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    detached: bool,
    bare: bool,
    locked: Option<String>,
    prunable: Option<String>,
}

fn parse_worktrees(bytes: &[u8]) -> Result<Vec<WorktreeRecord>, GitError> {
    if bytes.len() < 2 || !bytes.ends_with(&[0, 0]) {
        return Err(GitError::Parse(
            "worktree output lacks final record separator".into(),
        ));
    }
    let fields: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    let mut result = Vec::new();
    let mut current: Option<WorktreeRecord> = None;
    for (index, field) in fields.iter().enumerate() {
        let is_final_empty = index == fields.len() - 1 && field.is_empty();
        if field.is_empty() {
            if is_final_empty {
                continue;
            }
            let record = current
                .take()
                .ok_or_else(|| GitError::Parse("empty worktree record".into()))?;
            validate_worktree(&record)?;
            result.push(record);
            continue;
        }
        let split = field.iter().position(|b| *b == b' ');
        let (key, value) = split.map_or((*field, &[][..]), |i| (&field[..i], &field[i + 1..]));
        if key == b"worktree" {
            if current.is_some() {
                return Err(GitError::Parse("worktree before record separator".into()));
            }
            if value.is_empty() {
                return Err(GitError::Parse("empty worktree path".into()));
            }
            current = Some(WorktreeRecord {
                path: PathBuf::from(os_string(value)),
                head: None,
                branch: None,
                detached: false,
                bare: false,
                locked: None,
                prunable: None,
            });
            continue;
        }
        let record = current
            .as_mut()
            .ok_or_else(|| GitError::Parse("record must begin with worktree".into()))?;
        match key {
            b"HEAD" => set_once(&mut record.head, utf8(value)?.to_owned(), "HEAD")?,
            b"branch" => set_once(&mut record.branch, utf8(value)?.to_owned(), "branch")?,
            b"detached" => {
                if record.detached {
                    return Err(GitError::Parse("duplicate detached".into()));
                }
                record.detached = true;
            }
            b"bare" => {
                if record.bare {
                    return Err(GitError::Parse("duplicate bare".into()));
                }
                record.bare = true;
            }
            b"locked" => set_once(&mut record.locked, utf8(value)?.to_owned(), "locked")?,
            b"prunable" => set_once(&mut record.prunable, utf8(value)?.to_owned(), "prunable")?,
            _ if key.iter().all(|b| b.is_ascii_lowercase() || *b == b'-') => {}
            _ => return Err(GitError::Parse("unknown worktree record".into())),
        }
    }
    if current.is_some() {
        return Err(GitError::Parse("missing worktree record separator".into()));
    }
    if result.is_empty() {
        return Err(GitError::Parse("no worktree records".into()));
    }
    Ok(result)
}

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> Result<(), GitError> {
    if slot.replace(value).is_some() {
        return Err(GitError::Parse(format!("duplicate {name}")));
    }
    Ok(())
}

fn validate_worktree(record: &WorktreeRecord) -> Result<(), GitError> {
    if record.bare && (record.branch.is_some() || record.detached || record.head.is_some()) {
        return Err(GitError::Parse(
            "bare worktree has branch, detached, or HEAD state".into(),
        ));
    }
    if record.branch.is_some() && record.detached {
        return Err(GitError::Parse("detached worktree has branch".into()));
    }
    Ok(())
}

fn apply_status(item: &mut Worktree, bytes: &[u8]) -> Result<(), GitError> {
    if bytes.is_empty() || *bytes.last().unwrap() != 0 {
        return Err(GitError::Parse("status output lacks final NUL".into()));
    }
    item.status = CheckoutStatus::Clean;
    let mut saw_head = false;
    let mut saw_branch = false;
    let mut saw_upstream = false;
    let mut saw_ab = false;
    let fields: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    let mut index = 0;
    while index + 1 < fields.len() {
        let field = fields[index];
        index += 1;
        if field.is_empty() {
            continue;
        }
        if field[0] == b'#' {
            let text = utf8(field)?;
            if let Some(v) = text.strip_prefix("# branch.oid ") {
                if saw_head {
                    return Err(GitError::Parse("duplicate branch.oid".into()));
                }
                saw_head = true;
                item.head_oid = (v != "(initial)").then(|| v.to_owned());
            } else if let Some(v) = text.strip_prefix("# branch.head ") {
                if saw_branch {
                    return Err(GitError::Parse("duplicate branch.head".into()));
                }
                saw_branch = true;
                item.detached = v == "(detached)";
                if !item.detached {
                    item.branch = Some(v.to_owned());
                }
            } else if let Some(v) = text.strip_prefix("# branch.upstream ") {
                if saw_upstream {
                    return Err(GitError::Parse("duplicate branch.upstream".into()));
                }
                saw_upstream = true;
                item.upstream = Some(v.to_owned());
            } else if let Some(v) = text.strip_prefix("# branch.ab ") {
                if saw_ab {
                    return Err(GitError::Parse("duplicate branch.ab".into()));
                }
                saw_ab = true;
                let mut parts = v.split_whitespace();
                item.ahead = Some(parse_count(parts.next(), "+")?);
                item.behind = Some(parse_count(parts.next(), "-")?);
                if parts.next().is_some() {
                    return Err(GitError::Parse("malformed branch.ab".into()));
                }
            }
        } else {
            match field[0] {
                b'1' => {
                    validate_data_prefix(field, 8)?;
                    item.status = CheckoutStatus::Dirty;
                }
                b'2' => {
                    validate_data_prefix(field, 9)?;
                    let original = fields.get(index).ok_or_else(|| {
                        GitError::Parse("rename record lacks original path".into())
                    })?;
                    if original.is_empty() {
                        return Err(GitError::Parse(
                            "rename record has empty original path".into(),
                        ));
                    }
                    index += 1;
                    item.status = CheckoutStatus::Dirty;
                }
                b'u' => {
                    validate_data_prefix(field, 10)?;
                    item.status = CheckoutStatus::Dirty;
                }
                b'?' => {
                    if field.len() < 3 || field[1] != b' ' {
                        return Err(GitError::Parse("malformed untracked record".into()));
                    }
                    item.status = CheckoutStatus::Dirty;
                }
                b'!' => {
                    if field.len() < 3 || field[1] != b' ' {
                        return Err(GitError::Parse("malformed ignored record".into()));
                    }
                }
                _ => return Err(GitError::Parse("unknown or malformed status record".into())),
            }
        }
    }
    Ok(())
}

fn validate_data_prefix(field: &[u8], spaces: usize) -> Result<(), GitError> {
    let mut seen = 0;
    for (index, byte) in field.iter().enumerate() {
        if *byte == b' ' {
            seen += 1;
            if seen == spaces {
                if index + 1 == field.len() {
                    return Err(GitError::Parse("status record lacks pathname".into()));
                }
                std::str::from_utf8(&field[..index])
                    .map_err(|_| GitError::Parse("non-UTF-8 status metadata".into()))?;
                return Ok(());
            }
        }
    }
    Err(GitError::Parse("malformed status metadata".into()))
}

fn parse_count(value: Option<&str>, sign: &str) -> Result<u32, GitError> {
    let value = value.ok_or_else(|| GitError::Parse("malformed branch.ab".into()))?;
    value
        .strip_prefix(sign)
        .ok_or_else(|| GitError::Parse("malformed branch.ab".into()))?
        .parse()
        .map_err(|_| GitError::Parse("malformed branch.ab".into()))
}

fn utf8(value: &[u8]) -> Result<&str, GitError> {
    std::str::from_utf8(value).map_err(|_| GitError::Parse("non-UTF-8 metadata".into()))
}

#[cfg(unix)]
fn os_string(value: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(value.to_vec())
}
#[cfg(not(unix))]
fn os_string(value: &[u8]) -> std::ffi::OsString {
    std::ffi::OsString::from(String::from_utf8_lossy(value).into_owned())
}

struct Output {
    stdout: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ObservedNode {
    Regular { bytes: Vec<u8>, mode: u32 },
    Directory,
    Symlink { target: PathBuf },
}

fn exact_source(
    node: &ObservedNode,
    expected: &crate::lifecycle::ArtifactSourceExpectationV3,
) -> bool {
    match (node, expected) {
        (ObservedNode::Directory, crate::lifecycle::ArtifactSourceExpectationV3::Directory) => true,
        (
            ObservedNode::Regular { bytes, mode },
            crate::lifecycle::ArtifactSourceExpectationV3::Regular(want),
        ) => {
            bytes.len() as u64 == want.bytes
                && crate::planner::artifact_digest(bytes) == want.digest
                && *mode == want.mode
        }
        _ => false,
    }
}

pub(crate) fn source_expectation_matches(
    node: &ObservedNode,
    expected: &crate::lifecycle::ArtifactSourceExpectationV3,
) -> bool {
    exact_source(node, expected)
}

#[cfg(unix)]
pub(crate) fn readonly_observe_node(
    trusted_root: &Path,
    path: &Path,
) -> Result<Option<ObservedNode>, GitError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, open, openat, readlinkat, statat};
    use std::{fs::File, io::Read};

    let planned_root = planner::normalize_lexical(trusted_root.to_owned());
    let path = planner::normalize_lexical(path.to_owned());
    let relative = path
        .strip_prefix(&planned_root)
        .map_err(|_| GitError::Parse("path is outside trusted root".into()))?;
    let trusted_root = planned_root
        .canonicalize()
        .map_err(|error| GitError::Discovery(error.to_string()))?;
    let mut dirfd = open(
        &trusted_root,
        OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| GitError::Command(error.to_string()))?;
    let components: Vec<_> = relative.components().collect();
    if components.is_empty() {
        return Ok(Some(ObservedNode::Directory));
    }
    for component in components.iter().take(components.len() - 1) {
        let std::path::Component::Normal(name) = component else {
            return Ok(None);
        };
        dirfd = match openat(
            &dirfd,
            *name,
            OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(value) => value,
            Err(error) if observation_mismatch(error) => return Ok(None),
            Err(error) => return Err(GitError::Command(error.to_string())),
        };
    }
    let std::path::Component::Normal(name) = components[components.len() - 1] else {
        return Ok(None);
    };
    let stat = match statat(&dirfd, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(value) => value,
        Err(error) if observation_mismatch(error) => return Ok(None),
        Err(error) => return Err(GitError::Command(error.to_string())),
    };
    let file_type = FileType::from_raw_mode(stat.st_mode);
    if file_type.is_symlink() {
        let target = match readlinkat(&dirfd, name, Vec::new()) {
            Ok(value) => value.into_bytes(),
            Err(error) if observation_mismatch(error) => return Ok(None),
            Err(error) => return Err(GitError::Command(error.to_string())),
        };
        return Ok(Some(ObservedNode::Symlink {
            target: PathBuf::from(std::ffi::OsString::from_vec(target)),
        }));
    }
    observation_fifo_swap_hook(&path, &dirfd, name)?;
    let final_fd = match openat(
        &dirfd,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(value) => value,
        Err(error) if observation_mismatch(error) => return Ok(None),
        Err(error) => return Err(GitError::Command(error.to_string())),
    };
    let final_stat = fstat(&final_fd).map_err(|error| GitError::Command(error.to_string()))?;
    let final_type = FileType::from_raw_mode(final_stat.st_mode);
    if final_type.is_dir() {
        return Ok(Some(ObservedNode::Directory));
    }
    if !final_type.is_file() {
        return Ok(None);
    }
    let mut file = File::from(final_fd);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| GitError::Command(error.to_string()))?;
    #[cfg(target_os = "macos")]
    let mode = u32::from(final_stat.st_mode & 0o7777);
    #[cfg(not(target_os = "macos"))]
    let mode = final_stat.st_mode & 0o7777;
    Ok(Some(ObservedNode::Regular { bytes, mode }))
}

#[cfg(all(unix, test))]
struct ObservationFifoSwapState {
    armed_path: Option<PathBuf>,
    invocations: usize,
}

#[cfg(all(unix, test))]
thread_local! {
    static OBSERVATION_FIFO_SWAP: std::cell::RefCell<ObservationFifoSwapState> = const {
        std::cell::RefCell::new(ObservationFifoSwapState { armed_path: None, invocations: 0 })
    };
}

#[cfg(all(unix, test))]
#[allow(dead_code)]
pub(crate) struct ObservationFifoSwapGuard;

#[cfg(all(unix, test))]
#[allow(dead_code)]
pub(crate) fn arm_observation_fifo_swap(path: &Path) -> ObservationFifoSwapGuard {
    OBSERVATION_FIFO_SWAP.with(|state| {
        let mut state = state.borrow_mut();
        state.armed_path = Some(path.to_owned());
        state.invocations = 0;
    });
    ObservationFifoSwapGuard
}

#[cfg(all(unix, test))]
#[allow(dead_code)]
pub(crate) fn observation_fifo_swap_invocation_count() -> usize {
    OBSERVATION_FIFO_SWAP.with(|state| state.borrow().invocations)
}

#[cfg(all(unix, test))]
impl Drop for ObservationFifoSwapGuard {
    fn drop(&mut self) {
        OBSERVATION_FIFO_SWAP.with(|state| {
            let mut state = state.borrow_mut();
            state.armed_path = None;
            state.invocations = 0;
        });
    }
}

#[cfg(all(unix, test))]
fn observation_fifo_swap_hook(
    path: &Path,
    dirfd: &rustix::fd::OwnedFd,
    name: &OsStr,
) -> Result<(), GitError> {
    use rustix::fs::{AtFlags, renameat, unlinkat};
    OBSERVATION_FIFO_SWAP.with(|state| {
        let mut state = state.borrow_mut();
        if state.armed_path.as_deref() != Some(path) {
            return Ok(());
        }
        state.invocations += 1;
        state.armed_path = None;
        let temporary = OsString::from(".ewtm-observation-original");
        renameat(dirfd, name, dirfd, &temporary)
            .map_err(|error| GitError::Command(error.to_string()))?;
        let status = Command::new("mkfifo")
            .arg(path)
            .status()
            .map_err(|error| GitError::Command(error.to_string()))?;
        if !status.success() {
            return Err(GitError::Command("mkfifo test hook failed".into()));
        }
        unlinkat(dirfd, &temporary, AtFlags::empty())
            .map_err(|error| GitError::Command(error.to_string()))?;
        Ok(())
    })
}

#[cfg(all(unix, not(test)))]
fn observation_fifo_swap_hook(
    _path: &Path,
    _dirfd: &rustix::fd::OwnedFd,
    _name: &OsStr,
) -> Result<(), GitError> {
    Ok(())
}

#[cfg(unix)]
fn observation_mismatch(error: rustix::io::Errno) -> bool {
    matches!(
        error,
        rustix::io::Errno::NOENT | rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR
    )
}

#[cfg(not(unix))]
pub(crate) fn readonly_observe_node(
    _trusted_root: &Path,
    _path: &Path,
) -> Result<Option<ObservedNode>, GitError> {
    Err(GitError::Parse(
        "descriptor-relative observations unsupported on this platform".into(),
    ))
}

/// The small read-only surface used by the execution backend.  Keeping the
/// command runner here ensures execution never gets access to mutation
/// plumbing.
pub(crate) fn readonly_ref_oid(
    cwd: &Path,
    reference: &str,
) -> Result<Option<crate::lifecycle::ObjectId>, GitError> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ])
        .output()
        .map_err(|e| GitError::Command(e.to_string()))?;
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).trim().into(),
        ));
    }
    let value = parse_line(&output.stdout)?;
    crate::lifecycle::ObjectId::new(value.trim().to_owned())
        .map(Some)
        .map_err(GitError::Parse)
}

pub(crate) fn readonly_ancestor(
    cwd: &Path,
    ancestor: &crate::lifecycle::ObjectId,
    descendant: &crate::lifecycle::ObjectId,
) -> Result<bool, GitError> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args([
            "merge-base",
            "--is-ancestor",
            ancestor.as_str(),
            descendant.as_str(),
        ])
        .output()
        .map_err(|e| GitError::Command(e.to_string()))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).trim().into(),
        )),
    }
}

pub(crate) fn readonly_branch_upstream(
    cwd: &Path,
    branch: &str,
) -> Result<Option<String>, GitError> {
    let output = git(
        cwd,
        [
            "for-each-ref",
            "--format=%(upstream)",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    let value = parse_line(&output.stdout)?;
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

pub(crate) fn readonly_remote_ref(
    cwd: &Path,
    remote: &str,
    branch: &str,
) -> Result<Option<crate::lifecycle::ObjectId>, GitError> {
    readonly_validate_remote(cwd, remote)?;
    let reference = format!("refs/heads/{branch}");
    let output = git(cwd, ["ls-remote", "--refs", remote, &reference])?;
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| GitError::Parse("non-UTF-8 remote ref".into()))?;
    let lines: Vec<_> = text.lines().filter(|line| !line.is_empty()).collect();
    if lines.is_empty() {
        return Ok(None);
    }
    if lines.len() != 1 {
        return Err(GitError::Parse("duplicate remote ref".into()));
    }
    let (oid, name) = lines[0]
        .split_once('\t')
        .ok_or_else(|| GitError::Parse("malformed remote ref".into()))?;
    if name != reference {
        return Err(GitError::Parse("mismatched remote ref".into()));
    }
    crate::lifecycle::ObjectId::new(oid.to_owned())
        .map(Some)
        .map_err(GitError::Parse)
}

pub(crate) fn readonly_remote_default(cwd: &Path, remote: &str) -> Result<String, GitError> {
    readonly_validate_remote(cwd, remote)?;
    let output = git(cwd, ["ls-remote", "--symref", remote, "HEAD"])?;
    let mut found = None;
    for line in String::from_utf8(output.stdout)
        .map_err(|_| GitError::Parse("non-UTF-8 remote HEAD".into()))?
        .lines()
    {
        if let Some(value) = line.strip_prefix("ref: ") {
            let (reference, name) = value
                .split_once('\t')
                .ok_or_else(|| GitError::Parse("malformed remote HEAD".into()))?;
            if name != "HEAD" {
                return Err(GitError::Parse("malformed remote HEAD".into()));
            }
            if found.replace(reference.to_owned()).is_some() {
                return Err(GitError::Parse("duplicate remote HEAD".into()));
            }
        }
    }
    found.ok_or_else(|| GitError::Parse("remote HEAD has no symbolic reference".into()))
}

fn readonly_validate_remote(cwd: &Path, remote: &str) -> Result<(), GitError> {
    if remote.is_empty()
        || remote.starts_with('-')
        || remote.contains("://")
        || !remote
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        || remote.starts_with('.')
        || remote.ends_with('.')
    {
        return Err(GitError::Parse("invalid remote identity".into()));
    }
    let key = format!("remote.{remote}.url");
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["config", "--get", &key])
        .output()
        .map_err(|error| GitError::Command(error.to_string()))?;
    if output.status.code() == Some(1) {
        return Err(GitError::Parse("remote is not configured".into()));
    }
    if !output.status.success() {
        return Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).trim().into(),
        ));
    }
    Ok(())
}

pub(crate) fn readonly_list(path: &Path) -> Result<ListResult, GitError> {
    let result = GitCli.list(path)?;
    if let Some(warning) = result
        .warnings
        .iter()
        .find(|warning| warning.code != "worktree_prunable")
    {
        return Err(GitError::Parse(format!(
            "incomplete repository observation: {}",
            warning.code
        )));
    }
    Ok(result)
}

pub(crate) fn readonly_safe_directory(path: &Path) -> Result<bool, GitError> {
    Ok(matches!(
        readonly_observe_absolute_node(path)?,
        Some(ObservedNode::Directory)
    ))
}

pub(crate) fn readonly_safe_parent_of(path: &Path) -> Result<bool, GitError> {
    let normalized = planner::normalize_lexical(path.to_owned());
    let parent = normalized
        .parent()
        .ok_or_else(|| GitError::Parse("path has no parent".into()))?;
    readonly_safe_directory(parent)
}

pub(crate) fn readonly_observe_absolute_node(
    path: &Path,
) -> Result<Option<ObservedNode>, GitError> {
    let path = planner::normalize_lexical(path.to_owned());
    if !path.is_absolute() {
        return Err(GitError::Parse(
            "absolute observation requires an absolute path".into(),
        ));
    }
    let root = platform_alias_root(&path);
    readonly_observe_node(&root, &path)
}

#[cfg(unix)]
pub(crate) fn readonly_final_absent(path: &Path) -> Result<bool, GitError> {
    use rustix::fs::{AtFlags, Mode, OFlags, open, openat, statat};

    let path = planner::normalize_lexical(path.to_owned());
    if !path.is_absolute() {
        return Err(GitError::Parse(
            "absolute observation requires an absolute path".into(),
        ));
    }
    let planned_root = platform_alias_root(&path);
    let relative = path
        .strip_prefix(&planned_root)
        .map_err(|_| GitError::Parse("path is outside trusted root".into()))?;
    let components: Vec<_> = relative.components().collect();
    let Some(std::path::Component::Normal(final_name)) = components.last() else {
        return Ok(false);
    };
    let final_name = *final_name;
    let canonical_root = planned_root
        .canonicalize()
        .map_err(|error| GitError::Discovery(error.to_string()))?;
    let mut parent = open(
        &canonical_root,
        OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| GitError::Command(error.to_string()))?;
    for component in components.iter().take(components.len() - 1) {
        let std::path::Component::Normal(name) = component else {
            return Ok(false);
        };
        parent = match openat(
            &parent,
            *name,
            OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(value) => value,
            Err(error) if observation_mismatch(error) => return Ok(false),
            Err(error) => return Err(GitError::Command(error.to_string())),
        };
    }
    match statat(&parent, final_name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(false),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(true),
        Err(error) if observation_mismatch(error) => Ok(false),
        Err(error) => Err(GitError::Command(error.to_string())),
    }
}

#[cfg(not(unix))]
pub(crate) fn readonly_final_absent(_path: &Path) -> Result<bool, GitError> {
    Err(GitError::Parse(
        "descriptor-relative observations unsupported on this platform".into(),
    ))
}

#[cfg(unix)]
fn validate_absolute_path(path: &Path) -> Result<PathBuf, GitError> {
    let normalized = planner::normalize_lexical(path.to_owned());
    if !normalized.is_absolute()
        || normalized != path
        || !normalized.components().all(|component| {
            !matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(GitError::Parse(
            "mutation path is not normalized absolute".into(),
        ));
    }
    Ok(normalized)
}

#[cfg(unix)]
fn validate_mutation_path(path: &Path) -> Result<PathBuf, GitError> {
    let normalized = validate_absolute_path(path)?;
    if !matches!(normalized.file_name(), Some(name) if !name.is_empty()) {
        return Err(GitError::Parse("mutation path has no final name".into()));
    }
    Ok(normalized)
}

#[cfg(unix)]
fn mutation_parent(path: &Path) -> Result<(rustix::fd::OwnedFd, OsString), GitError> {
    use rustix::fs::{Mode, OFlags, open, openat};
    let path = validate_mutation_path(path)?;
    let name = path
        .file_name()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| GitError::Parse("mutation path has no final name".into()))?
        .to_owned();
    let parent = path
        .parent()
        .ok_or_else(|| GitError::Parse("mutation path has no parent".into()))?;
    let trusted_root = platform_alias_root(&path);
    let canonical_root = trusted_root
        .canonicalize()
        .map_err(|error| GitError::Discovery(error.to_string()))?;
    let relative_parent = parent
        .strip_prefix(&trusted_root)
        .map_err(|_| GitError::Parse("mutation path is outside trusted root".into()))?;
    let mut fd = open(
        &canonical_root,
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| GitError::Command(e.to_string()))?;
    for component in relative_parent.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(GitError::Parse("mutation parent is not normalized".into()));
        };
        fd = openat(
            &fd,
            part,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| GitError::Command(e.to_string()))?;
    }
    Ok((fd, name))
}

#[cfg(unix)]
fn mutation_parents(
    destination: &Path,
    staging: &Path,
) -> Result<(rustix::fd::OwnedFd, OsString, OsString), GitError> {
    let destination = validate_mutation_path(destination)?;
    let staging = validate_mutation_path(staging)?;
    let destination_parent = destination
        .parent()
        .ok_or_else(|| GitError::Parse("destination has no parent".into()))?;
    let staging_parent = staging
        .parent()
        .ok_or_else(|| GitError::Parse("staging has no parent".into()))?;
    if destination_parent != staging_parent {
        return Err(GitError::Parse(
            "staging and destination parents differ".into(),
        ));
    }
    let destination_name = destination.file_name().unwrap().to_owned();
    let staging_name = staging.file_name().unwrap().to_owned();
    if destination_name == staging_name {
        return Err(GitError::Parse(
            "staging and destination names must differ".into(),
        ));
    }
    let (parent, _) = mutation_parent(&destination)?;
    Ok((parent, destination_name, staging_name))
}

#[cfg(unix)]
fn mutation_source(path: &Path) -> Result<(rustix::fd::OwnedFd, ObservedNode), GitError> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, openat};
    let (parent, name) = mutation_parent(path)?;
    let fd = openat(
        &parent,
        &name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|e| GitError::Command(e.to_string()))?;
    let st = fstat(&fd).map_err(|e| GitError::Command(e.to_string()))?;
    let ty = FileType::from_raw_mode(st.st_mode);
    if ty.is_dir() {
        return Ok((fd, ObservedNode::Directory));
    }
    if !ty.is_file() {
        return Err(GitError::Command(
            "source is not a regular file or directory".into(),
        ));
    }
    let mut file = std::fs::File::from(fd);
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)
        .map_err(|e| GitError::Command(e.to_string()))?;
    #[cfg(target_os = "macos")]
    let mode = u32::from(st.st_mode & 0o7777);
    #[cfg(not(target_os = "macos"))]
    let mode = st.st_mode & 0o7777;
    let fd = file.into();
    Ok((fd, ObservedNode::Regular { bytes, mode }))
}

#[cfg(all(unix, test))]
fn artifact_fault(point: ArtifactFaultPoint) -> Result<(), GitError> {
    ARTIFACT_FAULT.with(|state| {
        let mut state = state.borrow_mut();
        state.counts[point as usize] += 1;
        if state.armed == Some(point) {
            state.armed = None;
            return Err(GitError::Command("injected artifact fault".into()));
        }
        Ok(())
    })
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArtifactFaultPoint {
    StagingCreate,
    Write,
    Fchmod,
    FileFsync,
    Rename,
    CopyParentFsync,
    SymlinkCreate,
    SymlinkParentFsync,
}

#[cfg(all(unix, test))]
struct ArtifactFaultState {
    armed: Option<ArtifactFaultPoint>,
    counts: [usize; 8],
}
#[cfg(all(unix, test))]
thread_local! {
    static ARTIFACT_FAULT: std::cell::RefCell<ArtifactFaultState> = const {
        std::cell::RefCell::new(ArtifactFaultState { armed: None, counts: [0; 8] })
    };
}
#[cfg(all(unix, test))]
#[allow(dead_code)]
pub(crate) struct ArtifactFaultGuard;
#[cfg(all(unix, test))]
#[allow(dead_code)]
pub(crate) fn arm_artifact_fault(point: ArtifactFaultPoint) -> ArtifactFaultGuard {
    ARTIFACT_FAULT.with(|state| {
        let mut state = state.borrow_mut();
        state.armed = Some(point);
        state.counts = [0; 8];
    });
    ArtifactFaultGuard
}
#[cfg(all(unix, test))]
#[allow(dead_code)]
pub(crate) fn artifact_fault_invocation_count(point: ArtifactFaultPoint) -> usize {
    ARTIFACT_FAULT.with(|state| state.borrow().counts[point as usize])
}
#[cfg(all(unix, test))]
impl Drop for ArtifactFaultGuard {
    fn drop(&mut self) {
        ARTIFACT_FAULT.with(|state| {
            let mut state = state.borrow_mut();
            state.armed = None;
            state.counts = [0; 8];
        });
    }
}
#[cfg(all(unix, not(test)))]
fn artifact_fault(_point: ArtifactFaultPoint) -> Result<(), GitError> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn mutate_copy_file_v3(
    source_root: &Path,
    source: &Path,
    expected_source: &crate::lifecycle::RegularFileStateV3,
    destination: &Path,
    desired: &crate::lifecycle::RegularFileStateV3,
    staging: &crate::lifecycle::OwnedStagingV3,
) -> Result<(), GitError> {
    use rustix::fs::{
        AtFlags, Mode, OFlags, RenameFlags, fchmod, fsync, openat, renameat_with, statat,
    };
    let source_root = validate_absolute_path(source_root)?;
    let source = validate_mutation_path(source)?;
    let staging_path = validate_mutation_path(staging.path.as_path())?;
    validate_mutation_path(destination)?;
    if source == source_root || !source.starts_with(&source_root) {
        return Err(GitError::Parse("source is outside source root".into()));
    }
    if expected_source.mode > 0o7777
        || desired.mode > 0o7777
        || desired.bytes != expected_source.bytes
        || desired.digest != expected_source.digest
    {
        return Err(GitError::Parse(
            "copy source/output contract is invalid".into(),
        ));
    }
    let (_source_fd, observed) = mutation_source(&source)?;
    if !exact_source(
        &observed,
        &crate::lifecycle::ArtifactSourceExpectationV3::Regular(expected_source.clone()),
    ) {
        return Err(GitError::Command("source changed since planning".into()));
    }
    let data = match observed {
        ObservedNode::Regular { bytes, .. } => bytes,
        _ => unreachable!(),
    };
    let (parent, final_name, stage_name) = mutation_parents(destination, &staging_path)?;
    match statat(&parent, &final_name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => return Err(GitError::Command("destination already exists".into())),
        Err(error) if error == rustix::io::Errno::NOENT => {}
        Err(error) => return Err(GitError::Command(error.to_string())),
    }
    let stage = openat(
        &parent,
        &stage_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|e| GitError::Command(e.to_string()))?;
    artifact_fault(ArtifactFaultPoint::StagingCreate)?;
    let mut file = std::fs::File::from(stage);
    std::io::Write::write_all(&mut file, &data).map_err(|e| GitError::Command(e.to_string()))?;
    artifact_fault(ArtifactFaultPoint::Write)?;
    #[cfg(target_os = "macos")]
    let desired_mode = u16::try_from(desired.mode)
        .map_err(|_| GitError::Parse("copy output mode is out of range".into()))?;
    #[cfg(not(target_os = "macos"))]
    let desired_mode = desired.mode;
    fchmod(&file, Mode::from_raw_mode(desired_mode))
        .map_err(|e| GitError::Command(e.to_string()))?;
    artifact_fault(ArtifactFaultPoint::Fchmod)?;
    fsync(&file).map_err(|e| GitError::Command(e.to_string()))?;
    artifact_fault(ArtifactFaultPoint::FileFsync)?;
    drop(file);
    renameat_with(
        &parent,
        &stage_name,
        &parent,
        &final_name,
        RenameFlags::NOREPLACE,
    )
    .map_err(|e| GitError::Command(e.to_string()))?;
    artifact_fault(ArtifactFaultPoint::Rename)?;
    fsync(&parent).map_err(|e| GitError::Command(e.to_string()))?;
    artifact_fault(ArtifactFaultPoint::CopyParentFsync)
}

#[cfg(not(unix))]
pub(crate) fn mutate_copy_file_v3(
    _: &Path,
    _: &Path,
    _: &crate::lifecycle::RegularFileStateV3,
    _: &Path,
    _: &crate::lifecycle::RegularFileStateV3,
    _: &crate::lifecycle::OwnedStagingV3,
) -> Result<(), GitError> {
    Err(GitError::Parse(
        "descriptor-relative mutation unsupported on this platform".into(),
    ))
}

#[cfg(unix)]
pub(crate) fn mutate_create_symlink_v3(
    source_root: &Path,
    source: &Path,
    expected: &crate::lifecycle::ArtifactSourceExpectationV3,
    destination: &Path,
    desired: &crate::lifecycle::SymlinkStateV3,
) -> Result<(), GitError> {
    use rustix::fs::{fsync, symlinkat};
    let root = validate_absolute_path(source_root)?;
    let source = validate_mutation_path(source)?;
    validate_mutation_path(destination)?;
    if source == root || !source.starts_with(&root) {
        return Err(GitError::Parse("source is outside source root".into()));
    }
    match expected {
        crate::lifecycle::ArtifactSourceExpectationV3::Regular(state) if state.mode <= 0o7777 => {}
        crate::lifecycle::ArtifactSourceExpectationV3::Directory => {}
        _ => return Err(GitError::Parse("symlink source contract is invalid".into())),
    }
    if desired.target.as_path() != source
        || crate::planner::artifact_digest(desired.target.as_path().as_os_str().as_encoded_bytes())
            != desired.target_digest
    {
        return Err(GitError::Parse("symlink target contract is invalid".into()));
    }
    let (_fd, observed) = mutation_source(&source)?;
    if !exact_source(&observed, expected) {
        return Err(GitError::Command("source changed since planning".into()));
    }
    let (parent, name) = mutation_parent(destination)?;
    symlinkat(desired.target.as_path(), &parent, &name)
        .map_err(|e| GitError::Command(e.to_string()))?;
    artifact_fault(ArtifactFaultPoint::SymlinkCreate)?;
    fsync(&parent).map_err(|e| GitError::Command(e.to_string()))?;
    artifact_fault(ArtifactFaultPoint::SymlinkParentFsync)
}

#[cfg(not(unix))]
pub(crate) fn mutate_create_symlink_v3(
    _: &Path,
    _: &Path,
    _: &crate::lifecycle::ArtifactSourceExpectationV3,
    _: &Path,
    _: &crate::lifecycle::SymlinkStateV3,
) -> Result<(), GitError> {
    Err(GitError::Parse(
        "descriptor-relative mutation unsupported on this platform".into(),
    ))
}

fn platform_alias_root(path: &Path) -> PathBuf {
    let Some(first) = path.components().find_map(|component| match component {
        std::path::Component::Normal(name) => Some(name),
        _ => None,
    }) else {
        return PathBuf::from("/");
    };
    let prefix = Path::new("/").join(first);
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::symlink_metadata(&prefix)
        && metadata.file_type().is_symlink()
        && let Ok(target) = prefix.canonicalize()
        && target.is_dir()
    {
        return prefix;
    }
    PathBuf::from("/")
}

pub(crate) fn readonly_same_path(left: &Path, right: &Path) -> bool {
    same_path(left, right)
}

pub(crate) fn readonly_normalize(path: PathBuf) -> PathBuf {
    planner::normalize_lexical(path)
}

pub(crate) fn readonly_ongoing(path: &Path) -> Result<bool, GitError> {
    ongoing_git_operation(path).map_err(|error| GitError::Command(error.message))
}

pub(crate) fn readonly_ref_is_symbolic(cwd: &Path, reference: &str) -> Result<bool, GitError> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["symbolic-ref", "--quiet", reference])
        .output()
        .map_err(|error| GitError::Command(error.to_string()))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).trim().into(),
        )),
    }
}

fn mutation_git(cwd: &Path, args: Vec<OsString>) -> Result<(), GitError> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|error| GitError::Command(error.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).trim().into(),
        ));
    }
    #[cfg(test)]
    if let Some(error) = test_mutation_success_error() {
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
struct MutationFaultState {
    remaining: bool,
    invocations: usize,
}

#[cfg(test)]
thread_local! {
    static MUTATION_FAULT: std::cell::RefCell<MutationFaultState> =
        const { std::cell::RefCell::new(MutationFaultState { remaining: false, invocations: 0 }) };
}

#[cfg(test)]
pub(crate) struct MutationFaultGuard;

#[cfg(test)]
pub(crate) fn arm_mutation_success_error() -> MutationFaultGuard {
    MUTATION_FAULT.with(|state| {
        let mut state = state.borrow_mut();
        state.remaining = true;
        state.invocations = 0;
    });
    MutationFaultGuard
}

#[cfg(test)]
pub(crate) fn mutation_invocation_count() -> usize {
    MUTATION_FAULT.with(|state| state.borrow().invocations)
}

#[cfg(test)]
fn test_mutation_success_error() -> Option<GitError> {
    MUTATION_FAULT.with(|state| {
        let mut state = state.borrow_mut();
        state.invocations += 1;
        state.remaining.then(|| {
            state.remaining = false;
            GitError::Command("injected post-success mutation error".into())
        })
    })
}

#[cfg(test)]
impl Drop for MutationFaultGuard {
    fn drop(&mut self) {
        MUTATION_FAULT.with(|state| {
            let mut state = state.borrow_mut();
            state.remaining = false;
            state.invocations = 0;
        });
    }
}

pub(crate) fn mutate_create_worktree(
    cwd: &Path,
    destination: &Path,
    source: &crate::lifecycle::CreateSource,
    source_oid: &crate::lifecycle::ObjectId,
) -> Result<(), GitError> {
    let destination = destination.as_os_str().to_owned();
    let args = match source {
        crate::lifecycle::CreateSource::NewBranch { branch, base: _ } => {
            let args = vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--no-track"),
                OsString::from("-b"),
                OsString::from(branch.as_str()),
                destination,
                OsString::from(source_oid.as_str()),
            ];
            args
        }
        crate::lifecycle::CreateSource::ExistingLocal { branch } => {
            let reference = format!("refs/heads/{branch}");
            if readonly_ref_oid(cwd, &reference)? != Some(source_oid.clone()) {
                return Err(GitError::Command(
                    "existing local branch OID changed".into(),
                ));
            }
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                destination,
                OsString::from(reference),
            ]
        }
        crate::lifecycle::CreateSource::RemoteTracking {
            remote,
            remote_branch,
            local_branch,
        } => {
            readonly_validate_remote(cwd, remote.as_str())?;
            let reference = format!("refs/remotes/{remote}/{remote_branch}");
            if readonly_ref_oid(cwd, &reference)? != Some(source_oid.clone()) {
                return Err(GitError::Command("remote-tracking ref OID changed".into()));
            }
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--track"),
                OsString::from("-b"),
                OsString::from(local_branch.as_str()),
                destination,
                OsString::from(reference),
            ]
        }
    };
    mutation_git(cwd, args)
}

pub(crate) fn mutate_remove_worktree(cwd: &Path, path: &Path) -> Result<(), GitError> {
    mutation_git(
        cwd,
        vec![
            OsString::from("worktree"),
            OsString::from("remove"),
            path.as_os_str().to_owned(),
        ],
    )
}

pub(crate) fn mutate_delete_local_branch(
    cwd: &Path,
    branch: &str,
    expected: &crate::lifecycle::ObjectId,
) -> Result<(), GitError> {
    if expected.as_str().chars().all(|value| value == '0') {
        return Err(GitError::Parse(
            "zero object id is not a deletion lease".into(),
        ));
    }
    let reference = format!("refs/heads/{branch}");
    if readonly_ref_is_symbolic(cwd, &reference)? {
        return Err(GitError::Parse(
            "symbolic local branch ref cannot be deleted".into(),
        ));
    }
    mutation_git(
        cwd,
        vec![
            OsString::from("update-ref"),
            OsString::from("--no-deref"),
            OsString::from("-d"),
            OsString::from(reference),
            OsString::from(expected.as_str()),
        ],
    )
}

pub(crate) fn mutate_delete_remote_branch(
    cwd: &Path,
    remote: &str,
    branch: &str,
    expected: &crate::lifecycle::ObjectId,
) -> Result<(), GitError> {
    if expected.as_str().chars().all(|value| value == '0') {
        return Err(GitError::Parse(
            "zero object id is not a deletion lease".into(),
        ));
    }
    readonly_validate_remote(cwd, remote)?;
    mutation_git(
        cwd,
        vec![
            OsString::from("push"),
            OsString::from(format!(
                "--force-with-lease=refs/heads/{branch}:{}",
                expected.as_str()
            )),
            OsString::from(remote),
            OsString::from(format!(":refs/heads/{branch}")),
        ],
    )
}

fn git<I, A>(cwd: &Path, args: I) -> Result<Output, GitError>
where
    I: IntoIterator<Item = A>,
    A: AsRef<OsStr>,
{
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|e| GitError::Command(e.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(Output {
        stdout: output.stdout,
    })
}

pub fn unicode_slug(input: &str, cap: usize, cwd: &Path) -> Result<String, GitError> {
    if input.starts_with("@{-") && input.ends_with('}') {
        return Err(GitError::Parse("branch shorthand is not allowed".into()));
    }
    let normalized: String = input.nfc().collect();
    let mut out = String::new();
    for component in normalized.split('/') {
        let mut part = String::new();
        for ch in component.chars() {
            if ch.is_alphanumeric() {
                part.extend(ch.to_lowercase());
            } else if !part.ends_with('-') && !part.is_empty() {
                part.push('-');
            }
        }
        let part = part.trim_matches('-');
        if !part.is_empty() {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(part);
        }
    }
    let value = truncate_slug(&out, cap);
    if value.is_empty() || value == "@{-1}" {
        return Err(GitError::Parse("empty or shorthand branch slug".into()));
    }
    let check = Command::new("git")
        .current_dir(cwd)
        .args(["check-ref-format", "--branch", &value])
        .output()
        .map_err(|e| GitError::Command(e.to_string()))?;
    if !check.status.success() {
        return Err(GitError::Parse("invalid branch slug".into()));
    }
    Ok(value)
}

pub fn collision_candidate(base: &str, n: u32, cap: usize, cwd: &Path) -> Result<String, GitError> {
    if cap < 8 || base.trim().is_empty() {
        return Err(GitError::Parse(
            "slug cap is too small or base is empty".into(),
        ));
    }
    let suffix = format!("-{n}");
    if suffix.len() >= cap {
        return Err(GitError::Parse(
            "slug cap cannot fit collision suffix".into(),
        ));
    }
    let prefix = truncate_slug(base, cap - suffix.len());
    let value = truncate_slug(&format!("{prefix}{suffix}"), cap);
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["check-ref-format", "--branch", &value])
        .output()
        .map_err(|error| GitError::Command(error.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Parse("invalid collision branch candidate".into()));
    }
    Ok(value)
}

fn truncate_slug(value: &str, cap: usize) -> String {
    let mut result = value.to_owned();
    while result.len() > cap {
        result.pop();
    }
    result = result.trim_matches(['-', '/']).to_owned();
    result
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| part.trim_matches('-'))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn destination_facts_are_read_only_and_distinguish_absent_present() {
        let temp = TempDir::new().unwrap();
        let parent = temp.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let absent = destination_facts(parent.join("absent")).unwrap();
        assert_eq!(absent.state, DestinationState::Absent);
        std::fs::write(parent.join("present"), b"x").unwrap();
        let present = destination_facts(parent.join("present")).unwrap();
        assert_eq!(present.state, DestinationState::Present);
    }

    #[cfg(unix)]
    #[test]
    fn destination_facts_detect_dangling_symlink_and_unsafe_parent() {
        let temp = TempDir::new().unwrap();
        let parent = temp.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        std::os::unix::fs::symlink("missing", parent.join("dangling")).unwrap();
        assert_eq!(
            destination_facts(parent.join("dangling")).unwrap().state,
            DestinationState::Dangling
        );
        std::os::unix::fs::symlink(&parent, temp.path().join("link-parent")).unwrap();
        assert!(
            !destination_facts(temp.path().join("link-parent/file"))
                .unwrap()
                .parent_safe
        );
    }

    #[test]
    fn create_facts_are_read_only_and_resolve_explicit_base() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        run_git(temp.path(), ["init", repo.to_str().unwrap()]);
        run_git(&repo, ["config", "user.name", "facts-test"]);
        run_git(&repo, ["config", "user.email", "facts@example.invalid"]);
        std::fs::write(repo.join("tracked"), b"value").unwrap();
        run_git(&repo, ["add", "tracked"]);
        run_git(&repo, ["commit", "-m", "initial"]);
        let request = crate::application::CreatePlanRequest {
            repo: repo.clone(),
            invocation_cwd: temp.path().to_owned(),
            source: crate::application::CreateSourceRequest::New {
                branch: "feature/facts".into(),
                base: Some("HEAD".into()),
            },
            custom_path: None,
            selected_tasks: BTreeSet::new(),
            skipped_rules: BTreeSet::new(),
            granted_consents: BTreeSet::new(),
        };
        let facts = GitCli
            .create_facts(&request, None, "origin", None, None)
            .unwrap();
        assert_eq!(facts.primary_count, 1);
        assert!(!facts.bare);
        assert!(matches!(
            facts.source,
            CreateSource::NewBranch { base: Some(_), .. }
        ));
        assert_eq!(facts.destination.state, DestinationState::Absent);
        let refs_before = git(&repo, ["show-ref"]).unwrap().stdout;
        let _ = facts;
        assert_eq!(git(&repo, ["show-ref"]).unwrap().stdout, refs_before);
    }

    #[test]
    fn unresolved_upstream_is_not_silently_replaced_by_primary_head() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        run_git(temp.path(), ["init", "-b", "main", repo.to_str().unwrap()]);
        run_git(&repo, ["config", "user.name", "facts-test"]);
        run_git(&repo, ["config", "user.email", "facts@example.invalid"]);
        std::fs::write(repo.join("tracked"), b"value").unwrap();
        run_git(&repo, ["add", "tracked"]);
        run_git(&repo, ["commit", "-m", "initial"]);
        let primary = ObjectId::new("0123456789012345678901234567890123456789").unwrap();
        let error = safety_target(
            &repo,
            "feature",
            Some("refs/heads/does-not-exist"),
            &primary,
        )
        .unwrap_err();
        assert_eq!(error.code, "git_facts");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_directory_is_one_artifact_and_source_symlink_is_refused() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir(temp.path().join("dir")).unwrap();
        std::fs::write(temp.path().join("dir/file"), b"x").unwrap();
        let rule = manifest_rule(
            "schema = 1\n[file_rules.link]\nkind = \"symlink\"\nsource = \"dir\"\ndestination = \"dir-link\"\n",
            "link",
        );
        let request = crate::application::CreatePlanRequest {
            repo: temp.path().to_owned(),
            invocation_cwd: temp.path().to_owned(),
            source: crate::application::CreateSourceRequest::New {
                branch: "feature".into(),
                base: Some("HEAD".into()),
            },
            custom_path: None,
            selected_tasks: BTreeSet::new(),
            skipped_rules: BTreeSet::new(),
            granted_consents: BTreeSet::new(),
        };
        let manifests = GitCli
            .plan_manifests(
                &request,
                &manifest_facts(temp.path(), &temp.path().join("future")),
                vec![rule],
            )
            .unwrap();
        assert_eq!(manifests[0].artifacts.len(), 1);
        std::os::unix::fs::symlink("dir/file", temp.path().join("source-link")).unwrap();
        let bad = manifest_rule(
            "schema = 1\n[file_rules.link]\nkind = \"symlink\"\nsource = \"source-link\"\ndestination = \"bad\"\n",
            "link",
        );
        assert_eq!(
            GitCli
                .plan_manifests(
                    &request,
                    &manifest_facts(temp.path(), &temp.path().join("future")),
                    vec![bad]
                )
                .unwrap_err()
                .code,
            "source_type"
        );
    }

    #[cfg(unix)]
    #[test]
    fn trusted_root_alias_is_allowed_but_nested_symlink_is_rejected() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir(temp.path().join("nested")).unwrap();
        std::fs::write(temp.path().join("nested/file"), b"x").unwrap();
        std::os::unix::fs::symlink(temp.path(), temp.path().join("root-alias")).unwrap();

        let allowed = manifest_rule(
            "schema = 1\n[file_rules.file]\nkind = \"copy\"\nsource = \"nested/file\"\ndestination = \"file\"\n",
            "file",
        );
        assert_eq!(
            manifest_candidates(&temp.path().join("root-alias"), &allowed)
                .unwrap()
                .len(),
            1
        );

        std::os::unix::fs::symlink("nested", temp.path().join("nested-alias")).unwrap();
        let rejected = manifest_rule(
            "schema = 1\n[file_rules.file]\nkind = \"copy\"\nsource = \"nested-alias/file\"\ndestination = \"file\"\n",
            "file",
        );
        assert_eq!(
            manifest_candidates(&temp.path().join("root-alias"), &rejected)
                .unwrap_err()
                .code,
            "unsafe_source"
        );
    }

    #[cfg(unix)]
    #[test]
    fn relink_preserves_relative_and_absolute_targets() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("file"), b"x").unwrap();
        std::os::unix::fs::symlink("file", temp.path().join("relative")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("file"), temp.path().join("absolute")).unwrap();
        let request = crate::application::CreatePlanRequest {
            repo: temp.path().to_owned(),
            invocation_cwd: temp.path().to_owned(),
            source: crate::application::CreateSourceRequest::New {
                branch: "feature".into(),
                base: Some("HEAD".into()),
            },
            custom_path: None,
            selected_tasks: BTreeSet::new(),
            skipped_rules: BTreeSet::new(),
            granted_consents: BTreeSet::new(),
        };
        for source in ["relative", "absolute"] {
            let rule = manifest_rule(
                &format!(
                    "schema = 1\n[file_rules.link]\nkind = \"relink\"\nsource = \"{source}\"\ndestination = \"{source}-out\"\non_conflict = \"replace_symlink_only\"\n"
                ),
                "link",
            );
            let error = GitCli
                .plan_manifests(
                    &request,
                    &manifest_facts(temp.path(), &temp.path().join("future")),
                    vec![rule],
                )
                .unwrap_err();
            assert_eq!(error.code, "relink_v3_unavailable");
        }
    }

    #[test]
    fn duplicate_and_ancestor_destinations_are_rejected() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("a"), b"a").unwrap();
        let first = manifest_rule(
            "schema = 1\n[file_rules.a]\nkind = \"copy\"\nsource = \"a\"\ndestination = \"same\"\n",
            "a",
        );
        let second = manifest_rule(
            "schema = 1\n[file_rules.b]\nkind = \"copy\"\nsource = \"a\"\ndestination = \"same/child\"\n",
            "b",
        );
        let request = crate::application::CreatePlanRequest {
            repo: temp.path().to_owned(),
            invocation_cwd: temp.path().to_owned(),
            source: crate::application::CreateSourceRequest::New {
                branch: "feature".into(),
                base: Some("HEAD".into()),
            },
            custom_path: None,
            selected_tasks: BTreeSet::new(),
            skipped_rules: BTreeSet::new(),
            granted_consents: BTreeSet::new(),
        };
        assert_eq!(
            GitCli
                .plan_manifests(
                    &request,
                    &manifest_facts(temp.path(), &temp.path().join("future")),
                    vec![first, second]
                )
                .unwrap_err()
                .code,
            "manifest_overlap"
        );
    }

    #[test]
    fn basename_excludes_apply_at_any_depth() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("nested")).unwrap();
        std::fs::write(temp.path().join("nested/daemon.pid"), b"pid").unwrap();
        std::fs::write(temp.path().join("nested/data.db"), b"db").unwrap();
        let rule = manifest_rule(
            "schema = 1\n[file_rules.db]\nkind = \"copy_tree\"\nsource = \"nested\"\ndestination = \"db\"\nexcludes = [\"daemon.pid\"]\n",
            "db",
        );
        let request = crate::application::CreatePlanRequest {
            repo: temp.path().to_owned(),
            invocation_cwd: temp.path().to_owned(),
            source: crate::application::CreateSourceRequest::New {
                branch: "feature".into(),
                base: Some("HEAD".into()),
            },
            custom_path: None,
            selected_tasks: BTreeSet::new(),
            skipped_rules: BTreeSet::new(),
            granted_consents: BTreeSet::new(),
        };
        let artifacts = &GitCli
            .plan_manifests(
                &request,
                &manifest_facts(temp.path(), &temp.path().join("future")),
                vec![rule],
            )
            .unwrap()[0]
            .artifacts;
        assert_eq!(artifacts.len(), 1);
        assert!(artifacts[0].source.as_path().ends_with("data.db"));
    }

    #[cfg(unix)]
    #[test]
    fn excluded_special_file_is_omitted_before_type_validation() {
        let temp = TempDir::new().unwrap();
        Command::new("mkfifo")
            .arg(temp.path().join("runtime.sock"))
            .status()
            .unwrap();
        let rule = manifest_rule(
            "schema = 1\n[file_rules.runtime]\nkind = \"copy_tree\"\nsource = \".\"\ndestination = \"runtime\"\nexcludes = [\"runtime.sock\"]\n",
            "runtime",
        );
        let request = crate::application::CreatePlanRequest {
            repo: temp.path().to_owned(),
            invocation_cwd: temp.path().to_owned(),
            source: crate::application::CreateSourceRequest::New {
                branch: "feature".into(),
                base: Some("HEAD".into()),
            },
            custom_path: None,
            selected_tasks: BTreeSet::new(),
            skipped_rules: BTreeSet::new(),
            granted_consents: BTreeSet::new(),
        };
        assert!(
            GitCli
                .plan_manifests(
                    &request,
                    &manifest_facts(temp.path(), &temp.path().join("future")),
                    vec![rule]
                )
                .is_ok()
        );
    }

    #[test]
    fn manifest_digest_changes_with_content_and_identity() {
        let temp = TempDir::new().unwrap();
        let a = digest_bytes(b"a");
        let b = digest_bytes(b"b");
        assert_ne!(a, b);
        let artifact = FileArtifact {
            kind: FileArtifactKind::CopyFile,
            source: StoredPath::from(temp.path().join("a")),
            destination: StoredPath::from(temp.path().join("future/a")),
            bytes: 1,
            digest: a.clone(),
            fingerprint: a.clone(),
            source_expectation: crate::lifecycle::ArtifactSourceExpectationV3::Regular(
                crate::lifecycle::RegularFileStateV3 {
                    bytes: 1,
                    digest: a.clone(),
                    mode: 0o644,
                },
            ),
            link_target: None,
            sensitive: false,
            mode_policy: crate::planner::FileModePolicy::PreserveSafe,
            confirm: false,
            conflict: false,
            overlap: false,
            replace_symlink: false,
            compensation: None,
        };
        let mut changed = artifact.clone();
        changed.destination = StoredPath::from(temp.path().join("future/b"));
        assert_ne!(
            manifest_digest(&[artifact], &temp.path().join("future"), temp.path()),
            manifest_digest(&[changed], &temp.path().join("future"), temp.path())
        );
    }

    fn manifest_rule(contents: &str, name: &str) -> crate::application::ManifestRuleSpec {
        let loaded = crate::config::load_layers(
            &[crate::config::LayerContents {
                path: "manifest.toml".into(),
                contents: Some(contents.into()),
                source: crate::config::LayerSource::Project,
            }],
            &crate::config::ConfigOverrides::default(),
        )
        .unwrap();
        let rule = loaded.config.file_rules.get(name).unwrap();
        crate::application::ManifestRuleSpec {
            name: name.into(),
            match_mode: rule.match_mode,
            kind: rule.kind,
            source: rule.source.as_str().into(),
            destination: rule.destination.as_str().into(),
            source_root: rule.source_root,
            ignored_only: rule.ignored_only,
            excludes: rule
                .excludes
                .iter()
                .map(|value| value.as_str().to_owned())
                .collect(),
            sensitive: rule.sensitive,
            confirm: rule.confirm,
        }
    }

    fn manifest_facts(root: &Path, destination: &Path) -> CreatePlanningFacts {
        let oid = ObjectId::new("0123456789012345678901234567890123456789").unwrap();
        let repository = RepositoryIdentity {
            common_dir: StoredPath::from(root.join(".git")),
            primary_root: StoredPath::from(root.to_owned()),
            repository_oid: oid.clone(),
        };
        let branch = BranchName::new("feature").unwrap();
        CreatePlanningFacts {
            repository,
            source: CreateSource::NewBranch {
                branch: branch.clone(),
                base: Some(RefName::new("HEAD").unwrap()),
            },
            source_facts: CreateSourceFacts::NewBranch {
                branch,
                base_ref: RefName::new("HEAD").unwrap(),
                base_oid: oid,
                branch_absent: true,
            },
            bare: false,
            primary_count: 1,
            invocation_cwd: root.to_owned(),
            primary_root: StoredPath::from(root.to_owned()),
            current_worktree_root: StoredPath::from(root.to_owned()),
            destination: DestinationFacts {
                path: StoredPath::from(destination.to_owned()),
                state: DestinationState::Absent,
                parent: StoredPath::from(destination.parent().unwrap().to_owned()),
                parent_safe: true,
            },
            branch_checked_out: false,
            branch_collision: false,
        }
    }

    #[test]
    fn manifest_copy_tree_is_nested_sorted_and_hashed() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("tree/nested")).unwrap();
        std::fs::write(temp.path().join("tree/z"), b"z").unwrap();
        std::fs::write(temp.path().join("tree/nested/a"), b"a").unwrap();
        let rule = manifest_rule(
            "schema = 1\n[file_rules.tree]\nkind = \"copy_tree\"\nsource = \"tree\"\ndestination = \"files\"\n",
            "tree",
        );
        let destination = temp.path().join("future");
        let manifests = GitCli
            .plan_manifests(
                &crate::application::CreatePlanRequest {
                    repo: temp.path().to_owned(),
                    invocation_cwd: temp.path().to_owned(),
                    source: crate::application::CreateSourceRequest::New {
                        branch: "feature".into(),
                        base: Some("HEAD".into()),
                    },
                    custom_path: None,
                    selected_tasks: BTreeSet::new(),
                    skipped_rules: BTreeSet::new(),
                    granted_consents: BTreeSet::new(),
                },
                &manifest_facts(temp.path(), &destination),
                vec![rule],
            )
            .unwrap();
        let paths: Vec<_> = manifests[0]
            .artifacts
            .iter()
            .map(|artifact| {
                artifact
                    .destination
                    .as_path()
                    .strip_prefix(&destination)
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            paths,
            vec![PathBuf::from("files/nested/a"), PathBuf::from("files/z")]
        );
        assert_ne!(
            manifests[0].artifacts[0].digest,
            manifests[0].artifacts[1].digest
        );
    }

    #[test]
    fn manifest_rejects_forbidden_destination_and_preserves_symlink_target() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("source"), b"x").unwrap();
        let forbidden = manifest_rule(
            "schema = 1\n[file_rules.x]\nkind = \"copy\"\nsource = \"source\"\ndestination = \".git\"\n",
            "x",
        );
        assert_eq!(
            GitCli
                .plan_manifests(
                    &crate::application::CreatePlanRequest {
                        repo: temp.path().to_owned(),
                        invocation_cwd: temp.path().to_owned(),
                        source: crate::application::CreateSourceRequest::New {
                            branch: "feature".into(),
                            base: Some("HEAD".into())
                        },
                        custom_path: None,
                        selected_tasks: BTreeSet::new(),
                        skipped_rules: BTreeSet::new(),
                        granted_consents: BTreeSet::new()
                    },
                    &manifest_facts(temp.path(), &temp.path().join("future")),
                    vec![forbidden]
                )
                .unwrap_err()
                .code,
            "forbidden_destination"
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("target/file", temp.path().join("link")).unwrap();
            let relink = manifest_rule(
                "schema = 1\n[file_rules.x]\nkind = \"relink\"\nsource = \"link\"\ndestination = \"link\"\non_conflict = \"replace_symlink_only\"\n",
                "x",
            );
            let error = GitCli
                .plan_manifests(
                    &crate::application::CreatePlanRequest {
                        repo: temp.path().to_owned(),
                        invocation_cwd: temp.path().to_owned(),
                        source: crate::application::CreateSourceRequest::New {
                            branch: "feature".into(),
                            base: Some("HEAD".into()),
                        },
                        custom_path: None,
                        selected_tasks: BTreeSet::new(),
                        skipped_rules: BTreeSet::new(),
                        granted_consents: BTreeSet::new(),
                    },
                    &manifest_facts(temp.path(), &temp.path().join("future")),
                    vec![relink],
                )
                .unwrap_err();
            assert_eq!(error.code, "relink_v3_unavailable");
        }
    }

    #[test]
    fn parser_covers_worktree_variants_and_rejects_bad_delimiters() {
        let bytes = b"worktree /normal\0HEAD abc\0branch refs/heads/main\0\0worktree /detached\0HEAD def\0detached\0\0worktree /bare\0bare\0\0worktree /locked\0HEAD 123\0locked reason\0\0worktree /stale\0HEAD 456\0prunable missing\0\0";
        let records = parse_worktrees(bytes).unwrap();
        assert_eq!(records.len(), 5);
        assert!(records[1].detached);
        assert!(records[2].bare);
        assert_eq!(records[3].locked.as_deref(), Some("reason"));
        assert_eq!(records[4].prunable.as_deref(), Some("missing"));
        assert!(parse_worktrees(b"worktree /x\0HEAD x").is_err());
        assert!(parse_worktrees(b"HEAD x\0\0").is_err());
        assert!(parse_worktrees(b"worktree /x\0branch refs/heads/a\0detached\0\0").is_err());
    }

    #[test]
    fn parser_covers_status_variants() {
        let mut item = Worktree::new(".");
        apply_status(&mut item, b"# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\x001 M. 100644 100644 100644 abc def file name\0").unwrap();
        assert_eq!(item.status, CheckoutStatus::Dirty);
        assert_eq!(item.ahead, Some(2));
        assert_eq!(item.behind, Some(1));
        let mut renamed = Worktree::new(".");
        apply_status(
            &mut renamed,
            b"2 R. N... 100644 100644 100644 abc def R100 new\0old\nname\0",
        )
        .unwrap();
        let mut unmerged = Worktree::new(".");
        apply_status(
            &mut unmerged,
            b"u UU N... 100644 100644 100644 100644 abc def ghi conflict\0",
        )
        .unwrap();
        let mut ignored = Worktree::new(".");
        apply_status(&mut ignored, b"! ignored path\0").unwrap();
        assert_eq!(ignored.status, CheckoutStatus::Clean);
        let mut non_utf8 = Worktree::new(".");
        apply_status(&mut non_utf8, b"? \xff\0").unwrap();
        let mut unborn = Worktree::new(".");
        apply_status(
            &mut unborn,
            b"# branch.oid (initial)\0# branch.head (detached)\0# future ignored\0",
        )
        .unwrap();
        assert_eq!(unborn.status, CheckoutStatus::Clean);
        assert!(apply_status(&mut unborn, b"1 malformed\0").is_err());
        assert!(
            apply_status(
                &mut unborn,
                b"2 R. N... 100644 100644 100644 abc def R100 new\0"
            )
            .is_err()
        );
        assert!(apply_status(&mut unborn, b"x malformed\0").is_err());
        assert!(apply_status(&mut unborn, b"# branch.ab +x -1\0").is_err());
        assert!(apply_status(&mut unborn, b"# branch.head main\0# branch.head other\0").is_err());
    }

    #[test]
    fn real_repositories_list_with_spaces_unicode_and_bare() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("normal source Ω");
        run_git(temp.path(), ["init", root.to_str().unwrap()]);
        std::fs::write(root.join("file"), "content").unwrap();
        run_git(&root, ["add", "."]);
        run_git(&root, ["commit", "-m", "initial"]);
        let linked = temp.path().join("linked worktree Ω");
        run_git(
            &root,
            ["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
        );
        let detached = temp.path().join("detached path");
        run_git(
            &root,
            ["worktree", "add", "--detach", detached.to_str().unwrap()],
        );
        let data = GitCli.list(&root).unwrap();
        assert_eq!(data.data.worktrees.len(), 3);
        assert_eq!(
            data.data
                .worktrees
                .iter()
                .filter(|w| w.classification == WorktreeClass::Primary)
                .count(),
            1
        );
        assert!(data.data.worktrees.iter().any(|w| w.detached));
        let linked_data = GitCli.list(&linked).unwrap();
        let detached_data = GitCli.list(&detached).unwrap();
        for listing in [&data, &linked_data, &detached_data] {
            assert_eq!(
                listing
                    .data
                    .worktrees
                    .iter()
                    .filter(|worktree| worktree.classification == WorktreeClass::Primary)
                    .count(),
                1
            );
        }
        let linked_item = linked_data
            .data
            .worktrees
            .iter()
            .find(|worktree| same_path(&worktree.path, &linked))
            .unwrap();
        assert_eq!(linked_item.classification, WorktreeClass::Linked);
        let detached_item = detached_data
            .data
            .worktrees
            .iter()
            .find(|worktree| same_path(&worktree.path, &detached))
            .unwrap();
        assert_eq!(detached_item.classification, WorktreeClass::Linked);
        assert!(detached_item.detached);
        std::fs::remove_dir_all(&linked).unwrap();
        let stale = GitCli.list(&root).unwrap();
        assert!(
            stale
                .warnings
                .iter()
                .any(|warning| warning.code == "worktree_prunable")
        );
        let bare = temp.path().join("bare repo");
        run_git(temp.path(), ["init", "--bare", bare.to_str().unwrap()]);
        let bare_data = GitCli.list(&bare).unwrap();
        assert!(bare_data.data.repository.bare);
        let bare_item = bare_data
            .data
            .worktrees
            .iter()
            .find(|worktree| same_path(&worktree.path, &bare))
            .unwrap();
        assert_eq!(bare_item.classification, WorktreeClass::Bare);
        assert_ne!(bare_item.classification, WorktreeClass::Primary);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_is_tagged_in_json() {
        use std::os::unix::ffi::OsStringExt;
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'x', 0xff]));
        let item = Worktree::new(path);
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"kind\":\"bytes\""));
        assert!(json.contains("255"));
    }

    #[test]
    fn slug_normalizes_and_caps() {
        assert_eq!(
            unicode_slug("Cafe\u{301}/Feature Name", 64, Path::new(".")).unwrap(),
            "café/feature-name"
        );
        assert_eq!(
            unicode_slug("café", 64, Path::new(".")).unwrap(),
            unicode_slug("cafe\u{301}", 64, Path::new(".")).unwrap()
        );
        assert_eq!(unicode_slug("abcdef", 4, Path::new(".")).unwrap(), "abcd");
        assert_eq!(
            unicode_slug("a///---/β", 64, Path::new(".")).unwrap(),
            "a/β"
        );
        assert_eq!(unicode_slug("ééé", 5, Path::new(".")).unwrap(), "éé");
        assert!(unicode_slug("@{-1}", 64, Path::new(".")).is_err());
        assert!(unicode_slug("a..b", 64, Path::new(".")).is_ok());
    }

    #[test]
    fn collision_is_deterministic() {
        assert_eq!(
            collision_candidate("topic", 2, 64, Path::new(".")).unwrap(),
            "topic-2"
        );
        assert_eq!(
            collision_candidate("long-name", 12, 8, Path::new(".")).unwrap(),
            "long-12"
        );
        assert!(collision_candidate("x", 2, 3, Path::new(".")).is_err());
    }

    fn run_git<'a>(cwd: &Path, args: impl IntoIterator<Item = &'a str>) {
        let output = Command::new("git")
            .current_dir(cwd)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "ewtm test")
            .env("GIT_AUTHOR_EMAIL", "ewtm@example.invalid")
            .env("GIT_COMMITTER_NAME", "ewtm test")
            .env("GIT_COMMITTER_EMAIL", "ewtm@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    mod artifact_primitives {
        use super::*;
        #[cfg(not(target_os = "macos"))]
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        fn id(bytes: &[u8]) -> ObjectId {
            crate::planner::artifact_digest(bytes)
        }

        fn regular(bytes: &[u8], mode: u32) -> crate::lifecycle::RegularFileStateV3 {
            crate::lifecycle::RegularFileStateV3 {
                bytes: bytes.len() as u64,
                digest: id(bytes),
                mode,
            }
        }

        fn staging(path: &Path) -> crate::lifecycle::OwnedStagingV3 {
            crate::lifecycle::OwnedStagingV3 {
                path: StoredPath::from(path.to_owned()),
                ownership_token: id(b"staging-token"),
            }
        }

        fn setup(
            bytes: &[u8],
        ) -> (
            TempDir,
            PathBuf,
            PathBuf,
            PathBuf,
            PathBuf,
            crate::lifecycle::RegularFileStateV3,
        ) {
            let temp = TempDir::new().unwrap();
            let root = temp.path().join("root");
            std::fs::create_dir_all(root.join("sub")).unwrap();
            let source = root.join("sub/source");
            let destination = root.join("sub/destination");
            let stage = root.join("sub/.destination.stage");
            std::fs::write(&source, bytes).unwrap();
            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o641)).unwrap();
            (
                temp,
                root,
                source,
                destination,
                stage,
                regular(bytes, 0o641),
            )
        }

        fn assert_mode(path: &Path, mode: u32) {
            assert_eq!(
                std::fs::symlink_metadata(path).unwrap().mode() & 0o7777,
                mode
            );
        }

        #[test]
        fn copy_contract_bytes_modes_and_publish_are_exact() {
            for mode in [0o600, 0o641] {
                let (_temp, root, source, destination, stage, expected) = setup(b"exact\0bytes");
                let desired = crate::lifecycle::RegularFileStateV3 {
                    mode,
                    ..expected.clone()
                };
                mutate_copy_file_v3(
                    &root,
                    &source,
                    &expected,
                    &destination,
                    &desired,
                    &staging(&stage),
                )
                .unwrap();
                assert_eq!(std::fs::read(&destination).unwrap(), b"exact\0bytes");
                assert_mode(&destination, mode);
                assert!(!stage.exists());
            }
        }

        #[test]
        fn symlink_regular_and_directory_expectations_are_exact() {
            let (_temp, root, source, destination, _stage, expected) = setup(b"source");
            let target = crate::lifecycle::SymlinkStateV3 {
                target: StoredPath::from(source.clone()),
                target_digest: id(source.as_os_str().as_encoded_bytes()),
            };
            mutate_create_symlink_v3(
                &root,
                &source,
                &crate::lifecycle::ArtifactSourceExpectationV3::Regular(expected),
                &destination,
                &target,
            )
            .unwrap();
            assert_eq!(std::fs::read_link(&destination).unwrap(), source);

            let dir = root.join("sub/dir");
            std::fs::create_dir(&dir).unwrap();
            let out = root.join("sub/dir-link");
            let target = crate::lifecycle::SymlinkStateV3 {
                target: StoredPath::from(dir.clone()),
                target_digest: id(dir.as_os_str().as_encoded_bytes()),
            };
            mutate_create_symlink_v3(
                &root,
                &dir,
                &crate::lifecycle::ArtifactSourceExpectationV3::Directory,
                &out,
                &target,
            )
            .unwrap();
            assert_eq!(std::fs::read_link(out).unwrap(), dir);
        }

        #[cfg(not(target_os = "macos"))]
        #[test]
        fn unix_non_utf8_names_are_preserved() {
            let (_temp, root, _source, _destination, _stage, _) = setup(b"unused");
            let parent = root.join("sub");
            let source = parent.join(OsString::from_vec(b"src-\xff".to_vec()));
            let destination = parent.join(OsString::from_vec(b"dst-\xfe".to_vec()));
            let stage = parent.join(OsString::from_vec(b"stage-\xfd".to_vec()));
            std::fs::write(&source, b"raw").unwrap();
            let mode = std::fs::metadata(&source).unwrap().permissions().mode() & 0o7777;
            let expected = regular(b"raw", mode);
            mutate_copy_file_v3(
                &root,
                &source,
                &expected,
                &destination,
                &expected,
                &staging(&stage),
            )
            .unwrap();
            assert_eq!(std::fs::read(&destination).unwrap(), b"raw");
            assert!(!stage.exists());
            assert_eq!(
                destination.file_name().unwrap().as_encoded_bytes(),
                b"dst-\xfe"
            );
        }

        fn unchanged_copy(
            case: impl FnOnce(
                &Path,
                &Path,
                &Path,
                &Path,
                &crate::lifecycle::RegularFileStateV3,
            ) -> Result<(), GitError>,
        ) {
            let (_temp, root, source, destination, stage, expected) = setup(b"before");
            std::fs::write(&destination, b"final").unwrap();
            std::fs::write(&stage, b"staging").unwrap();
            let before = (
                std::fs::read(&destination).unwrap(),
                std::fs::read(&stage).unwrap(),
            );
            assert!(case(&root, &source, &destination, &stage, &expected).is_err());
            assert_eq!(std::fs::read(&destination).unwrap(), before.0);
            assert_eq!(std::fs::read(&stage).unwrap(), before.1);
        }

        #[test]
        fn invalid_copy_contracts_reject_before_mutation() {
            unchanged_copy(|root, source, dest, stage, expected| {
                let mut desired = expected.clone();
                desired.bytes += 1;
                mutate_copy_file_v3(root, source, expected, dest, &desired, &staging(stage))
            });
            unchanged_copy(|root, source, dest, stage, expected| {
                let mut bad = expected.clone();
                bad.mode = 0o10000;
                mutate_copy_file_v3(root, source, &bad, dest, expected, &staging(stage))
            });
            unchanged_copy(|root, _source, dest, stage, expected| {
                mutate_copy_file_v3(root, root, expected, dest, expected, &staging(stage))
            });
            unchanged_copy(|root, source, dest, stage, expected| {
                mutate_copy_file_v3(
                    root,
                    source,
                    expected,
                    &dest.join(".."),
                    expected,
                    &staging(stage),
                )
            });
            unchanged_copy(|root, source, dest, _stage, expected| {
                mutate_copy_file_v3(
                    root,
                    source,
                    expected,
                    dest,
                    expected,
                    &staging(&dest.join("other/.stage")),
                )
            });
        }

        #[test]
        fn symlink_contracts_reject_without_touching_existing_output() {
            let (_temp, root, source, destination, _stage, expected) = setup(b"source");
            std::fs::write(&destination, b"keep").unwrap();
            let good = crate::lifecycle::SymlinkStateV3 {
                target: StoredPath::from(source.clone()),
                target_digest: id(source.as_os_str().as_encoded_bytes()),
            };
            let bad_target = crate::lifecycle::SymlinkStateV3 {
                target: StoredPath::from(root.join("other")),
                target_digest: id(b"other"),
            };
            assert!(
                mutate_create_symlink_v3(
                    &root,
                    &source,
                    &crate::lifecycle::ArtifactSourceExpectationV3::Symlink(good.clone()),
                    &destination,
                    &good
                )
                .is_err()
            );
            assert!(
                mutate_create_symlink_v3(
                    &root,
                    &source,
                    &crate::lifecycle::ArtifactSourceExpectationV3::Regular(expected.clone()),
                    &destination,
                    &bad_target
                )
                .is_err()
            );
            let mut stale = expected;
            stale.digest = id(b"stale");
            assert!(
                mutate_create_symlink_v3(
                    &root,
                    &source,
                    &crate::lifecycle::ArtifactSourceExpectationV3::Regular(stale),
                    &destination,
                    &good
                )
                .is_err()
            );
            assert_eq!(std::fs::read(&destination).unwrap(), b"keep");
        }

        #[test]
        fn no_follow_overwrite_and_missing_parent_safety() {
            let (_temp, root, source, destination, stage, expected) = setup(b"source");
            for existing in ["regular", "stage"] {
                let p = if existing == "regular" {
                    destination.clone()
                } else {
                    stage.clone()
                };
                std::fs::write(&p, b"preserve").unwrap();
            }
            assert!(
                mutate_copy_file_v3(
                    &root,
                    &source,
                    &expected,
                    &destination,
                    &expected,
                    &staging(&stage)
                )
                .is_err()
            );
            assert_eq!(std::fs::read(&destination).unwrap(), b"preserve");
            assert_eq!(std::fs::read(&stage).unwrap(), b"preserve");
            std::fs::remove_file(&destination).unwrap();
            std::fs::remove_file(&stage).unwrap();
            std::os::unix::fs::symlink("elsewhere", &destination).unwrap();
            assert!(
                mutate_copy_file_v3(
                    &root,
                    &source,
                    &expected,
                    &destination,
                    &expected,
                    &staging(&stage)
                )
                .is_err()
            );
            assert_eq!(
                std::fs::read_link(&destination).unwrap(),
                PathBuf::from("elsewhere")
            );
            std::fs::remove_file(&destination).unwrap();
            std::fs::create_dir(&destination).unwrap();
            assert!(
                mutate_copy_file_v3(
                    &root,
                    &source,
                    &expected,
                    &destination,
                    &expected,
                    &staging(&stage)
                )
                .is_err()
            );
            assert!(std::fs::symlink_metadata(&destination).unwrap().is_dir());
            std::fs::remove_dir(&destination).unwrap();
            std::os::unix::fs::symlink("preserved", &stage).unwrap();
            assert!(
                mutate_copy_file_v3(
                    &root,
                    &source,
                    &expected,
                    &destination,
                    &expected,
                    &staging(&stage)
                )
                .is_err()
            );
            assert_eq!(
                std::fs::read_link(&stage).unwrap(),
                PathBuf::from("preserved")
            );
            std::fs::remove_file(&stage).unwrap();
            let outside = TempDir::new().unwrap();
            std::fs::write(outside.path().join("outside"), b"untouched").unwrap();
            let outside_link = root.join("sub/outside-parent");
            std::os::unix::fs::symlink(outside.path(), &outside_link).unwrap();
            let escaped = outside_link.join("new");
            assert!(
                mutate_copy_file_v3(
                    &root,
                    &source,
                    &expected,
                    &escaped,
                    &expected,
                    &staging(&outside.path().join("stage"))
                )
                .is_err()
            );
            assert_eq!(
                std::fs::read(outside.path().join("outside")).unwrap(),
                b"untouched"
            );
            let missing = root.join("missing/child");
            assert!(
                mutate_copy_file_v3(
                    &root,
                    &source,
                    &expected,
                    &missing,
                    &expected,
                    &staging(&root.join("missing/.stage"))
                )
                .is_err()
            );
            assert!(!root.join("missing").exists());
        }

        #[test]
        fn source_drift_special_file_and_symlink_ancestors_are_refused() {
            let (_temp, root, source, destination, stage, expected) = setup(b"source");
            std::fs::write(&source, b"drift").unwrap();
            assert!(
                mutate_copy_file_v3(
                    &root,
                    &source,
                    &expected,
                    &destination,
                    &expected,
                    &staging(&stage)
                )
                .is_err()
            );
            std::fs::remove_file(&source).unwrap();
            std::os::unix::fs::symlink(root.join("sub"), &source).unwrap();
            assert!(
                mutate_copy_file_v3(
                    &root,
                    &source,
                    &expected,
                    &destination,
                    &expected,
                    &staging(&stage)
                )
                .is_err()
            );
            std::fs::remove_file(&source).unwrap();
            std::process::Command::new("mkfifo")
                .arg(&source)
                .status()
                .unwrap();
            assert!(
                mutate_copy_file_v3(
                    &root,
                    &source,
                    &expected,
                    &destination,
                    &expected,
                    &staging(&stage)
                )
                .is_err()
            );
            assert!(!destination.exists() && !stage.exists());
        }

        #[test]
        fn every_artifact_fault_has_one_invocation_and_exact_partial_state() {
            let points = [
                ArtifactFaultPoint::StagingCreate,
                ArtifactFaultPoint::Write,
                ArtifactFaultPoint::Fchmod,
                ArtifactFaultPoint::FileFsync,
                ArtifactFaultPoint::Rename,
                ArtifactFaultPoint::CopyParentFsync,
            ];
            for point in points {
                let (_temp, root, source, destination, stage, expected) = setup(b"fault");
                let desired = crate::lifecycle::RegularFileStateV3 {
                    mode: 0o600,
                    ..expected.clone()
                };
                let guard = arm_artifact_fault(point);
                assert!(
                    mutate_copy_file_v3(
                        &root,
                        &source,
                        &expected,
                        &destination,
                        &desired,
                        &staging(&stage)
                    )
                    .is_err()
                );
                assert_eq!(artifact_fault_invocation_count(point), 1);
                match point {
                    ArtifactFaultPoint::Rename | ArtifactFaultPoint::CopyParentFsync => {
                        assert_eq!(std::fs::read(&destination).unwrap(), b"fault");
                        assert!(!stage.exists());
                    }
                    ArtifactFaultPoint::StagingCreate => {
                        assert_eq!(std::fs::read(&stage).unwrap(), b"");
                        assert!(!destination.exists());
                    }
                    _ => {
                        assert_eq!(std::fs::read(&stage).unwrap(), b"fault");
                        assert!(!destination.exists());
                    }
                }
                drop(guard);
                assert_eq!(artifact_fault_invocation_count(point), 0);
            }
            for point in [
                ArtifactFaultPoint::SymlinkCreate,
                ArtifactFaultPoint::SymlinkParentFsync,
            ] {
                let (_temp, root, source, destination, _stage, expected) = setup(b"fault");
                let target = crate::lifecycle::SymlinkStateV3 {
                    target: StoredPath::from(source.clone()),
                    target_digest: id(source.as_os_str().as_encoded_bytes()),
                };
                let guard = arm_artifact_fault(point);
                assert!(
                    mutate_create_symlink_v3(
                        &root,
                        &source,
                        &crate::lifecycle::ArtifactSourceExpectationV3::Regular(expected),
                        &destination,
                        &target
                    )
                    .is_err()
                );
                assert_eq!(artifact_fault_invocation_count(point), 1);
                assert_eq!(std::fs::read_link(&destination).unwrap(), source);
                drop(guard);
            }
        }

        #[test]
        fn artifact_fault_guard_resets_on_unwind() {
            let result = std::panic::catch_unwind(|| {
                let _guard = arm_artifact_fault(ArtifactFaultPoint::Write);
                panic!("intentional");
            });
            assert!(result.is_err());
            assert_eq!(
                artifact_fault_invocation_count(ArtifactFaultPoint::Write),
                0
            );
        }
    }
}
