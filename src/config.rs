use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config {path}: {message} at line {line:?}, column {column:?}")]
    Invalid {
        path: PathBuf,
        message: String,
        line: Option<usize>,
        column: Option<usize>,
    },
}

impl ConfigError {
    pub fn details(self) -> (PathBuf, Option<usize>, Option<usize>, String) {
        match self {
            Self::Invalid {
                path,
                message,
                line,
                column,
            } => (path, line, column, message),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectiveConfig {
    pub schema: u32,
    pub create: CreateConfig,
    pub git: GitConfig,
    pub file_rules: BTreeMap<String, FileRule>,
    pub tasks: BTreeMap<String, Task>,
    pub hooks: BTreeMap<String, Task>,
}

impl EffectiveConfig {
    pub fn empty() -> Self {
        Self {
            schema: 1,
            create: CreateConfig {
                default_base: None,
                slug_max_bytes: 60,
                worktree_root: None,
                directory_prefix: None,
            },
            git: GitConfig {
                remote: "origin".into(),
            },
            file_rules: BTreeMap::new(),
            tasks: BTreeMap::new(),
            hooks: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GitConfig {
    pub remote: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateConfig {
    pub default_base: Option<String>,
    pub slug_max_bytes: usize,
    pub worktree_root: Option<String>,
    pub directory_prefix: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    Path,
    Glob,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRuleKind {
    Copy,
    CopyTree,
    Symlink,
    Relink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    Fail,
    ReplaceSymlinkOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRoot {
    CurrentWorktree,
    PrimaryWorktree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    Manual,
    PreCreate,
    PostCreate,
    PreRemove,
    PostRemove,
    Sync,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelativePath(String);

impl RelativePath {
    pub fn new(value: String) -> Result<Self, String> {
        validate_relative(&value).map(|()| Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NonEmptyArgv(Vec<String>);

impl NonEmptyArgv {
    pub fn new(value: Vec<String>) -> Result<Self, String> {
        if value.is_empty() || value.iter().any(|part| part.trim().is_empty()) {
            Err("argv must be non-empty".into())
        } else {
            Ok(Self(value))
        }
    }
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentName(String);

impl EnvironmentName {
    pub fn new(value: String) -> Result<Self, String> {
        if value.is_empty()
            || !value.chars().enumerate().all(|(index, ch)| {
                if index == 0 {
                    ch == '_' || ch.is_ascii_alphabetic()
                } else {
                    ch == '_' || ch.is_ascii_alphanumeric()
                }
            })
        {
            Err(format!("invalid environment variable name: {value}"))
        } else {
            Ok(Self(value))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FileRule {
    pub match_mode: MatchMode,
    pub kind: FileRuleKind,
    pub source: RelativePath,
    pub destination: RelativePath,
    pub source_root: SourceRoot,
    pub on_conflict: ConflictPolicy,
    pub ignored_only: bool,
    pub excludes: Vec<RelativePath>,
    pub enabled: bool,
    pub sensitive: bool,
    pub confirm: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub phase: TaskPhase,
    pub argv: NonEmptyArgv,
    pub cwd: Option<RelativePath>,
    pub required: bool,
    pub environment_allowlist: Vec<EnvironmentName>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Provenance {
    pub scalars: BTreeMap<String, ProvenanceValue>,
    pub file_rules: BTreeMap<String, ProvenanceValue>,
    pub tasks: BTreeMap<String, ProvenanceValue>,
    pub hooks: BTreeMap<String, ProvenanceValue>,
}

#[derive(Debug, Clone)]
pub struct LayerContents {
    pub path: PathBuf,
    pub contents: Option<String>,
    pub source: LayerSource,
}

#[derive(Debug, Clone, Copy)]
pub enum LayerSource {
    User,
    Project,
    Local,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceValue {
    Defaults,
    User {
        #[serde(serialize_with = "crate::domain::serialize_path")]
        path: PathBuf,
    },
    Project {
        #[serde(serialize_with = "crate::domain::serialize_path")]
        path: PathBuf,
    },
    Local {
        #[serde(serialize_with = "crate::domain::serialize_path")]
        path: PathBuf,
    },
    Cli,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoadedConfig {
    pub config: EffectiveConfig,
    pub provenance: Provenance,
    #[serde(serialize_with = "serialize_paths")]
    pub layers: Vec<PathBuf>,
}

fn serialize_paths<S>(paths: &[PathBuf], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    paths
        .iter()
        .map(|path| crate::domain::PathDto::from(path.clone()))
        .collect::<Vec<_>>()
        .serialize(serializer)
}

pub struct ConfigLocations {
    pub user: Option<PathBuf>,
    pub project: PathBuf,
    pub local: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    pub slug_max_bytes: Option<usize>,
    pub worktree_root: Option<String>,
    pub directory_prefix: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    schema: Option<u32>,
    create: Option<RawCreate>,
    git: Option<RawGit>,
    #[serde(default)]
    file_rules: BTreeMap<String, RawFileRule>,
    #[serde(default)]
    tasks: BTreeMap<String, RawTask>,
    #[serde(default)]
    hooks: BTreeMap<String, RawTask>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawGit {
    remote: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawCreate {
    default_base: Option<String>,
    slug_max_bytes: Option<usize>,
    worktree_root: Option<String>,
    directory_prefix: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawFileRule {
    kind: Option<String>,
    match_mode: Option<String>,
    source: Option<String>,
    destination: Option<String>,
    on_conflict: Option<String>,
    source_root: Option<String>,
    ignored_only: Option<bool>,
    enabled: Option<bool>,
    sensitive: Option<bool>,
    confirm: Option<bool>,
    excludes: Option<Vec<String>>,
    delete: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawTask {
    phase: Option<String>,
    argv: Option<Vec<String>>,
    cwd: Option<String>,
    required: Option<bool>,
    environment_allowlist: Option<Vec<String>>,
    enabled: Option<bool>,
    delete: Option<bool>,
}

pub fn load_layers(
    layers: &[LayerContents],
    overrides: &ConfigOverrides,
) -> Result<LoadedConfig, ConfigError> {
    let mut state = State::default();
    state.apply_defaults();
    let mut loaded_layers = Vec::new();
    for layer in layers {
        let provenance = match layer.source {
            LayerSource::User => ProvenanceValue::User {
                path: layer.path.clone(),
            },
            LayerSource::Project => ProvenanceValue::Project {
                path: layer.path.clone(),
            },
            LayerSource::Local => ProvenanceValue::Local {
                path: layer.path.clone(),
            },
        };
        if let Some(source) = &layer.contents {
            apply_source(
                &mut state,
                &layer.path,
                source,
                provenance,
                &mut loaded_layers,
            )?;
        }
    }
    state.apply_overrides(overrides)?;
    state.validate()?;
    Ok(LoadedConfig {
        config: state.config,
        provenance: state.provenance,
        layers: loaded_layers,
    })
}

fn apply_source(
    state: &mut State,
    path: &Path,
    source: &str,
    provenance: ProvenanceValue,
    layers: &mut Vec<PathBuf>,
) -> Result<(), ConfigError> {
    let raw: RawConfig = toml::from_str(source).map_err(|error| toml_error(path, source, error))?;
    validate_raw(&raw, path, source)?;
    state.apply(raw, path, provenance)?;
    layers.push(path.to_owned());
    Ok(())
}

fn toml_error(path: &Path, source: &str, error: toml::de::Error) -> ConfigError {
    let (line, column) = error
        .span()
        .map(|span| {
            let prefix = &source[..span.start.min(source.len())];
            let line = prefix.lines().count().max(1);
            let column = prefix.rsplit('\n').next().map_or(1, |line| line.len() + 1);
            (line, column)
        })
        .map_or((None, None), |(line, column)| (Some(line), Some(column)));
    ConfigError::Invalid {
        path: path.to_owned(),
        message: error.to_string(),
        line,
        column,
    }
}

fn invalid(path: &Path, message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        path: path.to_owned(),
        message: message.into(),
        line: None,
        column: None,
    }
}

fn validate_raw(raw: &RawConfig, path: &Path, source: &str) -> Result<(), ConfigError> {
    if raw.schema != Some(1) {
        return Err(invalid(path, "schema must be 1"));
    }
    if let Some(create) = &raw.create {
        if let Some(cap) = create.slug_max_bytes
            && cap < 8
        {
            return Err(invalid(path, "create.slug_max_bytes must be at least 8"));
        }
        if let Some(root) = &create.worktree_root {
            validate_root(root).map_err(|message| invalid(path, message))?;
        }
        if let Some(prefix) = &create.directory_prefix {
            validate_prefix(prefix).map_err(|message| invalid(path, message))?;
        }
    }
    if let Some(git) = &raw.git
        && let Some(remote) = &git.remote
        && (remote.trim().is_empty() || remote.chars().any(|c| c.is_control() || c == '\0'))
    {
        return Err(invalid(
            path,
            "git.remote must be a nonempty name without controls",
        ));
    }
    for (name, rule) in &raw.file_rules {
        validate_name(name, path)?;
        validate_delete(rule.delete, rule_has_fields(rule), path)?;
        if rule.delete == Some(true) {
            continue;
        }
        let kind = rule
            .kind
            .as_deref()
            .ok_or_else(|| invalid(path, format!("file rule {name} needs kind")))?;
        if !["copy", "copy_tree", "symlink", "relink"].contains(&kind) {
            return Err(invalid(path, format!("file rule {name} has invalid kind")));
        }
        let source = rule
            .source
            .as_deref()
            .ok_or_else(|| invalid(path, format!("file rule {name} needs source")))?;
        let mode = rule.match_mode.as_deref().unwrap_or("path");
        if !["path", "glob"].contains(&mode) {
            return Err(invalid(
                path,
                format!("file rule {name} has invalid match_mode"),
            ));
        }
        let destination = rule
            .destination
            .as_deref()
            .ok_or_else(|| invalid(path, format!("file rule {name} needs destination")))?;
        for value in [source, destination] {
            validate_relative(value).map_err(|message| invalid(path, message))?;
        }
        if !["fail", "replace_symlink_only"]
            .contains(&rule.on_conflict.as_deref().unwrap_or("fail"))
        {
            return Err(invalid(path, "invalid conflict policy"));
        }
        let conflict = rule.on_conflict.as_deref().unwrap_or("fail");
        match kind {
            "copy" | "copy_tree" if conflict != "fail" => {
                return Err(invalid(
                    path,
                    format!("file rule {name}: copy rules only support fail conflict policy"),
                ));
            }
            "symlink" if conflict != "fail" => {
                return Err(invalid(
                    path,
                    format!("file rule {name}: symlink rules only support fail conflict policy"),
                ));
            }
            "relink" if conflict != "replace_symlink_only" => {
                return Err(invalid(
                    path,
                    format!("file rule {name}: relink requires replace_symlink_only"),
                ));
            }
            _ => {}
        }
        if let Some(source_root) = &rule.source_root
            && !["current_worktree", "primary_worktree"].contains(&source_root.as_str())
        {
            return Err(invalid(path, "invalid source_root"));
        }
        for exclude in rule.excludes.as_deref().unwrap_or(&[]) {
            validate_relative(exclude).map_err(|message| invalid(path, message))?;
        }
        if rule.sensitive == Some(true) && rule.confirm != Some(true) {
            return Err(invalid(
                path,
                format!("sensitive file rule {name} requires confirm = true"),
            ));
        }
    }
    for (name, task) in raw.tasks.iter().chain(raw.hooks.iter()) {
        validate_name(name, path)?;
        validate_delete(task.delete, task_has_fields(task), path)?;
        if task.delete == Some(true) {
            continue;
        }
        let argv = task
            .argv
            .as_ref()
            .ok_or_else(|| invalid(path, format!("task {name} needs non-empty argv")))?;
        if argv.is_empty() || argv.iter().any(|part| part.trim().is_empty()) {
            return Err(invalid(path, format!("task {name} needs non-empty argv")));
        }
        if let Some(cwd) = &task.cwd {
            validate_relative(cwd).map_err(|message| invalid(path, message))?;
        }
        if ![
            "manual",
            "pre_create",
            "post_create",
            "pre_remove",
            "post_remove",
            "sync",
        ]
        .contains(&task.phase.as_deref().unwrap_or(""))
        {
            return Err(invalid(path, format!("task {name} has invalid phase")));
        }
        for name in task.environment_allowlist.as_deref().unwrap_or(&[]) {
            EnvironmentName::new(name.clone()).map_err(|message| invalid(path, message))?;
        }
    }
    let _ = source;
    Ok(())
}

fn validate_name(name: &str, path: &Path) -> Result<(), ConfigError> {
    if name.trim().is_empty() {
        Err(invalid(path, "named entry has an empty name"))
    } else {
        Ok(())
    }
}

fn validate_delete(delete: Option<bool>, has_fields: bool, path: &Path) -> Result<(), ConfigError> {
    if delete == Some(true) && has_fields {
        Err(invalid(
            path,
            "delete entry cannot contain operational fields",
        ))
    } else {
        Ok(())
    }
}

fn rule_has_fields(rule: &RawFileRule) -> bool {
    rule.kind.is_some()
        || rule.source.is_some()
        || rule.match_mode.is_some()
        || rule.destination.is_some()
        || rule.on_conflict.is_some()
        || rule.source_root.is_some()
        || rule.ignored_only.is_some()
        || rule.enabled.is_some()
        || rule.sensitive.is_some()
        || rule.confirm.is_some()
        || rule.excludes.is_some()
}

fn task_has_fields(task: &RawTask) -> bool {
    task.phase.is_some()
        || task.argv.is_some()
        || task.cwd.is_some()
        || task.required.is_some()
        || task.environment_allowlist.is_some()
        || task.enabled.is_some()
}

fn validate_relative(value: &str) -> Result<(), String> {
    let path = Path::new(value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        Err(format!("path is not relative and safe: {value}"))
    } else {
        Ok(())
    }
}

fn validate_root(value: &str) -> Result<(), String> {
    if value.is_empty() || value.contains('\0') {
        return Err("worktree_root must be nonempty and contain no NUL".into());
    }
    if Path::new(value).is_absolute() {
        return Ok(());
    }
    Ok(())
}

fn validate_prefix(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.contains('\0')
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
    {
        Err(format!(
            "directory_prefix is not a safe filename component: {value}"
        ))
    } else {
        Ok(())
    }
}

struct State {
    config: EffectiveConfig,
    provenance: Provenance,
}

impl Default for State {
    fn default() -> Self {
        Self {
            config: EffectiveConfig::empty(),
            provenance: Provenance {
                scalars: BTreeMap::new(),
                file_rules: BTreeMap::new(),
                tasks: BTreeMap::new(),
                hooks: BTreeMap::new(),
            },
        }
    }
}

impl State {
    fn apply_defaults(&mut self) {
        self.config.schema = 1;
        self.provenance
            .scalars
            .insert("schema".into(), ProvenanceValue::Defaults);
        self.provenance
            .scalars
            .insert("create.slug_max_bytes".into(), ProvenanceValue::Defaults);
        self.provenance
            .scalars
            .insert("create.default_base".into(), ProvenanceValue::Defaults);
        self.provenance
            .scalars
            .insert("git.remote".into(), ProvenanceValue::Defaults);
        self.provenance
            .scalars
            .insert("create.worktree_root".into(), ProvenanceValue::Defaults);
        self.provenance
            .scalars
            .insert("create.directory_prefix".into(), ProvenanceValue::Defaults);
    }
    fn apply(
        &mut self,
        raw: RawConfig,
        path: &Path,
        provenance: ProvenanceValue,
    ) -> Result<(), ConfigError> {
        if let Some(create) = raw.create {
            if create.default_base.is_some() {
                self.config.create.default_base = create.default_base;
                self.provenance
                    .scalars
                    .insert("create.default_base".into(), provenance.clone());
            }
            if let Some(slug_max_bytes) = create.slug_max_bytes {
                self.config.create.slug_max_bytes = slug_max_bytes;
                self.provenance
                    .scalars
                    .insert("create.slug_max_bytes".into(), provenance.clone());
            }
            if create.worktree_root.is_some() {
                self.config.create.worktree_root = create.worktree_root;
                self.provenance
                    .scalars
                    .insert("create.worktree_root".into(), provenance.clone());
            }
            if create.directory_prefix.is_some() {
                self.config.create.directory_prefix = create.directory_prefix;
                self.provenance
                    .scalars
                    .insert("create.directory_prefix".into(), provenance.clone());
            }
        }
        if let Some(git) = raw.git
            && let Some(remote) = git.remote
        {
            self.config.git.remote = remote;
            self.provenance
                .scalars
                .insert("git.remote".into(), provenance.clone());
        }
        apply_rules(
            &mut self.config.file_rules,
            raw.file_rules,
            provenance.clone(),
            &mut self.provenance.file_rules,
            path,
        )?;
        apply_tasks(
            &mut self.config.tasks,
            raw.tasks,
            provenance.clone(),
            &mut self.provenance.tasks,
            path,
        )?;
        apply_tasks(
            &mut self.config.hooks,
            raw.hooks,
            provenance,
            &mut self.provenance.hooks,
            path,
        )?;
        Ok(())
    }
    fn apply_overrides(&mut self, overrides: &ConfigOverrides) -> Result<(), ConfigError> {
        if let Some(value) = overrides.slug_max_bytes
            && value < 8
        {
            return Err(invalid(
                Path::new("<cli>"),
                "slug_max_bytes must be at least 8",
            ));
        }
        if let Some(value) = &overrides.worktree_root {
            validate_root(value).map_err(|m| invalid(Path::new("<cli>"), m))?;
        }
        if let Some(value) = &overrides.directory_prefix {
            validate_prefix(value).map_err(|m| invalid(Path::new("<cli>"), m))?;
        }
        if let Some(value) = overrides.slug_max_bytes {
            self.config.create.slug_max_bytes = value;
            self.provenance
                .scalars
                .insert("create.slug_max_bytes".into(), ProvenanceValue::Cli);
        }
        if overrides.worktree_root.is_some() {
            self.config.create.worktree_root = overrides.worktree_root.clone();
            self.provenance
                .scalars
                .insert("create.worktree_root".into(), ProvenanceValue::Cli);
        }
        if overrides.directory_prefix.is_some() {
            self.config.create.directory_prefix = overrides.directory_prefix.clone();
            self.provenance
                .scalars
                .insert("create.directory_prefix".into(), ProvenanceValue::Cli);
        }
        Ok(())
    }
    fn validate(&self) -> Result<(), ConfigError> {
        Ok(())
    }
}

fn apply_rules(
    target: &mut BTreeMap<String, FileRule>,
    entries: BTreeMap<String, RawFileRule>,
    source: ProvenanceValue,
    provenance: &mut BTreeMap<String, ProvenanceValue>,
    path: &Path,
) -> Result<(), ConfigError> {
    for (name, raw) in entries {
        if raw.delete == Some(true) {
            target.remove(&name);
            provenance.remove(&name);
        } else {
            target.insert(
                name.clone(),
                FileRule {
                    match_mode: parse_match_mode(raw.match_mode.as_deref().unwrap_or("path"))
                        .unwrap(),
                    kind: parse_kind(&raw.kind.unwrap()).unwrap(),
                    source: RelativePath::new(raw.source.unwrap()).unwrap(),
                    destination: RelativePath::new(raw.destination.unwrap()).unwrap(),
                    source_root: parse_source_root(
                        raw.source_root.as_deref().unwrap_or("current_worktree"),
                    )
                    .unwrap(),
                    on_conflict: parse_conflict(raw.on_conflict.as_deref().unwrap_or("fail"))
                        .unwrap(),
                    ignored_only: raw.ignored_only.unwrap_or(false),
                    excludes: raw
                        .excludes
                        .unwrap_or_default()
                        .into_iter()
                        .map(|value| RelativePath::new(value).unwrap())
                        .collect(),
                    enabled: raw.enabled.unwrap_or(true),
                    sensitive: raw.sensitive.unwrap_or(false),
                    confirm: raw.confirm.unwrap_or(false),
                },
            );
            provenance.insert(name, source.clone());
        }
    }
    let _ = path;
    Ok(())
}

fn parse_kind(value: &str) -> Result<FileRuleKind, String> {
    match value {
        "copy" => Ok(FileRuleKind::Copy),
        "copy_tree" => Ok(FileRuleKind::CopyTree),
        "symlink" => Ok(FileRuleKind::Symlink),
        "relink" => Ok(FileRuleKind::Relink),
        _ => Err("invalid file rule kind".into()),
    }
}

fn parse_match_mode(value: &str) -> Result<MatchMode, String> {
    match value {
        "path" => Ok(MatchMode::Path),
        "glob" => Ok(MatchMode::Glob),
        _ => Err("invalid match mode".into()),
    }
}

fn parse_conflict(value: &str) -> Result<ConflictPolicy, String> {
    match value {
        "fail" => Ok(ConflictPolicy::Fail),
        "replace_symlink_only" => Ok(ConflictPolicy::ReplaceSymlinkOnly),
        _ => Err("invalid conflict policy".into()),
    }
}

fn parse_source_root(value: &str) -> Result<SourceRoot, String> {
    match value {
        "current_worktree" => Ok(SourceRoot::CurrentWorktree),
        "primary_worktree" => Ok(SourceRoot::PrimaryWorktree),
        _ => Err("invalid source root".into()),
    }
}

fn parse_phase(value: &str) -> Result<TaskPhase, String> {
    match value {
        "manual" => Ok(TaskPhase::Manual),
        "pre_create" => Ok(TaskPhase::PreCreate),
        "post_create" => Ok(TaskPhase::PostCreate),
        "pre_remove" => Ok(TaskPhase::PreRemove),
        "post_remove" => Ok(TaskPhase::PostRemove),
        "sync" => Ok(TaskPhase::Sync),
        _ => Err("invalid task phase".into()),
    }
}

fn apply_tasks(
    target: &mut BTreeMap<String, Task>,
    entries: BTreeMap<String, RawTask>,
    source: ProvenanceValue,
    provenance: &mut BTreeMap<String, ProvenanceValue>,
    path: &Path,
) -> Result<(), ConfigError> {
    for (name, raw) in entries {
        if raw.delete == Some(true) {
            target.remove(&name);
            provenance.remove(&name);
        } else {
            target.insert(
                name.clone(),
                Task {
                    phase: parse_phase(raw.phase.as_deref().unwrap()).unwrap(),
                    argv: NonEmptyArgv::new(raw.argv.unwrap()).unwrap(),
                    cwd: raw.cwd.map(|value| RelativePath::new(value).unwrap()),
                    required: raw.required.unwrap_or(false),
                    environment_allowlist: raw
                        .environment_allowlist
                        .unwrap_or_default()
                        .into_iter()
                        .map(|value| EnvironmentName::new(value).unwrap())
                        .collect(),
                    enabled: raw.enabled.unwrap_or(true),
                },
            );
            provenance.insert(name, source.clone());
        }
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_rejects_unknown_and_unsafe_values() {
        let unknown: Result<RawConfig, _> = toml::from_str("schema = 1\nunknown = true\n");
        assert!(unknown.is_err());
        let raw: RawConfig =
            toml::from_str("schema = 1\n[file_rules.x]\nkind = \"copy\"\nsource = \"../secret\"\n")
                .unwrap();
        assert!(validate_raw(&raw, Path::new("project/.ewtm.toml"), "").is_err());
    }

    #[test]
    fn named_entries_replace_and_delete_deterministically() {
        let mut state = State::default();
        state.apply_defaults();
        let first: RawConfig =
            toml::from_str("schema = 1\n[tasks.test]\nphase = \"manual\"\nargv = [\"one\"]\n")
                .unwrap();
        validate_raw(&first, Path::new("a"), "").unwrap();
        state
            .apply(
                first,
                Path::new("a"),
                ProvenanceValue::Project {
                    path: PathBuf::from("a"),
                },
            )
            .unwrap();
        let second: RawConfig =
            toml::from_str("schema = 1\n[tasks.test]\ndelete = true\n").unwrap();
        validate_raw(&second, Path::new("b"), "").unwrap();
        state
            .apply(
                second,
                Path::new("b"),
                ProvenanceValue::Local {
                    path: PathBuf::from("b"),
                },
            )
            .unwrap();
        assert!(state.config.tasks.is_empty());
    }

    #[test]
    fn malformed_toml_reports_source_location() {
        let error = toml_error(
            Path::new("config.toml"),
            "schema = [",
            toml::from_str::<RawConfig>("schema = [").unwrap_err(),
        );
        let text = error.to_string();
        assert!(text.contains("config.toml"));
        assert!(text.contains("line"));
    }

    #[test]
    fn pure_layers_apply_precedence_replacement_delete_and_cli_provenance() {
        let layers = vec![
            LayerContents { path: "defaults".into(), source: LayerSource::User, contents: Some("schema = 1\n[create]\nslug_max_bytes = 8\n[tasks.build]\nphase = \"manual\"\nargv = [\"one\"]\n".into()) },
            LayerContents { path: "user".into(), source: LayerSource::User, contents: Some("schema = 1\n[tasks.build]\nphase = \"post_create\"\nargv = [\"two\"]\n".into()) },
            LayerContents { path: "project".into(), source: LayerSource::Project, contents: Some("schema = 1\n[tasks.build]\ndelete = true\n[tasks.test]\nphase = \"manual\"\nargv = [\"test\"]\n".into()) },
            LayerContents { path: "local".into(), source: LayerSource::Local, contents: Some("schema = 1\n[tasks.test]\nenabled = false\nphase = \"manual\"\nargv = [\"local\"]\n".into()) },
        ];
        let loaded = load_layers(
            &layers,
            &ConfigOverrides {
                slug_max_bytes: Some(60),
                worktree_root: Some("../worktrees".into()),
                directory_prefix: Some("repo-".into()),
            },
        )
        .unwrap();
        assert!(!loaded.config.tasks.contains_key("build"));
        assert!(!loaded.config.tasks["test"].enabled);
        assert_eq!(loaded.config.create.slug_max_bytes, 60);
        assert_eq!(
            loaded.config.create.worktree_root.as_deref(),
            Some("../worktrees")
        );
        assert!(matches!(
            loaded.provenance.scalars["create.slug_max_bytes"],
            ProvenanceValue::Cli
        ));
        assert_eq!(loaded.layers.len(), 4);
    }

    #[test]
    fn lower_unknown_field_fails_before_upper_override() {
        let layers = vec![
            LayerContents {
                path: "lower".into(),
                source: LayerSource::User,
                contents: Some("schema = 1\n[create]\nunknown = 1\n".into()),
            },
            LayerContents {
                path: "upper".into(),
                source: LayerSource::Project,
                contents: Some("schema = 1\n[create]\nslug_max_bytes = 60\n".into()),
            },
        ];
        assert!(load_layers(&layers, &ConfigOverrides::default()).is_err());
    }

    #[test]
    fn validation_keeps_root_sibling_relative_but_rejects_unsafe_fields() {
        let valid = LayerContents { path: "valid".into(), source: LayerSource::Project, contents: Some("schema = 1\n[create]\nworktree_root = \"../../sibling\"\ndirectory_prefix = \"repo-\"\n".into()) };
        assert!(load_layers(&[valid], &ConfigOverrides::default()).is_ok());
        let invalid = LayerContents {
            path: "invalid".into(),
            source: LayerSource::Project,
            contents: Some("schema = 1\n[create]\ndirectory_prefix = \"../bad\"\n".into()),
        };
        assert!(load_layers(&[invalid], &ConfigOverrides::default()).is_err());
    }

    #[test]
    fn m2_defaults_and_match_mode_are_preserved() {
        let loaded = load_layers(&[LayerContents { path: "config".into(), source: LayerSource::Project, contents: Some("schema = 1\n[git]\nremote = \"upstream\"\n[file_rules.env]\nkind = \"copy\"\nmatch_mode = \"glob\"\nsource = \"**/.env*\"\ndestination = \".\"\n".into()) }], &ConfigOverrides::default()).unwrap();
        assert_eq!(loaded.config.git.remote, "upstream");
        assert_eq!(loaded.config.file_rules["env"].match_mode, MatchMode::Glob);
        let defaults = load_layers(&[], &ConfigOverrides::default()).unwrap();
        assert_eq!(defaults.config.git.remote, "origin");
        assert_eq!(defaults.config.create.default_base, None);
    }

    #[test]
    fn m2_conflict_policies_are_kind_specific() {
        let copy_replace = "schema = 1\n[file_rules.x]\nkind = \"copy\"\nsource = \"a\"\ndestination = \"a\"\non_conflict = \"replace_symlink_only\"\n";
        assert!(
            load_layers(
                &[LayerContents {
                    path: "config".into(),
                    source: LayerSource::Project,
                    contents: Some(copy_replace.into())
                }],
                &ConfigOverrides::default()
            )
            .is_err()
        );
        let relink_fail =
            "schema = 1\n[file_rules.x]\nkind = \"relink\"\nsource = \"a\"\ndestination = \"a\"\n";
        assert!(
            load_layers(
                &[LayerContents {
                    path: "config".into(),
                    source: LayerSource::Project,
                    contents: Some(relink_fail.into())
                }],
                &ConfigOverrides::default()
            )
            .is_err()
        );
    }
}
