use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::lifecycle::RemoteBranch;
use crate::planner::{
    CreatePlanInput, CreateSourceFacts, DestinationFacts, RemoveFacts, RemovePlanInput,
};
use crate::{
    config::{self, ConfigLocations, ConfigOverrides, LayerContents, LoadedConfig},
    domain::{ListData, ListResult, PathDto, Warning},
    worktreerc::{Diagnostic, ImportResult},
};

pub trait RepositoryPort {
    type Error: std::error::Error + Send + Sync + 'static;
    fn list(&self, path: &Path) -> Result<ListResult, Self::Error>;
}
pub trait RecoveryPort {
    type Error: std::error::Error + Send + Sync + 'static;
    fn recover_list(&self, repo: &Path) -> Result<Vec<crate::journal::Journal>, Self::Error>;
    fn recover_show(
        &self,
        repo: &Path,
        id: &crate::lifecycle::OperationId,
    ) -> Result<crate::journal::Journal, Self::Error>;
    fn recovery_error_code(error: &Self::Error) -> &'static str;
}

pub trait ConfigLocationPort {
    fn locations(&self, repo: &Path) -> Result<ConfigLocations, String>;
}

pub trait ConfigFilePort {
    fn read_layers(&self, locations: &ConfigLocations) -> Result<Vec<LayerContents>, String>;
    fn read_import(&self, path: &Path) -> Result<String, String>;
}

pub trait EditorPort {
    fn prepare(&self, target: &Path) -> Result<(), String>;
    fn execute(&self, editor: &str, target: &Path) -> Result<(), String>;
}

pub trait EnvironmentPort {
    fn editor(&self) -> Result<String, String>;
    fn git_available(&self) -> bool;
}

pub trait ProcessPort {
    fn import(&self, source: &str, path: &Path) -> Result<ImportResult, Vec<Diagnostic>>;
}

pub trait PlanFilePort {
    fn read_plan(&self, path: &Path) -> Result<Vec<u8>, PlanFileError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanFileError {
    Missing,
    InvalidPath,
    NotRegular,
    TooLarge,
    Io,
}

impl PlanFileError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Missing | Self::Io => "plan_file_open",
            Self::InvalidPath => "plan_file_not_regular",
            Self::NotRegular => "plan_file_not_regular",
            Self::TooLarge => "plan_file_too_large",
        }
    }
}

pub trait ForwardExecutionPort {
    fn execute(&self, prepared: PreparedApply) -> Result<ExecutionResult, ApplyError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedApply {
    plan: crate::lifecycle::OperationPlan,
    anchor: crate::domain::StoredPath,
    raw_digest: String,
}

impl PreparedApply {
    fn new(
        plan: crate::lifecycle::OperationPlan,
        raw_digest: String,
    ) -> Result<Self, PlanPreparationError> {
        let anchor = match plan.intent() {
            crate::lifecycle::OperationIntent::Create(intent) => intent
                .current_worktree_root
                .clone()
                .ok_or(PlanPreparationError::NonCanonical)?,
            crate::lifecycle::OperationIntent::Remove(intent) => {
                intent.repository.primary_root.clone()
            }
        };
        Ok(Self {
            plan,
            anchor,
            raw_digest,
        })
    }
    pub fn plan(&self) -> &crate::lifecycle::OperationPlan {
        &self.plan
    }
    pub fn anchor(&self) -> &crate::domain::StoredPath {
        &self.anchor
    }
    pub fn raw_digest(&self) -> &str {
        &self.raw_digest
    }
    pub fn into_plan(self) -> crate::lifecycle::OperationPlan {
        self.plan
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionOutcomeKind {
    Applied,
    AlreadyApplied,
    PreflightRefused,
    Paused,
    NeedsAttention,
    ExistingOperation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionResult {
    pub operation_id: crate::lifecycle::OperationId,
    pub outcome: ExecutionOutcomeKind,
    pub step_id: Option<crate::lifecycle::StepId>,
    pub detail: Option<String>,
    pub exit_override: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionResponse {
    pub operation_id: crate::lifecycle::OperationId,
    pub outcome: ExecutionOutcomeKind,
}

impl ExecutionResult {
    pub fn is_success(&self) -> bool {
        matches!(
            self.outcome,
            ExecutionOutcomeKind::Applied | ExecutionOutcomeKind::AlreadyApplied
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyError {
    pub code: &'static str,
    pub message: &'static str,
    pub exit_override: Option<u8>,
}

pub struct ApplyService<'a, R, S, F, E> {
    pub planner: &'a Application<'a, R, S>,
    pub files: &'a F,
    pub forward: &'a E,
}

impl<'a, R, S, F, E> ApplyService<'a, R, S, F, E>
where
    R: RepositoryPort + RecoveryPort,
    S: ConfigLocationPort
        + ConfigFilePort
        + EditorPort
        + EnvironmentPort
        + ProcessPort
        + LifecyclePlanningPort
        + ManifestPlanningPort,
    F: PlanFilePort,
    E: ForwardExecutionPort,
{
    pub fn prepare(
        &self,
        path: &Path,
        expected_digest: &str,
    ) -> Result<PreparedApply, PlanPreparationError> {
        let raw = self.files.read_plan(path).map_err(|error| match error {
            PlanFileError::Missing => PlanPreparationError::FileMissing,
            PlanFileError::InvalidPath => PlanPreparationError::FileInvalidPath,
            PlanFileError::NotRegular => PlanPreparationError::FileNotRegular,
            PlanFileError::Io => PlanPreparationError::FileIo,
            PlanFileError::TooLarge => PlanPreparationError::TooLarge,
        })?;
        let confirmed = crate::plan_authority::confirm_plan(&raw, expected_digest).map_err(
            |error| match error {
                crate::plan_authority::PlanAuthorityError::DigestInvalid => {
                    PlanPreparationError::DigestInvalid
                }
                crate::plan_authority::PlanAuthorityError::DigestMismatch => {
                    PlanPreparationError::DigestMismatch
                }
                crate::plan_authority::PlanAuthorityError::TooLarge => {
                    PlanPreparationError::TooLarge
                }
                crate::plan_authority::PlanAuthorityError::JsonInvalid => {
                    PlanPreparationError::JsonInvalid
                }
                crate::plan_authority::PlanAuthorityError::NonCanonical => {
                    PlanPreparationError::NonCanonical
                }
                crate::plan_authority::PlanAuthorityError::NotExecutable => {
                    PlanPreparationError::NotExecutable
                }
            },
        )?;
        let raw_digest = confirmed.raw_digest().to_owned();
        let plan = self.planner.prepare_confirmed_plan(&confirmed)?;
        PreparedApply::new(plan, raw_digest)
    }

    pub fn apply(&self, path: &Path, expected_digest: &str) -> Result<ExecutionResult, ApplyError> {
        let prepared = self
            .prepare(path, expected_digest)
            .map_err(|error| ApplyError {
                code: error.code(),
                message: error.message(),
                exit_override: None,
            })?;
        self.forward.execute(prepared)
    }
}

#[derive(Debug, Clone)]
pub enum CreateSourceRequest {
    New {
        branch: String,
        base: Option<String>,
    },
    ExistingLocal {
        branch: String,
    },
    RemoteTracking {
        remote: String,
        remote_branch: String,
        local_branch: String,
    },
}

#[derive(Debug, Clone)]
pub struct CreatePlanRequest {
    pub repo: PathBuf,
    pub invocation_cwd: PathBuf,
    pub source: CreateSourceRequest,
    pub custom_path: Option<PathBuf>,
    pub selected_tasks: BTreeSet<String>,
    pub skipped_rules: BTreeSet<String>,
    pub granted_consents: BTreeSet<crate::lifecycle::ConsentId>,
}

#[derive(Debug, Clone)]
pub struct RemovePlanRequest {
    pub repo: PathBuf,
    pub invocation_cwd: PathBuf,
    pub target: PathBuf,
    pub allow_dirty_removal: bool,
    pub delete_local_branch: bool,
    pub force_delete_local_branch: bool,
    pub delete_remote_branch: Option<RemoteBranch>,
    pub granted_consents: BTreeSet<crate::lifecycle::ConsentId>,
}

#[derive(Debug, Clone)]
pub struct PlanningError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanPreparationError {
    FileMissing,
    FileInvalidPath,
    FileNotRegular,
    FileIo,
    DigestInvalid,
    DigestMismatch,
    TooLarge,
    JsonInvalid,
    NonCanonical,
    NotExecutable,
    RegenerationFailed,
    RegenerationMismatch,
}
impl PlanPreparationError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::FileMissing | Self::FileIo => "plan_file_open",
            Self::FileInvalidPath => "plan_file_not_regular",
            Self::FileNotRegular => "plan_file_not_regular",
            Self::DigestInvalid => "plan_digest_invalid",
            Self::DigestMismatch => "plan_digest_mismatch",
            Self::TooLarge => "plan_file_too_large",
            Self::JsonInvalid => "plan_json_invalid",
            Self::NonCanonical => "plan_noncanonical",
            Self::NotExecutable => "plan_not_executable",
            Self::RegenerationFailed => "plan_regeneration_failed",
            Self::RegenerationMismatch => "plan_regeneration_mismatch",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::FileMissing | Self::FileIo => "plan file could not be opened",
            Self::FileInvalidPath => "plan file is not regular",
            Self::FileNotRegular => "plan file is not regular",
            Self::DigestInvalid => "plan digest is invalid",
            Self::DigestMismatch => "plan digest does not match",
            Self::TooLarge => "plan file is too large",
            Self::JsonInvalid => "plan JSON is invalid",
            Self::NonCanonical => "plan is noncanonical",
            Self::NotExecutable => "plan is not executable",
            Self::RegenerationFailed => "plan regeneration failed",
            Self::RegenerationMismatch => "plan regeneration drifted",
        }
    }
}

impl std::fmt::Display for PlanPreparationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for PlanPreparationError {}

pub struct CreatePlanningFacts {
    pub repository: crate::lifecycle::RepositoryIdentity,
    pub source: crate::lifecycle::CreateSource,
    pub source_facts: CreateSourceFacts,
    pub bare: bool,
    pub primary_count: usize,
    pub invocation_cwd: PathBuf,
    pub primary_root: crate::domain::StoredPath,
    pub current_worktree_root: crate::domain::StoredPath,
    pub destination: DestinationFacts,
    pub branch_checked_out: bool,
    pub branch_collision: bool,
}

pub struct RemovePlanningFacts {
    pub repository: crate::lifecycle::RepositoryIdentity,
    pub facts: RemoveFacts,
}

#[derive(Debug, Clone)]
pub struct ManifestRuleSpec {
    pub name: String,
    pub match_mode: config::MatchMode,
    pub kind: config::FileRuleKind,
    pub source: String,
    pub destination: String,
    pub source_root: config::SourceRoot,
    pub ignored_only: bool,
    pub excludes: Vec<String>,
    pub sensitive: bool,
    pub confirm: bool,
}

pub trait ManifestPlanningPort {
    fn plan_manifests(
        &self,
        request: &CreatePlanRequest,
        facts: &CreatePlanningFacts,
        rules: Vec<ManifestRuleSpec>,
    ) -> Result<Vec<crate::planner::FileActionManifest>, PlanningError>;
}

pub trait LifecyclePlanningPort {
    fn create_facts(
        &self,
        request: &CreatePlanRequest,
        default_base: Option<&str>,
        remote: &str,
        worktree_root: Option<&str>,
        directory_prefix: Option<&str>,
        naming: CreateFactsNaming,
    ) -> Result<CreatePlanningFacts, PlanningError>;
    fn remove_facts(
        &self,
        request: &RemovePlanRequest,
    ) -> Result<RemovePlanningFacts, PlanningError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateNamingMode {
    Generate,
    Persisted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateFactsNaming {
    Generate(String),
    Persisted,
}

fn generated_branch_name(stem: &str, suffix: &str) -> String {
    format!("{stem}-{suffix}")
}

fn generated_source(source: CreateSourceRequest, suffix: &str) -> CreateSourceRequest {
    match source {
        CreateSourceRequest::New { branch, base } => CreateSourceRequest::New {
            branch: generated_branch_name(&branch, suffix),
            base,
        },
        CreateSourceRequest::RemoteTracking {
            remote,
            remote_branch,
            local_branch,
        } => CreateSourceRequest::RemoteTracking {
            remote,
            remote_branch,
            local_branch: generated_branch_name(&local_branch, suffix),
        },
        other => other,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DiagnosticDto {
    pub code: String,
    pub message: String,
    pub path: Option<PathDto>,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

#[derive(Debug)]
pub enum ResponseData {
    List(ListData),
    ConfigShow(LoadedConfig),
    ConfigValidate(ValidationResult),
    ConfigImport(ImportResult),
    ConfigEdit(EditResult),
    Doctor(DoctorReport),
    OperationPlan(crate::lifecycle::OperationPlan),
    JournalList(Vec<crate::journal::Journal>),
    Journal(crate::journal::Journal),
    Execution(ExecutionResponse),
    CompensationProposal(crate::compensation::CompensationProposalV1),
}

pub fn operation_plan_data(plan: crate::lifecycle::OperationPlan) -> ResponseData {
    ResponseData::OperationPlan(plan)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationResult {
    pub valid: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditResult {
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

#[derive(Debug)]
pub struct AppError {
    pub diagnostic: DiagnosticDto,
}

#[derive(Debug)]
pub struct AppOutcome {
    pub command: &'static str,
    pub result: Result<ResponseData, AppError>,
    pub warnings: Vec<DiagnosticDto>,
}

impl AppOutcome {
    pub fn ok(command: &'static str, data: ResponseData, warnings: Vec<DiagnosticDto>) -> Self {
        Self {
            command,
            result: Ok(data),
            warnings,
        }
    }

    pub fn fail(command: &'static str, diagnostic: DiagnosticDto) -> Self {
        Self {
            command,
            result: Err(AppError { diagnostic }),
            warnings: Vec::new(),
        }
    }

    pub fn is_success(&self) -> bool {
        self.result.is_ok()
    }
}

#[derive(Debug)]
pub enum Request {
    List {
        path: PathBuf,
    },
    ConfigShow {
        path: PathBuf,
        overrides: ConfigOverrides,
    },
    ConfigValidate {
        path: PathBuf,
        overrides: ConfigOverrides,
    },
    ConfigImport {
        repo: PathBuf,
        file: Option<PathBuf>,
    },
    ConfigEdit {
        repo: PathBuf,
        scope: String,
    },
    Doctor {
        path: PathBuf,
    },
    CreatePlan(CreatePlanRequest),
    RemovePlan(RemovePlanRequest),
    RecoverList {
        repo: PathBuf,
    },
    RecoverShow {
        repo: PathBuf,
        operation_id: crate::lifecycle::OperationId,
    },
}

pub struct Application<'a, R, S> {
    pub repository: &'a R,
    pub system: &'a S,
}

impl<'a, R, S> Application<'a, R, S>
where
    R: RepositoryPort + RecoveryPort,
    S: ConfigLocationPort
        + ConfigFilePort
        + EditorPort
        + EnvironmentPort
        + ProcessPort
        + LifecyclePlanningPort
        + ManifestPlanningPort,
{
    pub fn execute(&self, request: Request) -> AppOutcome {
        match request {
            Request::List { path } => self.list(&path),
            Request::ConfigShow { path, overrides } => {
                self.config(&path, &overrides, "config_show")
            }
            Request::ConfigValidate { path, overrides } => {
                self.config(&path, &overrides, "config_validate")
            }
            Request::ConfigImport { repo, file } => self.import(&repo, file.as_deref()),
            Request::ConfigEdit { repo, scope } => self.edit(&repo, &scope),
            Request::Doctor { path } => self.doctor(&path),
            Request::CreatePlan(request) => {
                let operation_id = crate::planner::new_operation_id();
                self.create_plan(request, operation_id, CreateNamingMode::Generate)
            }
            Request::RemovePlan(request) => self.remove_plan(request, None),
            Request::RecoverList { repo } => self.recover_list(&repo),
            Request::RecoverShow { repo, operation_id } => self.recover_show(&repo, &operation_id),
        }
    }

    /// Reconstruct a request from persisted intent and run the normal read-only
    /// facts/config/manifest/planner path with the persisted operation ID.
    pub fn prepare_confirmed_plan(
        &self,
        confirmed: &crate::plan_authority::ConfirmedPlan,
    ) -> Result<crate::lifecycle::OperationPlan, PlanPreparationError> {
        let plan = confirmed.plan();
        if let Ok(journal) = self.repository.recover_show(
            plan.repository().primary_root.as_path(),
            plan.operation_id(),
        ) && journal.status() == crate::journal::OperationStatus::Applied
            && journal.plan() == plan
        {
            return Ok(plan.clone());
        }
        let outcome = match plan.intent() {
            crate::lifecycle::OperationIntent::Create(intent) => {
                let destination = intent
                    .destination
                    .as_ref()
                    .ok_or(PlanPreparationError::RegenerationFailed)?;
                let current_worktree_root = intent
                    .current_worktree_root
                    .as_ref()
                    .ok_or(PlanPreparationError::RegenerationFailed)?;
                if !destination.as_path().is_absolute() {
                    return Err(PlanPreparationError::RegenerationFailed);
                }
                let source = match &intent.source {
                    crate::lifecycle::CreateSource::NewBranch { branch, base } => {
                        CreateSourceRequest::New {
                            branch: branch.as_str().into(),
                            base: base.as_ref().map(ToString::to_string),
                        }
                    }
                    crate::lifecycle::CreateSource::ExistingLocal { branch } => {
                        CreateSourceRequest::ExistingLocal {
                            branch: branch.as_str().into(),
                        }
                    }
                    crate::lifecycle::CreateSource::RemoteTracking {
                        remote,
                        remote_branch,
                        local_branch,
                    } => CreateSourceRequest::RemoteTracking {
                        remote: remote.as_str().into(),
                        remote_branch: remote_branch.as_str().into(),
                        local_branch: local_branch.as_str().into(),
                    },
                };
                self.create_plan(
                    CreatePlanRequest {
                        repo: current_worktree_root.as_path().to_owned(),
                        invocation_cwd: current_worktree_root.as_path().to_owned(),
                        source,
                        custom_path: Some(destination.as_path().to_owned()),
                        selected_tasks: intent.selected_tasks.clone(),
                        skipped_rules: intent.skipped_rules.clone(),
                        granted_consents: plan.granted_consents().clone(),
                    },
                    *plan.operation_id(),
                    CreateNamingMode::Persisted,
                )
            }
            crate::lifecycle::OperationIntent::Remove(intent) => {
                let root = intent.repository.primary_root.as_path().to_owned();
                self.remove_plan(
                    RemovePlanRequest {
                        repo: root.clone(),
                        invocation_cwd: root,
                        target: intent.worktree.as_path().to_owned(),
                        allow_dirty_removal: intent.allow_dirty_removal,
                        delete_local_branch: intent.delete_local_branch,
                        force_delete_local_branch: intent.force_delete_local_branch,
                        delete_remote_branch: intent.delete_remote_branch.clone(),
                        granted_consents: plan.granted_consents().clone(),
                    },
                    Some(*plan.operation_id()),
                )
            }
        };
        let ResponseData::OperationPlan(regenerated) = outcome
            .result
            .map_err(|_| PlanPreparationError::RegenerationFailed)?
        else {
            return Err(PlanPreparationError::RegenerationFailed);
        };
        if regenerated == *plan {
            Ok(plan.clone())
        } else {
            Err(PlanPreparationError::RegenerationMismatch)
        }
    }

    /// D5.1's application-facing preparation boundary. File acquisition stays
    /// outside this method; the reader supplies the one read byte slice.
    pub fn prepare_plan_bytes(
        &self,
        raw: &[u8],
        expected_digest: &str,
    ) -> Result<crate::lifecycle::OperationPlan, PlanPreparationError> {
        let confirmed = crate::plan_authority::confirm_plan(raw, expected_digest).map_err(
            |error| match error {
                crate::plan_authority::PlanAuthorityError::DigestInvalid => {
                    PlanPreparationError::DigestInvalid
                }
                crate::plan_authority::PlanAuthorityError::DigestMismatch => {
                    PlanPreparationError::DigestMismatch
                }
                crate::plan_authority::PlanAuthorityError::TooLarge => {
                    PlanPreparationError::TooLarge
                }
                crate::plan_authority::PlanAuthorityError::JsonInvalid => {
                    PlanPreparationError::JsonInvalid
                }
                crate::plan_authority::PlanAuthorityError::NonCanonical => {
                    PlanPreparationError::NonCanonical
                }
                crate::plan_authority::PlanAuthorityError::NotExecutable => {
                    PlanPreparationError::NotExecutable
                }
            },
        )?;
        self.prepare_confirmed_plan(&confirmed)
    }

    fn recover_list(&self, repo: &Path) -> AppOutcome {
        match self.repository.recover_list(repo) {
            Ok(value) => {
                AppOutcome::ok("recover_list", ResponseData::JournalList(value), Vec::new())
            }
            Err(error) => AppOutcome::fail(
                "recover_list",
                diagnostic(
                    R::recovery_error_code(&error),
                    error.to_string(),
                    None,
                    None,
                    None,
                ),
            ),
        }
    }
    fn recover_show(&self, repo: &Path, id: &crate::lifecycle::OperationId) -> AppOutcome {
        match self.repository.recover_show(repo, id) {
            Ok(value) => AppOutcome::ok("recover_show", ResponseData::Journal(value), Vec::new()),
            Err(error) => AppOutcome::fail(
                "recover_show",
                diagnostic(
                    R::recovery_error_code(&error),
                    error.to_string(),
                    None,
                    None,
                    None,
                ),
            ),
        }
    }

    fn list(&self, path: &Path) -> AppOutcome {
        match self.repository.list(path) {
            Ok(result) => AppOutcome::ok(
                "list",
                ResponseData::List(result.data),
                result.warnings.into_iter().map(warning).collect(),
            ),
            Err(error) => AppOutcome::fail(
                "list",
                diagnostic("repository_error", error.to_string(), None, None, None),
            ),
        }
    }

    fn create_plan(
        &self,
        request: CreatePlanRequest,
        operation_id: crate::lifecycle::OperationId,
        naming: CreateNamingMode,
    ) -> AppOutcome {
        let mut request = request;
        let facts_naming = match naming {
            CreateNamingMode::Generate => {
                let suffix = crate::planner::generated_name_suffix(&operation_id);
                request.source = generated_source(request.source, &suffix);
                CreateFactsNaming::Generate(suffix)
            }
            CreateNamingMode::Persisted => CreateFactsNaming::Persisted,
        };
        let loaded = match self.loaded(&request.repo, &ConfigOverrides::default()) {
            Ok(value) => value,
            Err(error) => return AppOutcome::fail("create", *error),
        };
        let facts = match self.system.create_facts(
            &request,
            loaded.config.create.default_base.as_deref(),
            &loaded.config.git.remote,
            loaded.config.create.worktree_root.as_deref(),
            loaded.config.create.directory_prefix.as_deref(),
            facts_naming,
        ) {
            Ok(value) => value,
            Err(error) => {
                return AppOutcome::fail(
                    "create",
                    diagnostic(&error.code, error.message, None, None, None),
                );
            }
        };
        let rules = loaded
            .config
            .file_rules
            .iter()
            .filter(|(name, rule)| rule.enabled && !request.skipped_rules.contains(*name))
            .map(|(name, rule)| ManifestRuleSpec {
                name: name.clone(),
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
            })
            .collect();
        let manifests = match self.system.plan_manifests(&request, &facts, rules) {
            Ok(value) => value,
            Err(error) => {
                return AppOutcome::fail(
                    "create",
                    diagnostic(&error.code, error.message, None, None, None),
                );
            }
        };
        let mut tasks = Vec::new();
        for name in &request.selected_tasks {
            let Some(task) = loaded.config.tasks.get(name) else {
                return AppOutcome::fail(
                    "create",
                    diagnostic(
                        "unknown_task",
                        format!("unknown task: {name}"),
                        None,
                        None,
                        None,
                    ),
                );
            };
            let argv = match crate::lifecycle::CommandArgv::new(task.argv.as_slice().to_vec()) {
                Ok(value) => value,
                Err(error) => {
                    return AppOutcome::fail(
                        "create",
                        diagnostic("invalid_task", error, None, None, None),
                    );
                }
            };
            let cwd = crate::planner::normalize_lexical(
                task.cwd
                    .as_ref()
                    .map(|path| facts.destination.path.as_path().join(path.as_str()))
                    .unwrap_or_else(|| facts.destination.path.as_path().to_owned()),
            );
            let relative_cwd = cwd.strip_prefix(facts.destination.path.as_path()).ok();
            if relative_cwd.is_none() || relative_cwd.is_some_and(|path| path.components().any(|component| matches!(component, std::path::Component::Normal(value) if value == ".git" || value == ".ewtm" || value == "ewtm"))) { return AppOutcome::fail("create", diagnostic("invalid_task", format!("task {name} has an unsafe cwd"), None, None, None)); }
            let environment_allowlist = match task
                .environment_allowlist
                .iter()
                .map(|name| crate::lifecycle::EnvironmentName::new(name.as_str().to_owned()))
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(value) => value,
                Err(error) => {
                    return AppOutcome::fail(
                        "create",
                        diagnostic(
                            "invalid_task",
                            format!("task {name}: {error}"),
                            None,
                            None,
                            None,
                        ),
                    );
                }
            };
            tasks.push(crate::planner::TaskSpec {
                name: name.clone(),
                argv,
                cwd: crate::domain::StoredPath::from(cwd),
                enabled: task.enabled,
                post_create: task.phase == config::TaskPhase::PostCreate,
                required: task.required,
                environment_allowlist,
            });
        }
        let task_contracts = tasks
            .iter()
            .filter(|task| request.selected_tasks.contains(&task.name))
            .map(|task| {
                (
                    task.name.clone(),
                    crate::lifecycle::TaskContract {
                        argv: task.argv.clone(),
                        cwd: task.cwd.clone(),
                        required: task.required,
                        environment_allowlist: task.environment_allowlist.clone(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let intent = crate::lifecycle::CreateIntent {
            repository: facts.repository.clone(),
            source: facts.source,
            destination: Some(facts.destination.path.clone()),
            selected_tasks: request.selected_tasks.clone(),
            skipped_rules: request.skipped_rules.clone(),
            granted_consents: request.granted_consents.clone(),
            task_contracts,
            current_worktree_root: Some(facts.current_worktree_root.clone()),
            artifact_rule_contracts: BTreeMap::new(),
        };
        let input = CreatePlanInput {
            operation_id,
            repository: facts.repository,
            intent,
            bare: facts.bare,
            primary_count: facts.primary_count,
            invocation_cwd: crate::domain::StoredPath::from(facts.invocation_cwd),
            primary_root: facts.primary_root,
            current_worktree_root: facts.current_worktree_root,
            destination: facts.destination,
            source_facts: facts.source_facts,
            branch_checked_out: facts.branch_checked_out,
            branch_collision: facts.branch_collision,
            known_rules: loaded.config.file_rules.keys().cloned().collect(),
            enabled_rules: loaded
                .config
                .file_rules
                .iter()
                .filter(|(_, rule)| rule.enabled)
                .map(|(name, _)| name.clone())
                .collect(),
            known_tasks: loaded.config.tasks.keys().cloned().collect(),
            manifests,
            tasks,
        };
        match crate::planner::plan_create(input) {
            Ok(plan) => AppOutcome::ok("create", ResponseData::OperationPlan(plan), Vec::new()),
            Err(error) => AppOutcome::fail(
                "create",
                diagnostic("planning_refused", error, None, None, None),
            ),
        }
    }

    fn remove_plan(
        &self,
        request: RemovePlanRequest,
        operation_id: Option<crate::lifecycle::OperationId>,
    ) -> AppOutcome {
        if let Err(error) = self.loaded(&request.repo, &ConfigOverrides::default()) {
            return AppOutcome::fail("remove", *error);
        }
        let facts = match self.system.remove_facts(&request) {
            Ok(value) => value,
            Err(error) => {
                return AppOutcome::fail(
                    "remove",
                    diagnostic(&error.code, error.message, None, None, None),
                );
            }
        };
        let intent = match crate::lifecycle::RemoveIntent::new(
            facts.repository.clone(),
            facts.facts.path.clone(),
            request.allow_dirty_removal,
            request.delete_local_branch,
            request.force_delete_local_branch,
            request.delete_remote_branch.clone(),
            request.granted_consents.clone(),
        ) {
            Ok(value) => value,
            Err(error) => {
                return AppOutcome::fail(
                    "remove",
                    diagnostic("invalid_remove_intent", error, None, None, None),
                );
            }
        };
        match crate::planner::plan_remove(RemovePlanInput {
            operation_id: operation_id.unwrap_or_else(crate::planner::new_operation_id),
            intent,
            facts: facts.facts,
        }) {
            Ok(plan) => AppOutcome::ok("remove", ResponseData::OperationPlan(plan), Vec::new()),
            Err(error) => AppOutcome::fail(
                "remove",
                diagnostic("planning_refused", error, None, None, None),
            ),
        }
    }

    fn loaded(
        &self,
        path: &Path,
        overrides: &ConfigOverrides,
    ) -> Result<LoadedConfig, Box<DiagnosticDto>> {
        let locations = self
            .system
            .locations(path)
            .map_err(|message| Box::new(diagnostic("location_error", message, None, None, None)))?;
        let layers = self
            .system
            .read_layers(&locations)
            .map_err(|message| Box::new(diagnostic("config_io", message, None, None, None)))?;
        config::load_layers(&layers, overrides).map_err(|error| Box::new(config_diagnostic(error)))
    }

    fn config(
        &self,
        path: &Path,
        overrides: &ConfigOverrides,
        command: &'static str,
    ) -> AppOutcome {
        match self.loaded(path, overrides) {
            Ok(loaded) if command == "config_show" => {
                AppOutcome::ok(command, ResponseData::ConfigShow(loaded), Vec::new())
            }
            Ok(_) => AppOutcome::ok(
                command,
                ResponseData::ConfigValidate(ValidationResult { valid: true }),
                Vec::new(),
            ),
            Err(error) => AppOutcome::fail(command, *error),
        }
    }

    fn import(&self, repo: &Path, file: Option<&Path>) -> AppOutcome {
        let source = match file {
            Some(path) => path.to_owned(),
            None => match self.system.locations(repo) {
                Ok(locations) => locations.project.with_file_name(".worktreerc"),
                Err(message) => {
                    return AppOutcome::fail(
                        "config_import",
                        diagnostic("location_error", message, None, None, None),
                    );
                }
            },
        };
        let text = match self.system.read_import(&source) {
            Ok(text) => text,
            Err(message) => {
                return AppOutcome::fail(
                    "config_import",
                    diagnostic("import_io", message, Some(source), None, None),
                );
            }
        };
        match self.system.import(&text, &source) {
            Ok(result) => {
                let warnings = result
                    .diagnostics
                    .iter()
                    .map(|item| import_warning(item, &result.source))
                    .collect();
                AppOutcome::ok(
                    "config_import",
                    ResponseData::ConfigImport(result),
                    warnings,
                )
            }
            Err(diagnostics) => {
                let location = diagnostics.first().map(|item| (item.line, item.column));
                let message = diagnostics
                    .into_iter()
                    .map(|item| item.message)
                    .collect::<Vec<_>>()
                    .join("; ");
                AppOutcome::fail(
                    "config_import",
                    diagnostic(
                        "import_failed",
                        message,
                        Some(source),
                        location.map(|item| item.0),
                        location.map(|item| item.1),
                    ),
                )
            }
        }
    }

    fn edit(&self, repo: &Path, scope: &str) -> AppOutcome {
        let locations = match self.system.locations(repo) {
            Ok(locations) => locations,
            Err(message) => {
                return AppOutcome::fail(
                    "config_edit",
                    diagnostic("location_error", message, None, None, None),
                );
            }
        };
        let target = match scope {
            "user" => locations.user,
            "project" => Some(locations.project),
            "local" => Some(locations.local),
            _ => {
                return AppOutcome::fail(
                    "config_edit",
                    diagnostic(
                        "invalid_scope",
                        "scope must be user, project, or local".into(),
                        None,
                        None,
                        None,
                    ),
                );
            }
        };
        let Some(target) = target else {
            return AppOutcome::fail(
                "config_edit",
                diagnostic(
                    "location_error",
                    "user config location is unavailable".into(),
                    None,
                    None,
                    None,
                ),
            );
        };
        let editor = match self.system.editor() {
            Ok(editor) if valid_editor(&editor) => editor,
            Ok(_editor) => {
                return AppOutcome::fail(
                    "config_edit",
                    diagnostic(
                        "editor_error",
                        "editor must be one executable without whitespace".into(),
                        None,
                        None,
                        None,
                    ),
                );
            }
            Err(message) => {
                return AppOutcome::fail(
                    "config_edit",
                    diagnostic("editor_error", message, None, None, None),
                );
            }
        };
        if let Err(message) = self.system.prepare(&target) {
            return AppOutcome::fail(
                "config_edit",
                diagnostic("edit_prepare", message, Some(target), None, None),
            );
        }
        match self.system.execute(&editor, &target) {
            Ok(()) => AppOutcome::ok(
                "config_edit",
                ResponseData::ConfigEdit(EditResult { path: target }),
                Vec::new(),
            ),
            Err(message) => AppOutcome::fail(
                "config_edit",
                diagnostic("editor_failed", message, Some(target), None, None),
            ),
        }
    }

    fn doctor(&self, path: &Path) -> AppOutcome {
        let checks = vec![
            DoctorCheck {
                name: "git",
                ok: self.system.git_available(),
            },
            DoctorCheck {
                name: "repository",
                ok: self.repository.list(path).is_ok(),
            },
            DoctorCheck {
                name: "config",
                ok: self.loaded(path, &ConfigOverrides::default()).is_ok(),
            },
        ];
        let report = DoctorReport { checks };
        if report.checks.iter().all(|check| check.ok) {
            AppOutcome::ok("doctor", ResponseData::Doctor(report), Vec::new())
        } else {
            AppOutcome {
                command: "doctor",
                result: Err(AppError {
                    diagnostic: diagnostic(
                        "doctor_failed",
                        "one or more required checks failed".into(),
                        None,
                        None,
                        None,
                    ),
                }),
                warnings: Vec::new(),
            }
        }
    }
}

pub fn valid_editor(editor: &str) -> bool {
    !editor.trim().is_empty() && !editor.chars().any(char::is_whitespace)
}

fn diagnostic(
    code: &str,
    message: String,
    path: Option<PathBuf>,
    line: Option<usize>,
    column: Option<usize>,
) -> DiagnosticDto {
    DiagnosticDto {
        code: code.into(),
        message,
        path: path.map(PathDto::from),
        line,
        column,
    }
}

fn config_diagnostic(error: config::ConfigError) -> DiagnosticDto {
    let (path, line, column, message) = error.details();
    diagnostic("config_error", message, Some(path), line, column)
}

fn warning(warning: Warning) -> DiagnosticDto {
    diagnostic(&warning.code, warning.message, warning.path, None, None)
}

fn import_warning(item: &Diagnostic, source: &Path) -> DiagnosticDto {
    diagnostic(
        "import_diagnostic",
        item.message.clone(),
        Some(source.to_owned()),
        Some(item.line),
        Some(item.column),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{cell::RefCell, fs, io, path::PathBuf, rc::Rc};

    struct FakeRepository;
    impl RepositoryPort for FakeRepository {
        type Error = io::Error;
        fn list(&self, _path: &Path) -> Result<ListResult, Self::Error> {
            Ok(ListResult {
                data: ListData {
                    repository: crate::domain::RepositorySummary {
                        common_dir: ".git".into(),
                        bare: false,
                    },
                    worktrees: Vec::new(),
                },
                warnings: Vec::new(),
            })
        }
    }
    impl RecoveryPort for FakeRepository {
        type Error = io::Error;
        fn recover_list(&self, _repo: &Path) -> Result<Vec<crate::journal::Journal>, Self::Error> {
            Err(io::Error::other("unused"))
        }
        fn recover_show(
            &self,
            _repo: &Path,
            _id: &crate::lifecycle::OperationId,
        ) -> Result<crate::journal::Journal, Self::Error> {
            Err(io::Error::other("unused"))
        }
        fn recovery_error_code(_error: &Self::Error) -> &'static str {
            "journal_error"
        }
    }

    struct FakeSystem {
        editor: Option<String>,
        events: Rc<RefCell<Vec<String>>>,
    }
    impl ConfigLocationPort for FakeSystem {
        fn locations(&self, _repo: &Path) -> Result<ConfigLocations, String> {
            Ok(ConfigLocations {
                user: Some("home/config.toml".into()),
                project: "project/.ewtm.toml".into(),
                local: "common/ewtm/config.toml".into(),
            })
        }
    }
    impl ConfigFilePort for FakeSystem {
        fn read_layers(&self, _locations: &ConfigLocations) -> Result<Vec<LayerContents>, String> {
            Ok(Vec::new())
        }
        fn read_import(&self, path: &Path) -> Result<String, String> {
            if path == Path::new("missing") {
                Err("not found".into())
            } else {
                Ok("WORKTREE_SLUG_MAX = 8".into())
            }
        }
    }
    impl EditorPort for FakeSystem {
        fn prepare(&self, _target: &Path) -> Result<(), String> {
            self.events.borrow_mut().push("prepare".into());
            Ok(())
        }
        fn execute(&self, _editor: &str, _target: &Path) -> Result<(), String> {
            self.events.borrow_mut().push("execute".into());
            Ok(())
        }
    }
    impl EnvironmentPort for FakeSystem {
        fn editor(&self) -> Result<String, String> {
            self.editor.clone().ok_or_else(|| "missing editor".into())
        }
        fn git_available(&self) -> bool {
            true
        }
    }
    impl ProcessPort for FakeSystem {
        fn import(&self, source: &str, path: &Path) -> Result<ImportResult, Vec<Diagnostic>> {
            crate::worktreerc::import_source(source, path)
        }
    }
    impl LifecyclePlanningPort for FakeSystem {
        fn create_facts(
            &self,
            _request: &CreatePlanRequest,
            _default_base: Option<&str>,
            _remote: &str,
            _worktree_root: Option<&str>,
            _directory_prefix: Option<&str>,
            _naming: CreateFactsNaming,
        ) -> Result<CreatePlanningFacts, PlanningError> {
            Err(PlanningError {
                code: "test_unimplemented".into(),
                message: "not used by M1 tests".into(),
            })
        }
        fn remove_facts(
            &self,
            _request: &RemovePlanRequest,
        ) -> Result<RemovePlanningFacts, PlanningError> {
            Err(PlanningError {
                code: "test_unimplemented".into(),
                message: "not used by M1 tests".into(),
            })
        }
    }
    impl ManifestPlanningPort for FakeSystem {
        fn plan_manifests(
            &self,
            _request: &CreatePlanRequest,
            _facts: &CreatePlanningFacts,
            _rules: Vec<ManifestRuleSpec>,
        ) -> Result<Vec<crate::planner::FileActionManifest>, PlanningError> {
            Ok(Vec::new())
        }
    }

    fn app(system: &FakeSystem) -> Application<'_, FakeRepository, FakeSystem> {
        Application {
            repository: &FakeRepository,
            system,
        }
    }

    #[test]
    fn fixed_id_generate_source_modes_bind_final_names() {
        let id = crate::lifecycle::OperationId::new(
            uuid::Uuid::parse_str("00112233-4455-4677-8899-aabbccddeeff").unwrap(),
        );
        let suffix = crate::planner::generated_name_suffix(&id);
        assert_eq!(suffix, "aaisem2e");
        assert!(matches!(
            generated_source(
                CreateSourceRequest::New {
                    branch: "feature/a".into(),
                    base: None,
                },
                &suffix,
            ),
            CreateSourceRequest::New { branch, .. } if branch == "feature/a-aaisem2e"
        ));
        assert!(matches!(
            generated_source(
                CreateSourceRequest::RemoteTracking {
                    remote: "origin".into(),
                    remote_branch: "feature/a".into(),
                    local_branch: "local".into(),
                },
                &suffix,
            ),
            CreateSourceRequest::RemoteTracking { remote, remote_branch, local_branch }
                if remote == "origin" && remote_branch == "feature/a" && local_branch == "local-aaisem2e"
        ));
        assert!(matches!(
            generated_source(
                CreateSourceRequest::ExistingLocal { branch: "existing".into() },
                &suffix,
            ),
            CreateSourceRequest::ExistingLocal { branch } if branch == "existing"
        ));
        assert_eq!(
            generated_branch_name("already-aaisem2e", &suffix),
            "already-aaisem2e-aaisem2e"
        );
    }

    #[test]
    fn every_use_case_has_stable_command_id() {
        let system = FakeSystem {
            editor: Some("vi".into()),
            events: Rc::new(RefCell::new(Vec::new())),
        };
        let application = app(&system);
        let requests = vec![
            Request::List { path: ".".into() },
            Request::ConfigShow {
                path: ".".into(),
                overrides: ConfigOverrides::default(),
            },
            Request::ConfigValidate {
                path: ".".into(),
                overrides: ConfigOverrides::default(),
            },
            Request::ConfigImport {
                repo: ".".into(),
                file: Some("import.rc".into()),
            },
            Request::ConfigEdit {
                repo: ".".into(),
                scope: "project".into(),
            },
            Request::Doctor { path: ".".into() },
        ];
        let commands: Vec<_> = requests
            .into_iter()
            .map(|request| application.execute(request).command)
            .collect();
        assert_eq!(
            commands,
            [
                "list",
                "config_show",
                "config_validate",
                "config_import",
                "config_edit",
                "doctor"
            ]
        );
    }

    #[test]
    fn editor_is_validated_before_prepare() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let system = FakeSystem {
            editor: None,
            events: events.clone(),
        };
        let outcome = app(&system).execute(Request::ConfigEdit {
            repo: ".".into(),
            scope: "project".into(),
        });
        assert!(outcome.result.is_err());
        assert!(events.borrow().is_empty());
    }

    #[test]
    fn editor_prepare_precedes_execute() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let system = FakeSystem {
            editor: Some("vi".into()),
            events: events.clone(),
        };
        let outcome = app(&system).execute(Request::ConfigEdit {
            repo: ".".into(),
            scope: "project".into(),
        });
        assert!(outcome.result.is_ok());
        assert_eq!(&*events.borrow(), &["prepare", "execute"]);
    }

    #[test]
    fn failures_are_typed_and_keep_command_id() {
        let system = FakeSystem {
            editor: Some("vi".into()),
            events: Rc::new(RefCell::new(Vec::new())),
        };
        let outcome = app(&system).execute(Request::ConfigImport {
            repo: ".".into(),
            file: Some("missing".into()),
        });
        assert_eq!(outcome.command, "config_import");
        assert_eq!(outcome.result.unwrap_err().diagnostic.code, "import_io");
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct CreateSnapshot {
        repo: PathBuf,
        invocation_cwd: PathBuf,
        custom_path: Option<PathBuf>,
        source: String,
        selected_tasks: BTreeSet<String>,
        skipped_rules: BTreeSet<String>,
        grants: BTreeSet<crate::lifecycle::ConsentId>,
    }
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RemoveSnapshot {
        repo: PathBuf,
        invocation_cwd: PathBuf,
        target: PathBuf,
        allow_dirty_removal: bool,
        delete_local_branch: bool,
        force_delete_local_branch: bool,
        delete_remote_branch: Option<RemoteBranch>,
        grants: BTreeSet<crate::lifecycle::ConsentId>,
    }
    #[derive(Default)]
    struct Trace {
        creates: Vec<CreateSnapshot>,
        removes: Vec<RemoveSnapshot>,
        locations: Vec<PathBuf>,
        reads: usize,
        manifests: usize,
    }
    struct PlanningFixture {
        _tempdir: tempfile::TempDir,
        repository: FakeRepository,
        trace: Rc<RefCell<Trace>>,
        create: CreatePlanningFacts,
        remove: RemovePlanningFacts,
        layers: Vec<LayerContents>,
        manifests: Vec<crate::planner::FileActionManifest>,
    }

    fn id(value: &str) -> crate::lifecycle::ObjectId {
        crate::lifecycle::ObjectId::new(value).unwrap()
    }
    fn branch(value: &str) -> crate::lifecycle::BranchName {
        crate::lifecycle::BranchName::new(value).unwrap()
    }
    fn reference(value: &str) -> crate::lifecycle::RefName {
        crate::lifecycle::RefName::new(value).unwrap()
    }
    fn remote(value: &str) -> crate::lifecycle::RemoteName {
        crate::lifecycle::RemoteName::new(value).unwrap()
    }
    fn path(value: PathBuf) -> crate::domain::StoredPath {
        crate::domain::StoredPath::from(value)
    }
    fn consent(value: &str) -> crate::lifecycle::ConsentId {
        crate::lifecycle::ConsentId::new(value).unwrap()
    }

    impl PlanningFixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let root_path = fs::canonicalize(root.path()).unwrap();
            let primary = root_path.join("primary");
            let current = root_path.join("current");
            let target = root_path.join("remove-target");
            let common = root_path.join("common");
            for item in [&primary, &current, &target, &common] {
                fs::create_dir_all(item).unwrap();
            }
            let destination = root_path.join("custom-destination");
            fs::create_dir_all(destination.join(".git/ewtm")).unwrap();
            fs::create_dir_all(common.join(".git/ewtm")).unwrap();
            fs::write(current.join("asset"), b"asset").unwrap();
            fs::write(primary.join(".git-sentinel"), b"primary").unwrap();
            fs::write(common.join(".git/sentinel"), b"git").unwrap();
            fs::write(common.join(".git/ewtm/task.log"), b"task-log").unwrap();
            fs::write(destination.join("sentinel"), b"destination").unwrap();
            let repository = crate::lifecycle::RepositoryIdentity {
                common_dir: path(common.clone()),
                primary_root: path(primary.clone()),
                repository_oid: id("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            };
            let source = crate::lifecycle::CreateSource::NewBranch {
                branch: branch("feature/pass-a"),
                base: Some(reference("refs/heads/main")),
            };
            let create = CreatePlanningFacts {
                repository: repository.clone(),
                source,
                source_facts: CreateSourceFacts::NewBranch {
                    branch: branch("feature/pass-a"),
                    base_ref: reference("refs/heads/main"),
                    base_oid: id("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                    branch_absent: true,
                },
                bare: false,
                primary_count: 1,
                invocation_cwd: current.clone(),
                primary_root: path(primary.clone()),
                current_worktree_root: path(current.clone()),
                destination: DestinationFacts {
                    path: path(destination.clone()),
                    state: crate::planner::DestinationState::Absent,
                    parent: path(root_path.clone()),
                    parent_safe: true,
                },
                branch_checked_out: false,
                branch_collision: false,
            };
            let artifact_digest = id("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
            let artifact = crate::planner::FileArtifact {
                kind: crate::planner::FileArtifactKind::CopyFile,
                source: path(current.join("asset")),
                destination: path(destination.join("asset")),
                bytes: 5,
                digest: artifact_digest.clone(),
                source_expectation: crate::lifecycle::ArtifactSourceExpectationV3::Regular(
                    crate::lifecycle::RegularFileStateV3 {
                        bytes: 5,
                        digest: artifact_digest.clone(),
                        mode: 0o644,
                    },
                ),
                fingerprint: artifact_digest.clone(),
                link_target: None,
                sensitive: false,
                mode_policy: crate::planner::FileModePolicy::PreserveSafe,
                confirm: false,
                conflict: false,
                overlap: false,
                replace_symlink: false,
                compensation: None,
                relink_facts: None,
            };
            let manifest_digest = crate::planner::canonical_manifest_digest(
                &[crate::planner::ManifestDigestArtifact {
                    source_root: path(current.clone()),
                    source: path(current.join("asset")),
                    destination: path(destination.join("asset")),
                    kind: crate::planner::FileArtifactKind::CopyFile,
                    bytes: 5,
                    digest: artifact_digest.clone(),
                    fingerprint: artifact_digest.clone(),
                    link_target: None,
                    sensitive: false,
                    confirm: false,
                    mode_policy: crate::planner::FileModePolicy::PreserveSafe,
                }],
                &destination,
            );
            let manifests = vec![crate::planner::FileActionManifest {
                rule: "asset".into(),
                source_root: path(current.clone()),
                artifacts: vec![artifact],
                digest: manifest_digest,
            }];
            let remote_branch = RemoteBranch {
                remote: remote("origin"),
                branch: branch("feature/pass-a"),
            };
            let remove = RemovePlanningFacts {
                repository: repository.clone(),
                facts: RemoveFacts {
                    repository,
                    class: crate::domain::WorktreeClass::Linked,
                    locked: false,
                    prunable: false,
                    ongoing: false,
                    oid_matches: true,
                    branch_elsewhere: false,
                    dirty: true,
                    local_branch_safe_to_delete: false,
                    safe_target_ref: reference("refs/heads/main"),
                    safe_target: id("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                    merge_provenance: crate::lifecycle::MergeTargetProvenance::LegacyUnspecified,
                    branch: branch("feature/pass-a"),
                    branch_oid: id("cccccccccccccccccccccccccccccccccccccccc"),
                    worktree_oid: id("cccccccccccccccccccccccccccccccccccccccc"),
                    remote_branch: Some(remote_branch.clone()),
                    remote_branch_oid: Some(id("dddddddddddddddddddddddddddddddddddddddd")),
                    remote_is_default: false,
                    path: path(target),
                },
            };
            let layers = vec![LayerContents {
                path: root_path.join("project.toml"),
                contents: Some("schema = 1\n[file_rules.asset]\nkind = \"copy\"\nsource = \"asset\"\ndestination = \"asset\"\nsource_root = \"current_worktree\"\n[tasks.build]\nphase = \"post_create\"\nargv = [\"build\"]\nrequired = true\nenabled = true\n[tasks.unselected]\nphase = \"post_create\"\nargv = [\"unused\"]\nenabled = false\n".into()),
                source: config::LayerSource::Project,
            }];
            Self {
                _tempdir: root,
                repository: FakeRepository,
                trace: Rc::new(RefCell::new(Trace::default())),
                create,
                remove,
                layers,
                manifests,
            }
        }
        fn run(&self, request: Request) -> AppOutcome {
            let system = PlanningSystem { fixture: self };
            Application {
                repository: &self.repository,
                system: &system,
            }
            .execute(request)
        }
        fn create_request(&self) -> CreatePlanRequest {
            let current = self.create.current_worktree_root.as_path().to_owned();
            CreatePlanRequest {
                repo: self.create.primary_root.as_path().to_owned(),
                invocation_cwd: current,
                source: CreateSourceRequest::New {
                    branch: "feature/pass-a".into(),
                    base: Some("refs/heads/main".into()),
                },
                custom_path: Some(self.create.destination.path.as_path().to_owned()),
                selected_tasks: ["build".into()].into_iter().collect(),
                skipped_rules: BTreeSet::new(),
                granted_consents: [consent("task:build")].into_iter().collect(),
            }
        }
        fn remove_request(&self) -> RemovePlanRequest {
            let f = &self.remove.facts;
            RemovePlanRequest {
                repo: f.repository.primary_root.as_path().to_owned(),
                invocation_cwd: f.repository.primary_root.as_path().to_owned(),
                target: f.path.as_path().to_owned(),
                allow_dirty_removal: true,
                delete_local_branch: true,
                force_delete_local_branch: true,
                delete_remote_branch: f.remote_branch.clone(),
                granted_consents: [
                    "remove:worktree",
                    "remove:dirty",
                    "remove:local-branch",
                    "remove:force-local-branch",
                    "remove:remote:origin/feature/pass-a",
                ]
                .into_iter()
                .map(consent)
                .collect(),
            }
        }
    }

    struct PlanningSystem<'a> {
        fixture: &'a PlanningFixture,
    }
    impl ConfigLocationPort for PlanningSystem<'_> {
        fn locations(&self, repo: &Path) -> Result<ConfigLocations, String> {
            self.fixture
                .trace
                .borrow_mut()
                .locations
                .push(repo.to_owned());
            Ok(ConfigLocations {
                user: None,
                project: self.fixture.layers[0].path.clone(),
                local: repo.join(".ewtm.toml"),
            })
        }
    }
    impl ConfigFilePort for PlanningSystem<'_> {
        fn read_layers(&self, _: &ConfigLocations) -> Result<Vec<LayerContents>, String> {
            self.fixture.trace.borrow_mut().reads += 1;
            Ok(self.fixture.layers.clone())
        }
        fn read_import(&self, _: &Path) -> Result<String, String> {
            Ok(String::new())
        }
    }
    impl EditorPort for PlanningSystem<'_> {
        fn prepare(&self, _: &Path) -> Result<(), String> {
            Ok(())
        }
        fn execute(&self, _: &str, _: &Path) -> Result<(), String> {
            Ok(())
        }
    }
    impl EnvironmentPort for PlanningSystem<'_> {
        fn editor(&self) -> Result<String, String> {
            Ok("vi".into())
        }
        fn git_available(&self) -> bool {
            true
        }
    }
    impl ProcessPort for PlanningSystem<'_> {
        fn import(&self, source: &str, path: &Path) -> Result<ImportResult, Vec<Diagnostic>> {
            crate::worktreerc::import_source(source, path)
        }
    }
    impl LifecyclePlanningPort for PlanningSystem<'_> {
        fn create_facts(
            &self,
            request: &CreatePlanRequest,
            _: Option<&str>,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: CreateFactsNaming,
        ) -> Result<CreatePlanningFacts, PlanningError> {
            self.fixture
                .trace
                .borrow_mut()
                .creates
                .push(CreateSnapshot {
                    repo: request.repo.clone(),
                    invocation_cwd: request.invocation_cwd.clone(),
                    custom_path: request.custom_path.clone(),
                    source: format!("{:?}", request.source),
                    selected_tasks: request.selected_tasks.clone(),
                    skipped_rules: request.skipped_rules.clone(),
                    grants: request.granted_consents.clone(),
                });
            let f = &self.fixture.create;
            Ok(CreatePlanningFacts {
                repository: f.repository.clone(),
                source: f.source.clone(),
                source_facts: f.source_facts.clone(),
                bare: f.bare,
                primary_count: f.primary_count,
                invocation_cwd: f.invocation_cwd.clone(),
                primary_root: f.primary_root.clone(),
                current_worktree_root: f.current_worktree_root.clone(),
                destination: f.destination.clone(),
                branch_checked_out: f.branch_checked_out,
                branch_collision: f.branch_collision,
            })
        }
        fn remove_facts(
            &self,
            request: &RemovePlanRequest,
        ) -> Result<RemovePlanningFacts, PlanningError> {
            self.fixture
                .trace
                .borrow_mut()
                .removes
                .push(RemoveSnapshot {
                    repo: request.repo.clone(),
                    invocation_cwd: request.invocation_cwd.clone(),
                    target: request.target.clone(),
                    allow_dirty_removal: request.allow_dirty_removal,
                    delete_local_branch: request.delete_local_branch,
                    force_delete_local_branch: request.force_delete_local_branch,
                    delete_remote_branch: request.delete_remote_branch.clone(),
                    grants: request.granted_consents.clone(),
                });
            Ok(RemovePlanningFacts {
                repository: self.fixture.remove.repository.clone(),
                facts: self.fixture.remove.facts.clone(),
            })
        }
    }
    impl ManifestPlanningPort for PlanningSystem<'_> {
        fn plan_manifests(
            &self,
            _: &CreatePlanRequest,
            _: &CreatePlanningFacts,
            _: Vec<ManifestRuleSpec>,
        ) -> Result<Vec<crate::planner::FileActionManifest>, PlanningError> {
            self.fixture.trace.borrow_mut().manifests += 1;
            Ok(self.fixture.manifests.clone())
        }
    }

    fn plan(outcome: AppOutcome) -> crate::lifecycle::OperationPlan {
        match outcome.result.unwrap() {
            ResponseData::OperationPlan(plan) => plan,
            _ => panic!("not a plan"),
        }
    }
    fn digest(raw: &[u8]) -> String {
        Sha256::digest(raw)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
    fn create_plan_and_raw(
        fixture: &PlanningFixture,
    ) -> (crate::lifecycle::OperationPlan, Vec<u8>) {
        let plan = plan(fixture.run(Request::CreatePlan(fixture.create_request())));
        let raw = serde_json::to_vec(&plan).unwrap();
        (plan, raw)
    }
    fn prepare(
        fixture: &PlanningFixture,
        raw: &[u8],
    ) -> Result<crate::lifecycle::OperationPlan, PlanPreparationError> {
        Application {
            repository: &fixture.repository,
            system: &PlanningSystem { fixture },
        }
        .prepare_plan_bytes(raw, &digest(raw))
    }
    fn replace_once(raw: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
        let position = raw
            .windows(from.len())
            .position(|window| window == from)
            .unwrap();
        let mut changed = raw.to_vec();
        changed.splice(position..position + from.len(), to.iter().copied());
        changed
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TreeEntry {
        relative: PathBuf,
        kind: &'static str,
        bytes: Vec<u8>,
        #[cfg(unix)]
        mode: u32,
    }
    fn snapshot_tree(root: &Path) -> Vec<TreeEntry> {
        fn visit(root: &Path, current: &Path, output: &mut Vec<TreeEntry>) {
            let mut entries: Vec<_> = fs::read_dir(current)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                let relative = path.strip_prefix(root).unwrap().to_owned();
                #[cfg(unix)]
                let mode = std::os::unix::fs::MetadataExt::mode(&metadata);
                if metadata.is_dir() {
                    output.push(TreeEntry {
                        relative: relative.clone(),
                        kind: "dir",
                        bytes: Vec::new(),
                        #[cfg(unix)]
                        mode,
                    });
                    visit(root, &path, output);
                } else if metadata.is_file() {
                    output.push(TreeEntry {
                        relative,
                        kind: "file",
                        bytes: fs::read(&path).unwrap(),
                        #[cfg(unix)]
                        mode,
                    });
                } else if metadata.file_type().is_symlink() {
                    output.push(TreeEntry {
                        relative,
                        kind: "symlink",
                        bytes: fs::read_link(&path)
                            .unwrap()
                            .as_os_str()
                            .as_encoded_bytes()
                            .to_vec(),
                        #[cfg(unix)]
                        mode,
                    });
                }
            }
        }
        let mut output = Vec::new();
        visit(root, root, &mut output);
        output
    }

    #[test]
    fn pass_a_create_prepare_round_trip_preserves_id_and_grants() {
        let fixture = PlanningFixture::new();
        let original = plan(fixture.run(Request::CreatePlan(fixture.create_request())));
        assert!(!original.steps().is_empty());
        let raw = serde_json::to_vec(&original).unwrap();
        let returned = fixture.run(Request::CreatePlan(fixture.create_request()));
        let prepared = Application {
            repository: &fixture.repository,
            system: &PlanningSystem { fixture: &fixture },
        }
        .prepare_plan_bytes(&raw, &digest(&raw))
        .unwrap();
        assert_eq!(prepared, original);
        assert_eq!(prepared.operation_id(), original.operation_id());
        assert_eq!(prepared.granted_consents(), original.granted_consents());
        assert!(returned.is_success());
    }

    #[test]
    fn pass_a_remove_prepare_round_trip_preserves_destructive_intent() {
        let fixture = PlanningFixture::new();
        let original = plan(fixture.run(Request::RemovePlan(fixture.remove_request())));
        let raw = serde_json::to_vec(&original).unwrap();
        let prepared = Application {
            repository: &fixture.repository,
            system: &PlanningSystem { fixture: &fixture },
        }
        .prepare_plan_bytes(&raw, &digest(&raw))
        .unwrap();
        assert_eq!(prepared, original);
        let crate::lifecycle::OperationIntent::Remove(intent) = prepared.intent() else {
            panic!("wrong intent")
        };
        assert!(
            intent.allow_dirty_removal
                && intent.delete_local_branch
                && intent.force_delete_local_branch
        );
        assert!(intent.delete_remote_branch.is_some());
        assert_eq!(prepared.granted_consents(), original.granted_consents());
    }

    #[test]
    fn pass_a_ordinary_calls_allocate_distinct_fresh_ids() {
        let fixture = PlanningFixture::new();
        let a = plan(fixture.run(Request::CreatePlan(fixture.create_request())));
        let b = plan(fixture.run(Request::CreatePlan(fixture.create_request())));
        assert_ne!(a.operation_id(), b.operation_id());
        let a = plan(fixture.run(Request::RemovePlan(fixture.remove_request())));
        let b = plan(fixture.run(Request::RemovePlan(fixture.remove_request())));
        assert_ne!(a.operation_id(), b.operation_id());
    }

    #[test]
    fn pass_a_create_regeneration_records_persisted_anchors_not_common_dir() {
        let fixture = PlanningFixture::new();
        let original = plan(fixture.run(Request::CreatePlan(fixture.create_request())));
        let raw = serde_json::to_vec(&original).unwrap();
        let system = PlanningSystem { fixture: &fixture };
        Application {
            repository: &fixture.repository,
            system: &system,
        }
        .prepare_plan_bytes(&raw, &digest(&raw))
        .unwrap();
        let trace = fixture.trace.borrow();
        let snapshot = trace.creates.last().unwrap();
        assert_eq!(
            snapshot.repo,
            fixture.create.current_worktree_root.as_path()
        );
        assert_eq!(
            snapshot.invocation_cwd,
            fixture.create.current_worktree_root.as_path()
        );
        assert_eq!(
            snapshot.custom_path,
            Some(fixture.create.destination.path.as_path().to_owned())
        );
        assert_eq!(
            snapshot.source,
            format!("{:?}", fixture.create_request().source)
        );
        assert_eq!(
            snapshot.selected_tasks,
            fixture.create_request().selected_tasks
        );
        assert_eq!(
            snapshot.skipped_rules,
            fixture.create_request().skipped_rules
        );
        assert_eq!(snapshot.grants, fixture.create_request().granted_consents);
        assert_ne!(
            snapshot.repo,
            fixture.create.repository.common_dir.as_path()
        );
    }

    #[test]
    fn pass_a_remove_regeneration_records_primary_anchor_target_flags_and_grants() {
        let fixture = PlanningFixture::new();
        let original = plan(fixture.run(Request::RemovePlan(fixture.remove_request())));
        let raw = serde_json::to_vec(&original).unwrap();
        let system = PlanningSystem { fixture: &fixture };
        Application {
            repository: &fixture.repository,
            system: &system,
        }
        .prepare_plan_bytes(&raw, &digest(&raw))
        .unwrap();
        let trace = fixture.trace.borrow();
        let snapshot = trace.removes.last().unwrap();
        assert_eq!(
            snapshot.repo,
            fixture.remove.facts.repository.primary_root.as_path()
        );
        assert_eq!(
            snapshot.invocation_cwd,
            fixture.remove.facts.repository.primary_root.as_path()
        );
        assert_eq!(snapshot.target, fixture.remove.facts.path.as_path());
        assert!(
            snapshot.allow_dirty_removal
                && snapshot.delete_local_branch
                && snapshot.force_delete_local_branch
        );
        assert_eq!(
            snapshot.delete_remote_branch,
            fixture.remove.facts.remote_branch
        );
        assert_eq!(snapshot.grants, fixture.remove_request().granted_consents);
    }

    #[test]
    fn pass_a_return_is_decoded_canonical_plan_not_regenerated_substitute() {
        let fixture = PlanningFixture::new();
        let original = plan(fixture.run(Request::CreatePlan(fixture.create_request())));
        let raw = serde_json::to_vec(&original).unwrap();
        let confirmed = crate::plan_authority::confirm_plan(&raw, &digest(&raw)).unwrap();
        let system = PlanningSystem { fixture: &fixture };
        let returned = Application {
            repository: &fixture.repository,
            system: &system,
        }
        .prepare_plan_bytes(&raw, &digest(&raw))
        .unwrap();
        assert_eq!(confirmed.plan(), &returned);
        assert_eq!(
            serde_json::to_vec(confirmed.plan()).unwrap(),
            serde_json::to_vec(&returned).unwrap()
        );
        assert_eq!(confirmed.plan().operation_id(), returned.operation_id());
    }

    #[test]
    fn pass_a_preparation_errors_are_fixed_and_do_not_leak_fixture_secrets() {
        let fixture = PlanningFixture::new();
        let error = Application {
            repository: &fixture.repository,
            system: &PlanningSystem { fixture: &fixture },
        }
        .prepare_plan_bytes(b"fixture-secret", &"0".repeat(64))
        .unwrap_err();
        assert_eq!(error.code(), "plan_digest_mismatch");
        assert_eq!(error.to_string(), "plan_digest_mismatch");
        assert!(!error.to_string().contains("fixture-secret"));
    }

    #[test]
    fn pass_b_repository_and_source_facts_drift_mismatch_with_persisted_id() {
        let mut fixture = PlanningFixture::new();
        let (original, raw) = create_plan_and_raw(&fixture);
        let persisted_id = *original.operation_id();
        fixture.create.repository.repository_oid = id("1111111111111111111111111111111111111111");
        fixture.create.source_facts = CreateSourceFacts::NewBranch {
            branch: branch("feature/pass-a"),
            base_ref: reference("refs/heads/main"),
            base_oid: id("1212121212121212121212121212121212121212"),
            branch_absent: true,
        };
        let error = prepare(&fixture, &raw).unwrap_err();
        assert_eq!(error, PlanPreparationError::RegenerationMismatch);
        assert_eq!(error.code(), "plan_regeneration_mismatch");
        assert_eq!(persisted_id, original.operation_id().to_owned());
    }

    #[test]
    fn pass_b_task_drift_fails_or_mismatches_and_unselected_drift_is_irrelevant() {
        let mut fixture = PlanningFixture::new();
        let (_, raw) = create_plan_and_raw(&fixture);
        let contents = fixture.layers[0].contents.as_mut().unwrap();
        *contents = contents.replace("[\"build\"]", "[\"changed\"]");
        let error = prepare(&fixture, &raw).unwrap_err();
        assert!(matches!(
            error,
            PlanPreparationError::RegenerationMismatch | PlanPreparationError::RegenerationFailed
        ));
        assert!(matches!(
            error.code(),
            "plan_regeneration_mismatch" | "plan_regeneration_failed"
        ));
        assert!(matches!(
            error.code(),
            "plan_regeneration_mismatch" | "plan_regeneration_failed"
        ));

        let mut fixture = PlanningFixture::new();
        let (_, raw) = create_plan_and_raw(&fixture);
        let contents = fixture.layers[0].contents.as_mut().unwrap();
        *contents = contents.replace("[\"unused\"]", "[\"changed-unused\"]");
        assert_eq!(
            prepare(&fixture, &raw).unwrap(),
            crate::plan_authority::confirm_plan(&raw, &digest(&raw))
                .unwrap()
                .into_plan()
        );

        let mut fixture = PlanningFixture::new();
        let (_, raw) = create_plan_and_raw(&fixture);
        let contents = fixture.layers[0].contents.as_mut().unwrap();
        *contents = contents.replace("enabled = true", "enabled = false");
        let error = prepare(&fixture, &raw).unwrap_err();
        assert_eq!(error, PlanPreparationError::RegenerationFailed);
        assert_eq!(error.code(), "plan_regeneration_failed");
    }

    #[test]
    fn pass_b_file_rule_and_manifest_drift_refuses_without_changing_raw_authority() {
        let mut fixture = PlanningFixture::new();
        let (_, raw) = create_plan_and_raw(&fixture);
        let original_digest = digest(&raw);
        fixture.manifests[0].artifacts[0].digest = id("2323232323232323232323232323232323232323");
        let error = prepare(&fixture, &raw).unwrap_err();
        assert!(matches!(
            error,
            PlanPreparationError::RegenerationMismatch | PlanPreparationError::RegenerationFailed
        ));
        assert_eq!(digest(&raw), original_digest);

        let mut fixture = PlanningFixture::new();
        let (_, raw) = create_plan_and_raw(&fixture);
        let contents = fixture.layers[0].contents.as_mut().unwrap();
        *contents = contents.replace("enabled = true", "enabled = false");
        let error = prepare(&fixture, &raw).unwrap_err();
        assert!(matches!(
            error,
            PlanPreparationError::RegenerationMismatch | PlanPreparationError::RegenerationFailed
        ));

        let mut fixture = PlanningFixture::new();
        let (_, raw) = create_plan_and_raw(&fixture);
        fixture.layers[0]
            .contents
            .as_mut()
            .unwrap()
            .push_str("\n[git]\nremote = \"upstream\"\n");
        fixture.create.source = crate::lifecycle::CreateSource::RemoteTracking {
            remote: remote("upstream"),
            remote_branch: branch("feature/pass-a"),
            local_branch: branch("feature/pass-a"),
        };
        fixture.create.source_facts = CreateSourceFacts::RemoteTracking {
            remote: remote("upstream"),
            remote_branch: branch("feature/pass-a"),
            remote_oid: id("6767676767676767676767676767676767676767"),
            local_branch: branch("feature/pass-a"),
            local_absent: true,
        };
        let error = prepare(&fixture, &raw).unwrap_err();
        assert!(matches!(
            error,
            PlanPreparationError::RegenerationMismatch | PlanPreparationError::RegenerationFailed
        ));
    }

    #[test]
    fn pass_b_relevant_config_and_anchor_drift_refuse_irrelevant_layer_is_accepted() {
        let mut fixture = PlanningFixture::new();
        let (_, raw) = create_plan_and_raw(&fixture);
        fixture.create.destination.path = path(
            fixture
                .create
                .destination
                .path
                .as_path()
                .with_file_name("other-destination"),
        );
        let error = prepare(&fixture, &raw).unwrap_err();
        assert!(matches!(
            error,
            PlanPreparationError::RegenerationMismatch | PlanPreparationError::RegenerationFailed
        ));

        let mut fixture = PlanningFixture::new();
        let (_, raw) = create_plan_and_raw(&fixture);
        fixture.layers[0]
            .contents
            .as_mut()
            .unwrap()
            .push_str("\n[create]\nslug_max_bytes = 60\n");
        assert!(prepare(&fixture, &raw).is_ok());

        let mut fixture = PlanningFixture::new();
        let (_, raw) = create_plan_and_raw(&fixture);
        fixture.layers[0]
            .contents
            .as_mut()
            .unwrap()
            .push_str("\n[create]\ndefault_base = \"refs/heads/other\"\n");
        fixture.create.source = crate::lifecycle::CreateSource::NewBranch {
            branch: branch("feature/pass-a"),
            base: Some(reference("refs/heads/other")),
        };
        fixture.create.source_facts = CreateSourceFacts::NewBranch {
            branch: branch("feature/pass-a"),
            base_ref: reference("refs/heads/other"),
            base_oid: id("3434343434343434343434343434343434343434"),
            branch_absent: true,
        };
        let error = prepare(&fixture, &raw).unwrap_err();
        assert!(matches!(
            error,
            PlanPreparationError::RegenerationMismatch | PlanPreparationError::RegenerationFailed
        ));
    }

    #[test]
    fn pass_b_remove_target_class_oid_and_raw_flags_refuse() {
        let mut fixture = PlanningFixture::new();
        let original = plan(fixture.run(Request::RemovePlan(fixture.remove_request())));
        let raw = serde_json::to_vec(&original).unwrap();
        fixture.remove.facts.path = path(
            fixture
                .remove
                .facts
                .path
                .as_path()
                .with_file_name("other-target"),
        );
        let error = prepare(&fixture, &raw).unwrap_err();
        assert!(matches!(
            error,
            PlanPreparationError::RegenerationMismatch | PlanPreparationError::RegenerationFailed
        ));

        let mut fixture = PlanningFixture::new();
        let original = plan(fixture.run(Request::RemovePlan(fixture.remove_request())));
        let raw = serde_json::to_vec(&original).unwrap();
        fixture.remove.facts.class = crate::domain::WorktreeClass::Primary;
        assert!(matches!(
            prepare(&fixture, &raw),
            Err(PlanPreparationError::RegenerationFailed
                | PlanPreparationError::RegenerationMismatch)
        ));

        let mut fixture = PlanningFixture::new();
        let original = plan(fixture.run(Request::RemovePlan(fixture.remove_request())));
        let raw = serde_json::to_vec(&original).unwrap();
        fixture.remove.facts.worktree_oid = id("4545454545454545454545454545454545454545");
        assert!(matches!(
            prepare(&fixture, &raw),
            Err(PlanPreparationError::RegenerationFailed
                | PlanPreparationError::RegenerationMismatch)
        ));

        let raw = replace_once(
            &raw,
            b"\"allow_dirty_removal\":true",
            b"\"allow_dirty_removal\":false",
        );
        let error = prepare(&PlanningFixture::new(), &raw).unwrap_err();
        assert!(matches!(
            error,
            PlanPreparationError::RegenerationMismatch
                | PlanPreparationError::RegenerationFailed
                | PlanPreparationError::NotExecutable
        ));
        assert!(matches!(
            error.code(),
            "plan_regeneration_mismatch" | "plan_regeneration_failed" | "plan_not_executable"
        ));
    }

    #[test]
    fn pass_b_grant_mutation_is_not_added_by_application() {
        let fixture = PlanningFixture::new();
        let (_, raw) = create_plan_and_raw(&fixture);
        let changed = replace_once(&raw, b"[\"task:build\"]", b"[]");
        let error = prepare(&fixture, &changed).unwrap_err();
        assert_eq!(error, PlanPreparationError::NotExecutable);
        assert_eq!(error.code(), "plan_not_executable");
    }

    #[test]
    fn pass_b_preparation_errors_are_redacted_and_debug_is_stable() {
        let fixture = PlanningFixture::new();
        let error = prepare(&fixture, b"fixture-secret-source-bytes").unwrap_err();
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert_eq!(display, "plan_json_invalid");
        assert_eq!(error.code(), "plan_json_invalid");
        assert!(!display.contains("fixture-secret"));
        assert!(!debug.contains("fixture-secret"));
    }

    #[test]
    fn pass_b_success_and_mismatch_are_filesystem_read_only() {
        let fixture = PlanningFixture::new();
        let before = snapshot_tree(fixture.create.primary_root.as_path().parent().unwrap());
        let (_, raw) = create_plan_and_raw(&fixture);
        assert!(prepare(&fixture, &raw).is_ok());
        let after_success = snapshot_tree(fixture.create.primary_root.as_path().parent().unwrap());
        assert_eq!(before, after_success);

        let mut fixture = PlanningFixture::new();
        let before = snapshot_tree(fixture.create.primary_root.as_path().parent().unwrap());
        let (_, raw) = create_plan_and_raw(&fixture);
        fixture.create.repository.repository_oid = id("5656565656565656565656565656565656565656");
        assert!(prepare(&fixture, &raw).is_err());
        let after_failure = snapshot_tree(fixture.create.primary_root.as_path().parent().unwrap());
        assert_eq!(before, after_failure);
    }

    struct ApplyFiles {
        result: Result<Vec<u8>, PlanFileError>,
    }
    impl PlanFilePort for ApplyFiles {
        fn read_plan(&self, _: &Path) -> Result<Vec<u8>, PlanFileError> {
            self.result.clone()
        }
    }

    struct CountingForward {
        calls: Rc<RefCell<usize>>,
    }
    impl ForwardExecutionPort for CountingForward {
        fn execute(&self, prepared: PreparedApply) -> Result<ExecutionResult, ApplyError> {
            *self.calls.borrow_mut() += 1;
            Ok(ExecutionResult {
                operation_id: *prepared.plan().operation_id(),
                outcome: ExecutionOutcomeKind::Applied,
                step_id: None,
                detail: None,
                exit_override: None,
            })
        }
    }

    fn apply_service<'a>(
        fixture: &'a PlanningFixture,
        raw: Result<Vec<u8>, PlanFileError>,
        calls: &'a Rc<RefCell<usize>>,
    ) -> ApplyService<'a, FakeRepository, PlanningSystem<'a>, ApplyFiles, CountingForward> {
        let system = Box::leak(Box::new(PlanningSystem { fixture }));
        let application = Box::leak(Box::new(Application {
            repository: &fixture.repository,
            system,
        }));
        let files = Box::leak(Box::new(ApplyFiles { result: raw }));
        let forward = Box::leak(Box::new(CountingForward {
            calls: calls.clone(),
        }));
        ApplyService {
            planner: application,
            files,
            forward,
        }
    }

    #[test]
    fn apply_does_not_forward_invalid_or_drifted_inputs() {
        let cases = [
            (b"{}".to_vec(), "not-a-digest", "digest_invalid"),
            (
                b"{}".to_vec(),
                "0000000000000000000000000000000000000000000000000000000000000000",
                "digest_mismatch",
            ),
            (b"{".to_vec(), "", "json_invalid"),
            (br#"{"a":1,"a":2}"#.to_vec(), "", "json_invalid"),
        ];
        for (raw, expected, expected_error) in cases {
            let fixture = PlanningFixture::new();
            let calls = Rc::new(RefCell::new(0));
            let service = apply_service(&fixture, Ok(raw.clone()), &calls);
            let expected = if expected.is_empty() {
                digest(&raw)
            } else {
                expected.to_owned()
            };
            let error = service.apply(Path::new("plan"), &expected).unwrap_err();
            assert_eq!(error.code, format!("plan_{expected_error}"));
            assert_eq!(*calls.borrow(), 0);
        }

        let fixture = PlanningFixture::new();
        let (_, raw) = create_plan_and_raw(&fixture);
        let mut noncanonical = serde_json::from_slice::<serde_json::Value>(&raw).unwrap();
        noncanonical["intent"]["Create"]
            .as_object_mut()
            .unwrap()
            .remove("task_contracts");
        let raw_noncanonical = serde_json::to_vec(&noncanonical).unwrap();
        let calls = Rc::new(RefCell::new(0));
        let service = apply_service(&fixture, Ok(raw_noncanonical.clone()), &calls);
        assert_eq!(
            service
                .apply(Path::new("plan"), &digest(&raw_noncanonical))
                .unwrap_err()
                .code,
            "plan_noncanonical"
        );
        assert_eq!(*calls.borrow(), 0);

        let mut drifted = serde_json::from_slice::<serde_json::Value>(&raw).unwrap();
        drifted["intent"]["Create"]["destination"] = serde_json::json!(
            fixture
                .create
                .destination
                .path
                .as_path()
                .with_file_name("drifted")
        );
        let drifted = serde_json::to_vec(&drifted).unwrap();
        let calls = Rc::new(RefCell::new(0));
        let service = apply_service(&fixture, Ok(drifted.clone()), &calls);
        assert!(matches!(
            service
                .apply(Path::new("plan"), &digest(&drifted))
                .unwrap_err()
                .code,
            "plan_regeneration_mismatch" | "plan_not_executable"
        ));
        assert_eq!(*calls.borrow(), 0);

        let calls = Rc::new(RefCell::new(0));
        let service = apply_service(&fixture, Err(PlanFileError::Io), &calls);
        assert_eq!(
            service
                .apply(Path::new("plan"), &"0".repeat(64))
                .unwrap_err()
                .code,
            "plan_file_open"
        );
        assert_eq!(*calls.borrow(), 0);
    }

    #[test]
    fn apply_preparation_preserves_plan_digest_and_persisted_anchor() {
        let fixture = PlanningFixture::new();
        let (create_plan, create_raw) = create_plan_and_raw(&fixture);
        let calls = Rc::new(RefCell::new(0));
        let service = apply_service(&fixture, Ok(create_raw.clone()), &calls);
        let prepared = service
            .prepare(Path::new("plan"), &digest(&create_raw))
            .unwrap();
        assert_eq!(prepared.plan(), &create_plan);
        assert_eq!(prepared.raw_digest(), digest(&create_raw));
        assert_eq!(prepared.anchor(), &fixture.create.current_worktree_root);

        let fixture = PlanningFixture::new();
        let remove_plan = plan(fixture.run(Request::RemovePlan(fixture.remove_request())));
        let remove_raw = serde_json::to_vec(&remove_plan).unwrap();
        let calls = Rc::new(RefCell::new(0));
        let service = apply_service(&fixture, Ok(remove_raw.clone()), &calls);
        let prepared = service
            .prepare(Path::new("plan"), &digest(&remove_raw))
            .unwrap();
        assert_eq!(prepared.plan(), &remove_plan);
        assert_eq!(prepared.anchor(), &fixture.remove.repository.primary_root);
    }

    #[test]
    fn apply_error_distinctions_are_stable() {
        let fixture = PlanningFixture::new();
        let calls = Rc::new(RefCell::new(0));
        for (file_error, code) in [
            (PlanFileError::Missing, "plan_file_open"),
            (PlanFileError::InvalidPath, "plan_file_not_regular"),
            (PlanFileError::NotRegular, "plan_file_not_regular"),
            (PlanFileError::Io, "plan_file_open"),
            (PlanFileError::TooLarge, "plan_file_too_large"),
        ] {
            let service = apply_service(&fixture, Err(file_error), &calls);
            assert_eq!(
                service
                    .apply(Path::new("plan"), &"0".repeat(64))
                    .unwrap_err()
                    .code,
                code
            );
        }
    }
}
