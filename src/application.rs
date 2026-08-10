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
    ) -> Result<CreatePlanningFacts, PlanningError>;
    fn remove_facts(
        &self,
        request: &RemovePlanRequest,
    ) -> Result<RemovePlanningFacts, PlanningError>;
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
            Request::CreatePlan(request) => self.create_plan(request),
            Request::RemovePlan(request) => self.remove_plan(request),
            Request::RecoverList { repo } => self.recover_list(&repo),
            Request::RecoverShow { repo, operation_id } => self.recover_show(&repo, &operation_id),
        }
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

    fn create_plan(&self, request: CreatePlanRequest) -> AppOutcome {
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
            operation_id: crate::planner::new_operation_id(),
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

    fn remove_plan(&self, request: RemovePlanRequest) -> AppOutcome {
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
            operation_id: crate::planner::new_operation_id(),
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
    use std::{cell::RefCell, io, rc::Rc};

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
}
