use crate::{
    journal::{Journal, OperationStatus, Reconciliation},
    journal_store::{JournalError, LockedJournalStore},
    lifecycle::{OperationPlan, PlanStep, Precondition},
};
use std::{error::Error, fmt, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConditionResult {
    Satisfied,
    Unsatisfied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeCapability {
    Deterministic,
    UnknownAfterCrash,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeContext {
    AfterAttempt { executor_succeeded: bool },
    StartupReconciliation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    Applied,
    NotApplied,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionOutcome {
    Applied {
        operation_id: crate::lifecycle::OperationId,
    },
    AlreadyApplied {
        operation_id: crate::lifecycle::OperationId,
    },
    PreflightRefused {
        operation_id: crate::lifecycle::OperationId,
        condition: Precondition,
    },
    Paused {
        operation_id: crate::lifecycle::OperationId,
        step_id: crate::lifecycle::StepId,
        condition: Precondition,
    },
    NeedsAttention {
        operation_id: crate::lifecycle::OperationId,
        step_id: crate::lifecycle::StepId,
    },
    ExistingOperation {
        operation_id: crate::lifecycle::OperationId,
        status: OperationStatus,
    },
}

#[derive(Debug)]
pub enum ExecutionError<E> {
    Backend(E),
    Journal(JournalError),
    UnsupportedPlan(String),
    MissingConsent(crate::lifecycle::ConsentId),
    RepositoryIdentityMismatch,
    ImmutableCollision,
}
impl<E: fmt::Display> fmt::Display for ExecutionError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(error) => write!(f, "backend error: {error}"),
            Self::Journal(error) => error.fmt(f),
            Self::UnsupportedPlan(message) => write!(f, "unsupported plan: {message}"),
            Self::MissingConsent(id) => write!(f, "required consent is missing: {id}"),
            Self::RepositoryIdentityMismatch => f.write_str("repository identity mismatch"),
            Self::ImmutableCollision => f.write_str("operation id has an immutable plan collision"),
        }
    }
}
impl<E: Error + 'static> Error for ExecutionError<E> {}
impl<E> From<JournalError> for ExecutionError<E> {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

pub trait ExecutionBackend {
    type Error: Error + Send + Sync + 'static;
    type Repository;

    fn discover_repository(&mut self) -> Result<Self::Repository, Self::Error>;
    fn repository_common_dir<'a>(&self, repository: &'a Self::Repository) -> &'a Path;
    fn repository_matches_plan(&self, repository: &Self::Repository, plan: &OperationPlan) -> bool;
    fn supports_precondition(
        &self,
        plan: &OperationPlan,
        step: Option<&PlanStep>,
        phase: ConditionPhase,
        precondition: &Precondition,
    ) -> bool;
    fn supports_action(&self, context: &StepExecutionContext<'_>) -> bool;
    fn probe_capability(&self, context: &StepExecutionContext<'_>) -> ProbeCapability;
    fn check_precondition(
        &mut self,
        plan: &OperationPlan,
        step: Option<&PlanStep>,
        phase: ConditionPhase,
        precondition: &Precondition,
    ) -> Result<ConditionResult, Self::Error>;
    fn invoke(&mut self, context: &StepExecutionContext<'_>) -> Result<(), Self::Error>;
    fn probe(
        &mut self,
        context: &StepExecutionContext<'_>,
        context: ProbeContext,
    ) -> Result<ProbeVerdict, Self::Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionPhase {
    InitialPreflight,
    BeforeInvoke,
}

pub struct StepExecutionContext<'a> {
    plan: &'a OperationPlan,
    step: &'a PlanStep,
}

impl<'a> StepExecutionContext<'a> {
    fn for_step(plan: &'a OperationPlan, step_id: &crate::lifecycle::StepId) -> Option<Self> {
        plan.steps()
            .iter()
            .find(|candidate| candidate.id() == step_id)
            .map(|step| Self { plan, step })
    }
    #[cfg(test)]
    pub(crate) fn new(plan: &'a OperationPlan, step: &'a PlanStep) -> Self {
        Self::for_step(plan, step.id()).expect("test step must belong to the plan")
    }
    pub fn plan(&self) -> &'a OperationPlan {
        self.plan
    }
    pub fn operation_id(&self) -> &crate::lifecycle::OperationId {
        self.plan.operation_id()
    }
    pub fn step(&self) -> &'a PlanStep {
        self.step
    }
}

pub struct ExecutionEngine<B> {
    backend: B,
}
impl<B> ExecutionEngine<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }
}

impl<B: ExecutionBackend> ExecutionEngine<B> {
    fn context_for_step<'a>(
        &self,
        plan: &'a OperationPlan,
        step: &'a PlanStep,
    ) -> Result<StepExecutionContext<'a>, ExecutionError<B::Error>> {
        StepExecutionContext::for_step(plan, step.id()).ok_or_else(|| {
            ExecutionError::UnsupportedPlan("step is not a member of its operation plan".into())
        })
    }
    pub fn execute(
        &mut self,
        plan: OperationPlan,
    ) -> Result<ExecutionOutcome, ExecutionError<B::Error>> {
        plan.validate_executable_plan()
            .map_err(ExecutionError::UnsupportedPlan)?;
        if let Some(consent) = plan
            .required_consents()
            .iter()
            .find(|consent| !plan.granted_consents().contains(&consent.id))
        {
            return Err(ExecutionError::MissingConsent(consent.id.clone()));
        }
        self.scan_support(&plan)?;
        let discovered = self
            .backend
            .discover_repository()
            .map_err(ExecutionError::Backend)?;
        if !self.backend.repository_matches_plan(&discovered, &plan) {
            return Err(ExecutionError::RepositoryIdentityMismatch);
        }
        let common = self.backend.repository_common_dir(&discovered).to_owned();
        let mut store = LockedJournalStore::acquire(&common)?;
        let rediscovered = self
            .backend
            .discover_repository()
            .map_err(ExecutionError::Backend)?;
        if !self.backend.repository_matches_plan(&rediscovered, &plan) {
            return Err(ExecutionError::RepositoryIdentityMismatch);
        }
        let journals = store.list()?;
        self.reconcile_running(&mut store, &journals, &rediscovered)?;
        let journals = store.list()?;
        if let Some(outcome) = self.classify_existing(&journals, &plan)? {
            return Ok(outcome);
        }
        if let Some(condition) = self.preflight(&plan)? {
            return Ok(ExecutionOutcome::PreflightRefused {
                operation_id: *plan.operation_id(),
                condition,
            });
        }
        let mut journal = Journal::new(plan.clone());
        store.write_new(&journal)?;
        for step in plan.steps() {
            if let Some(condition) = self.check_all(
                &plan,
                Some(step),
                ConditionPhase::BeforeInvoke,
                step.preconditions(),
            )? {
                return Ok(ExecutionOutcome::Paused {
                    operation_id: *plan.operation_id(),
                    step_id: step.id().clone(),
                    condition,
                });
            }
            let previous = journal.clone();
            journal
                .start_step(step.id())
                .map_err(|_| ExecutionError::Journal(JournalError::InvalidTransition))?;
            store.update(&previous, &journal)?;
            let context = self.context_for_step(&plan, step)?;
            let effect_error = self.backend.invoke(&context);
            let probe = self.backend.probe(
                &context,
                ProbeContext::AfterAttempt {
                    executor_succeeded: effect_error.is_ok(),
                },
            );
            let verdict = match probe {
                Ok(value) => value,
                Err(_error) => {
                    self.persist_attention(&mut store, &mut journal)?;
                    return Ok(ExecutionOutcome::NeedsAttention {
                        operation_id: *plan.operation_id(),
                        step_id: step.id().clone(),
                    });
                }
            };
            match verdict {
                ProbeVerdict::Applied => {
                    let previous = journal.clone();
                    journal
                        .reconcile_step(step.id(), Reconciliation::Applied)
                        .map_err(|_| ExecutionError::Journal(JournalError::InvalidTransition))?;
                    store.update(&previous, &journal)?;
                    let _ = effect_error;
                }
                ProbeVerdict::NotApplied | ProbeVerdict::Unknown => {
                    self.persist_attention(&mut store, &mut journal)?;
                    return Ok(ExecutionOutcome::NeedsAttention {
                        operation_id: *plan.operation_id(),
                        step_id: step.id().clone(),
                    });
                }
            }
        }
        Ok(ExecutionOutcome::Applied {
            operation_id: *plan.operation_id(),
        })
    }

    fn scan_support(&self, plan: &OperationPlan) -> Result<(), ExecutionError<B::Error>> {
        for condition in plan.preconditions() {
            if !self.backend.supports_precondition(
                plan,
                None,
                ConditionPhase::InitialPreflight,
                condition,
            ) {
                return Err(ExecutionError::UnsupportedPlan(
                    "unsupported plan precondition".into(),
                ));
            }
        }
        for step in plan.steps() {
            if [
                ConditionPhase::InitialPreflight,
                ConditionPhase::BeforeInvoke,
            ]
            .iter()
            .any(|phase| {
                step.preconditions().iter().any(|condition| {
                    !self
                        .backend
                        .supports_precondition(plan, Some(step), *phase, condition)
                })
            }) {
                return Err(ExecutionError::UnsupportedPlan(
                    "unsupported step precondition".into(),
                ));
            }
            let context = self.context_for_step(plan, step)?;
            if !self.backend.supports_action(&context) {
                return Err(ExecutionError::UnsupportedPlan(format!(
                    "unsupported step {}",
                    step.name()
                )));
            }
            if self.backend.probe_capability(&context) == ProbeCapability::Unsupported {
                return Err(ExecutionError::UnsupportedPlan(format!(
                    "unsupported probe for {}",
                    step.name()
                )));
            }
        }
        Ok(())
    }
    fn check_all(
        &mut self,
        plan: &OperationPlan,
        step: Option<&PlanStep>,
        phase: ConditionPhase,
        conditions: &[Precondition],
    ) -> Result<Option<Precondition>, ExecutionError<B::Error>> {
        for condition in conditions {
            match self
                .backend
                .check_precondition(plan, step, phase, condition)
                .map_err(ExecutionError::Backend)?
            {
                ConditionResult::Satisfied => {}
                ConditionResult::Unsatisfied => return Ok(Some(condition.clone())),
            }
        }
        Ok(None)
    }
    fn preflight(
        &mut self,
        plan: &OperationPlan,
    ) -> Result<Option<Precondition>, ExecutionError<B::Error>> {
        if let Some(condition) = self.check_all(
            plan,
            None,
            ConditionPhase::InitialPreflight,
            plan.preconditions(),
        )? {
            return Ok(Some(condition));
        }
        for step in plan.steps() {
            if let Some(condition) = self.check_all(
                plan,
                Some(step),
                ConditionPhase::InitialPreflight,
                step.preconditions(),
            )? {
                return Ok(Some(condition));
            }
        }
        Ok(None)
    }
    fn reconcile_running(
        &mut self,
        store: &mut LockedJournalStore,
        journals: &[Journal],
        discovered: &B::Repository,
    ) -> Result<(), ExecutionError<B::Error>> {
        for journal in journals
            .iter()
            .filter(|journal| journal.status() == OperationStatus::Running)
        {
            if journal.plan().validate_executable_plan().is_err() {
                continue;
            }
            let Some(step) = journal.started_step() else {
                return Err(ExecutionError::Journal(JournalError::Corrupt(
                    "running journal has no started step".into(),
                )));
            };
            let plan_step = journal
                .plan()
                .steps()
                .iter()
                .find(|candidate| candidate.id() == step.id())
                .ok_or_else(|| {
                    ExecutionError::Journal(JournalError::Corrupt(
                        "started step missing from plan".into(),
                    ))
                })?;
            let verdict = if !self
                .backend
                .repository_matches_plan(discovered, journal.plan())
            {
                ProbeVerdict::Unknown
            } else {
                let context = self.context_for_step(journal.plan(), plan_step)?;
                if self.backend.probe_capability(&context) != ProbeCapability::Deterministic {
                    ProbeVerdict::Unknown
                } else {
                    match self
                        .backend
                        .probe(&context, ProbeContext::StartupReconciliation)
                    {
                        Ok(value) => value,
                        Err(_) => ProbeVerdict::Unknown,
                    }
                }
            };
            let mut next = journal.clone();
            next.reconcile_step(
                step.id(),
                if verdict == ProbeVerdict::Applied {
                    Reconciliation::Applied
                } else {
                    Reconciliation::Pending
                },
            )
            .map_err(|_| ExecutionError::Journal(JournalError::InvalidTransition))?;
            let current = store.read(journal.operation_id())?;
            store.update(&current, &next)?;
        }
        Ok(())
    }
    fn classify_existing(
        &self,
        journals: &[Journal],
        plan: &OperationPlan,
    ) -> Result<Option<ExecutionOutcome>, ExecutionError<B::Error>> {
        if let Some(existing) = journals
            .iter()
            .find(|journal| journal.operation_id() == plan.operation_id())
        {
            if existing.plan() != plan {
                return Err(ExecutionError::ImmutableCollision);
            }
            if existing.status() == OperationStatus::Applied {
                return Ok(Some(ExecutionOutcome::AlreadyApplied {
                    operation_id: *plan.operation_id(),
                }));
            }
            return Ok(Some(ExecutionOutcome::ExistingOperation {
                operation_id: *existing.operation_id(),
                status: existing.status(),
            }));
        }
        if let Some(existing) = journals.iter().find(|journal| journal.is_unresolved()) {
            return Ok(Some(ExecutionOutcome::ExistingOperation {
                operation_id: *existing.operation_id(),
                status: existing.status(),
            }));
        }
        Ok(None)
    }
    fn persist_attention(
        &self,
        store: &mut LockedJournalStore,
        journal: &mut Journal,
    ) -> Result<(), ExecutionError<B::Error>> {
        let previous = journal.clone();
        let id = journal
            .started_step()
            .ok_or_else(|| {
                ExecutionError::Journal(JournalError::Corrupt("missing started step".into()))
            })?
            .id()
            .clone();
        journal
            .reconcile_step(&id, Reconciliation::Pending)
            .map_err(|_| ExecutionError::Journal(JournalError::InvalidTransition))?;
        store.update(&previous, journal)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::StoredPath,
        journal_store::{JournalStore, LockedJournalStore, RepositoryLock},
        lifecycle::{OperationId, OperationPlan, RepositoryIdentity, StepAction},
    };
    use serde_json::Value;
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        fs,
        path::{Path, PathBuf},
        rc::Rc,
    };
    use tempfile::TempDir;

    #[derive(Debug, Clone)]
    struct FakeError(&'static str);
    impl fmt::Display for FakeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }
    impl Error for FakeError {}

    struct FakeBackend {
        repository: RepositoryIdentity,
        events: Rc<RefCell<Vec<String>>>,
        discoveries: Rc<Cell<usize>>,
        supports: bool,
        reject_second_action: bool,
        capability: ProbeCapability,
        run_task_unknown: bool,
        checks: usize,
        fail_check_after: Option<usize>,
        effect_error: bool,
        probes: VecDeque<Result<ProbeVerdict, FakeError>>,
        operation_id: OperationId,
        inject_after_effect: bool,
    }
    impl FakeBackend {
        fn new(plan: &OperationPlan) -> Self {
            Self {
                repository: plan.repository().clone(),
                events: Rc::new(RefCell::new(Vec::new())),
                discoveries: Rc::new(Cell::new(0)),
                supports: true,
                reject_second_action: false,
                capability: ProbeCapability::Deterministic,
                run_task_unknown: false,
                checks: 0,
                fail_check_after: None,
                effect_error: false,
                probes: VecDeque::new(),
                operation_id: *plan.operation_id(),
                inject_after_effect: false,
            }
        }
    }
    impl ExecutionBackend for FakeBackend {
        type Error = FakeError;
        type Repository = RepositoryIdentity;
        fn discover_repository(&mut self) -> Result<Self::Repository, Self::Error> {
            self.discoveries.set(1);
            Ok(self.repository.clone())
        }
        fn repository_common_dir<'a>(&self, repository: &'a Self::Repository) -> &'a Path {
            repository.common_dir.as_path()
        }
        fn repository_matches_plan(
            &self,
            repository: &Self::Repository,
            plan: &OperationPlan,
        ) -> bool {
            repository == plan.repository()
        }
        fn supports_precondition(
            &self,
            _plan: &OperationPlan,
            _step: Option<&PlanStep>,
            _phase: ConditionPhase,
            _condition: &Precondition,
        ) -> bool {
            self.supports
        }
        fn supports_action(&self, context: &StepExecutionContext<'_>) -> bool {
            self.supports
                && !(self.reject_second_action && context.step().id().as_str() == "step-1")
        }
        fn probe_capability(&self, context: &StepExecutionContext<'_>) -> ProbeCapability {
            if !self.supports {
                ProbeCapability::Unsupported
            } else if self.run_task_unknown
                && matches!(context.step().action(), StepAction::RunTask { .. })
            {
                ProbeCapability::UnknownAfterCrash
            } else {
                self.capability.clone()
            }
        }
        fn check_precondition(
            &mut self,
            _plan: &OperationPlan,
            _step: Option<&PlanStep>,
            _phase: ConditionPhase,
            _condition: &Precondition,
        ) -> Result<ConditionResult, Self::Error> {
            self.checks += 1;
            self.events
                .borrow_mut()
                .push(format!("check:{}", self.checks));
            if self.fail_check_after == Some(self.checks) {
                Ok(ConditionResult::Unsatisfied)
            } else {
                Ok(ConditionResult::Satisfied)
            }
        }
        fn invoke(&mut self, context: &StepExecutionContext<'_>) -> Result<(), Self::Error> {
            let step = context.step();
            let journal = JournalStore::new(self.repository.common_dir.as_path())
                .read(&self.operation_id)
                .ok();
            let started_id = journal
                .as_ref()
                .and_then(|journal| journal.started_step())
                .map(|step| step.id().as_str())
                .unwrap_or("missing");
            self.events.borrow_mut().push(format!(
                "invoke:{}:{}:{}",
                step.name(),
                journal
                    .as_ref()
                    .map(|j| j.status().as_str())
                    .unwrap_or("missing"),
                started_id
            ));
            if self.inject_after_effect {
                crate::journal_store::inject_fail_before_rename();
            }
            if self.effect_error {
                Err(FakeError("effect"))
            } else {
                Ok(())
            }
        }
        fn probe(
            &mut self,
            context: &StepExecutionContext<'_>,
            probe_context: ProbeContext,
        ) -> Result<ProbeVerdict, Self::Error> {
            let step = context.step();
            self.events
                .borrow_mut()
                .push(format!("probe:{}:{probe_context:?}", step.name()));
            self.probes.pop_front().unwrap_or(Ok(ProbeVerdict::Applied))
        }
    }

    fn make_plan(temp: &TempDir, count: usize) -> OperationPlan {
        let root = temp.path().to_string_lossy().to_string();
        fs::create_dir_all(temp.path().join(".git")).unwrap();
        let mut value = serde_json::to_value(crate::lifecycle::v3_test_plan(count)).unwrap();
        fn replace(value: &mut Value, root: &str) {
            match value {
                Value::String(text) if text == "/r/.git" => {
                    *value = Value::String(format!("{root}/.git"))
                }
                Value::String(text) if text == "/r" => *value = Value::String(root.into()),
                Value::String(text) if text.starts_with("/r/") => {
                    *value = Value::String(format!("{root}{}", text.strip_prefix("/r").unwrap()))
                }
                Value::String(text) if text == "/w" => *value = Value::String(root.into()),
                Value::String(text) if text.starts_with("/w/") => {
                    *value = Value::String(format!("{root}{}", text.strip_prefix("/w").unwrap()))
                }
                Value::Array(values) => values.iter_mut().for_each(|value| replace(value, root)),
                Value::Object(values) => values.values_mut().for_each(|value| replace(value, root)),
                _ => {}
            }
        }
        replace(&mut value, &root);
        refresh_v3_fixture(&mut value);
        serde_json::from_value(value).unwrap()
    }
    fn alternate_plan(root: &Path) -> OperationPlan {
        let root_string = root.to_string_lossy().to_string();
        let mut value = serde_json::to_value(crate::lifecycle::v3_test_plan(1)).unwrap();
        fn replace(value: &mut Value, root: &str) {
            match value {
                Value::String(text) if text == "/r/.git" => {
                    *value = Value::String(format!("{root}/.git"))
                }
                Value::String(text) if text == "/r" => *value = Value::String(root.into()),
                Value::String(text) if text.starts_with("/r/") => {
                    *value = Value::String(format!("{root}{}", text.strip_prefix("/r").unwrap()))
                }
                Value::String(text) if text == "/w" => *value = Value::String(root.into()),
                Value::String(text) if text.starts_with("/w/") => {
                    *value = Value::String(format!("{root}{}", text.strip_prefix("/w").unwrap()))
                }
                Value::Array(values) => values.iter_mut().for_each(|value| replace(value, root)),
                Value::Object(values) => values.values_mut().for_each(|value| replace(value, root)),
                _ => {}
            }
        }
        replace(&mut value, &root_string);
        refresh_v3_fixture(&mut value);
        serde_json::from_value(value).unwrap()
    }

    fn refresh_v3_fixture(value: &mut Value) {
        if value["steps"]
            .as_array()
            .is_none_or(|steps| steps.len() < 2)
        {
            return;
        }
        let action: StepAction =
            serde_json::from_value(value["steps"][1]["action"].clone()).unwrap();
        let StepAction::CreateSymlinkV3 {
            rule,
            source_root,
            source,
            expected_source,
            destination,
            desired,
            sensitive,
            confirm,
            ..
        } = action
        else {
            return;
        };
        let target_digest = crate::planner::artifact_digest(
            desired.target.as_path().as_os_str().as_encoded_bytes(),
        );
        let desired = crate::lifecycle::SymlinkStateV3 {
            target: desired.target,
            target_digest,
        };
        let digest = crate::planner::canonical_manifest_digest_v3(
            &[crate::planner::ManifestDescriptorV3::CreateSymlinkV3 {
                source_root: source_root.clone(),
                source: source.clone(),
                expected_source: expected_source.clone(),
                destination: destination.clone(),
                desired: desired.clone(),
                sensitive,
                confirm,
            }],
            destination.as_path().parent().unwrap(),
        );
        value["steps"][1]["action"]["CreateSymlinkV3"]["desired"] =
            serde_json::to_value(&desired).unwrap();
        value["steps"][1]["action"]["CreateSymlinkV3"]["manifest_digest"] =
            serde_json::json!(digest.as_str());
        value["steps"][1]["preconditions"][0]["ArtifactSourceAtV3"]["manifest_digest"] =
            serde_json::json!(digest.as_str());
        value["steps"][1]["compensation"]["RemoveCreatedArtifactV3"]["expected"] =
            serde_json::json!({"Symlink": desired});
        value["intent"]["Create"]["artifact_rule_contracts"][rule.as_str()]["manifest_digest"] =
            serde_json::json!(digest.as_str());
    }
    fn journal(plan: &OperationPlan) -> Journal {
        Journal::new(plan.clone())
    }
    fn files(path: &Path) -> Vec<PathBuf> {
        fs::read_dir(path)
            .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
            .unwrap_or_default()
    }

    fn repository_snapshot(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        fn collect(root: &Path, path: &Path, snapshot: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
            let relative = path.strip_prefix(root).unwrap().to_owned();
            let metadata = fs::symlink_metadata(path).unwrap();
            if metadata.is_dir() {
                snapshot.push((relative, None));
                for entry in fs::read_dir(path).unwrap() {
                    collect(root, &entry.unwrap().path(), snapshot);
                }
            } else {
                snapshot.push((relative, Some(fs::read(path).unwrap())));
            }
        }
        let mut snapshot = Vec::new();
        collect(root, root, &mut snapshot);
        snapshot.sort_by(|left, right| left.0.cmp(&right.0));
        snapshot
    }

    fn preflight_count(plan: &OperationPlan) -> usize {
        plan.preconditions().len()
            + plan
                .steps()
                .iter()
                .map(|step| step.preconditions().len())
                .sum::<usize>()
    }

    #[test]
    fn full_preflight_refusal_checks_exactly_every_condition_and_has_no_journal_or_effect() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 2);
        let expected = plan.steps()[1].preconditions()[1].clone();
        let count = preflight_count(&plan);
        let mut backend = FakeBackend::new(&plan);
        backend.fail_check_after = Some(count);
        let events = backend.events.clone();
        let outcome = ExecutionEngine::new(backend).execute(plan.clone()).unwrap();
        assert_eq!(
            outcome,
            ExecutionOutcome::PreflightRefused {
                operation_id: *plan.operation_id(),
                condition: expected,
            }
        );
        assert_eq!(events.borrow().len(), count);
        assert!(
            events
                .borrow()
                .iter()
                .all(|event| event.starts_with("check:"))
        );
        assert!(files(&plan.repository().common_dir.as_path().join("ewtm/journal")).is_empty());
    }

    #[test]
    fn late_guard_refusal_preserves_first_applied_and_second_pending() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 2);
        let late = plan.steps()[1].preconditions()[0].clone();
        let first_step_checks = plan.steps()[0].preconditions().len();
        let mut backend = FakeBackend::new(&plan);
        backend.fail_check_after = Some(preflight_count(&plan) + first_step_checks + 1);
        let events = backend.events.clone();
        let outcome = ExecutionEngine::new(backend).execute(plan.clone()).unwrap();
        assert_eq!(
            outcome,
            ExecutionOutcome::Paused {
                operation_id: *plan.operation_id(),
                step_id: plan.steps()[1].id().clone(),
                condition: late,
            }
        );
        let journal = JournalStore::new(plan.repository().common_dir.as_path())
            .read(plan.operation_id())
            .unwrap();
        assert_eq!(journal.revision(), 2);
        assert_eq!(journal.status(), OperationStatus::Pending);
        assert_eq!(
            journal.steps()[0].status(),
            crate::journal::StepStatus::Applied
        );
        assert_eq!(
            journal.steps()[1].status(),
            crate::journal::StepStatus::Pending
        );
        assert_eq!(
            events
                .borrow()
                .iter()
                .filter(|event| event.starts_with("invoke:"))
                .count(),
            1
        );
        assert_eq!(
            events
                .borrow()
                .iter()
                .filter(|event| event.starts_with("probe:"))
                .count(),
            1
        );
    }

    #[test]
    fn event_ordering_is_exact_and_started_ids_are_durable() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 2);
        let count = preflight_count(&plan);
        let backend = FakeBackend::new(&plan);
        let events = backend.events.clone();
        ExecutionEngine::new(backend).execute(plan.clone()).unwrap();
        let events = events.borrow();
        let first_invoke = events
            .iter()
            .position(|event| event.starts_with("invoke:step-0:"))
            .unwrap();
        assert_eq!(first_invoke, count + plan.steps()[0].preconditions().len());
        let first_probe = first_invoke + 1;
        assert_eq!(
            events[first_probe],
            "probe:step-0:AfterAttempt { executor_succeeded: true }"
        );
        let second_invoke = events
            .iter()
            .position(|event| event.starts_with("invoke:step-1:"))
            .unwrap();
        assert_eq!(
            second_invoke,
            first_probe + 1 + plan.steps()[1].preconditions().len()
        );
        assert_eq!(
            events[second_invoke + 1],
            "probe:step-1:AfterAttempt { executor_succeeded: true }"
        );
        assert!(
            events[..first_invoke]
                .iter()
                .all(|event| event.starts_with("check:"))
        );
        assert!(
            events[first_probe + 1..second_invoke]
                .iter()
                .all(|event| event.starts_with("check:"))
        );
        assert!(events[first_invoke].ends_with(":running:step-0"));
        assert!(events[second_invoke].ends_with(":running:step-1"));
        let journal = JournalStore::new(plan.repository().common_dir.as_path())
            .read(plan.operation_id())
            .unwrap();
        assert_eq!(journal.status(), OperationStatus::Applied);
    }

    #[test]
    fn probe_capability_is_step_specific_even_without_postconditions() {
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 2);
        let mut wire = serde_json::to_value(&plan).unwrap();
        wire["steps"][1]["action"] = serde_json::json!({"RunTask": {"name":"task","argv":["true"],"cwd":temp.path().join("task").to_string_lossy(),"required":false,"environment_allowlist":[]}});
        let plan: OperationPlan = serde_json::from_value(wire).unwrap();
        assert!(!plan.steps()[0].postconditions().is_empty());
        assert!(plan.steps()[1].postconditions().is_empty());
        let mut backend = FakeBackend::new(&plan);
        backend.run_task_unknown = true;
        assert_eq!(
            backend.probe_capability(&StepExecutionContext::new(&plan, &plan.steps()[0])),
            ProbeCapability::Deterministic
        );
        assert_eq!(
            backend.probe_capability(&StepExecutionContext::new(&plan, &plan.steps()[1])),
            ProbeCapability::UnknownAfterCrash
        );
    }

    #[test]
    fn unsupported_final_step_and_missing_consent_reject_before_lock_or_effect() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 2);
        let mut backend = FakeBackend::new(&plan);
        backend.reject_second_action = true;
        let events = backend.events.clone();
        assert!(matches!(
            ExecutionEngine::new(backend).execute(plan.clone()),
            Err(ExecutionError::UnsupportedPlan(_))
        ));
        assert!(files(&temp.path().join(".git/ewtm")).is_empty());
        assert!(events.borrow().is_empty());
        let mut consent_wire = serde_json::to_value(&plan).unwrap();
        consent_wire["intent"]["Create"]["selected_tasks"] = serde_json::json!(["task"]);
        let task_cwd = match plan.steps()[0].action() {
            StepAction::CreateWorktree { destination, .. } => destination,
            _ => unreachable!(),
        };
        consent_wire["steps"].as_array_mut().unwrap().push(serde_json::json!({
            "id": "task",
            "name": "task",
            "action": {"RunTask": {"name": "task", "argv": ["true"], "cwd": task_cwd, "required": false, "environment_allowlist": []}},
            "preconditions": [],
            "postconditions": [],
            "compensation": null,
            "irreversible": true
        }));
        consent_wire["intent"]["Create"]["task_contracts"] = serde_json::json!({
            "task": {"argv":["true"], "cwd":task_cwd, "required":false, "environment_allowlist":[]}
        });
        consent_wire["risks"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"kind":"execute_task","message":"execute task task"}));
        consent_wire["required_consents"] = serde_json::json!([{"id":"task:task","risks":[{"kind":"execute_task","message":"execute task task"}]}]);
        let consent_plan: OperationPlan = serde_json::from_value(consent_wire).unwrap();
        let backend = FakeBackend::new(&consent_plan);
        let events = backend.events.clone();
        let result = ExecutionEngine::new(backend).execute(consent_plan);
        assert!(
            matches!(result, Err(ExecutionError::MissingConsent(ref id)) if id.as_str() == "task:task"),
            "{result:?}"
        );
        assert!(events.borrow().is_empty());
        assert!(files(&temp.path().join(".git/ewtm")).is_empty());
    }

    #[test]
    fn lock_contention_and_identity_or_corrupt_list_have_no_effect() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 1);
        let lock = RepositoryLock::acquire(&temp.path().join(".git")).unwrap();
        let backend = FakeBackend::new(&plan);
        let events = backend.events.clone();
        assert!(matches!(
            ExecutionEngine::new(backend).execute(plan.clone()),
            Err(ExecutionError::Journal(
                crate::journal_store::JournalError::RepositoryBusy
            ))
        ));
        assert!(events.borrow().is_empty());
        drop(lock);
        fs::create_dir_all(temp.path().join(".git/ewtm/journal")).unwrap();
        fs::write(temp.path().join(".git/ewtm/journal/bad.json"), b"{").unwrap();
        let backend = FakeBackend::new(&plan);
        assert!(matches!(
            ExecutionEngine::new(backend).execute(plan),
            Err(ExecutionError::Journal(
                crate::journal_store::JournalError::Corrupt(_)
            ))
        ));
    }

    #[test]
    fn executable_validator_rejects_missing_late_guards_before_journal() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 2);
        let mut serialized = serde_json::to_value(&plan).unwrap();
        serialized["steps"][1]["preconditions"] = serde_json::json!(["ExactlyOnePrimary"]);
        let plan: OperationPlan = serde_json::from_value(serialized).unwrap();
        let common = plan.repository().common_dir.as_path().to_owned();
        let outcome = ExecutionEngine::new(FakeBackend::new(&plan)).execute(plan.clone());
        assert!(matches!(outcome, Err(ExecutionError::UnsupportedPlan(_))));
        assert!(files(&common.join("ewtm/journal")).is_empty());
    }

    #[test]
    fn effects_are_bracketed_by_durable_started_and_applied_states() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 2);
        let backend = FakeBackend::new(&plan);
        let events = backend.events.clone();
        let common = plan.repository().common_dir.as_path().to_owned();
        match ExecutionEngine::new(backend).execute(plan.clone()).unwrap() {
            ExecutionOutcome::Applied { operation_id } => {
                assert_eq!(operation_id, *plan.operation_id())
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        let journal = JournalStore::new(&common)
            .read(plan.operation_id())
            .unwrap();
        assert_eq!(journal.status(), OperationStatus::Applied);
        let events = events.borrow();
        assert!(
            events
                .iter()
                .any(|event| event.starts_with("invoke:step-0:"))
        );
        assert!(
            events
                .iter()
                .any(|event| event.starts_with("invoke:step-1:"))
        );
        assert!(
            events
                .iter()
                .any(|event| event.starts_with("probe:step-0:"))
        );
        assert!(
            events
                .iter()
                .any(|event| event.starts_with("probe:step-1:"))
        );
    }

    #[test]
    fn started_write_fault_skips_effect_and_applied_write_fault_leaves_started() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 1);
        let backend = FakeBackend::new(&plan);
        let events = backend.events.clone();
        crate::journal_store::inject_fail_on_atomic_write(2);
        assert!(matches!(
            ExecutionEngine::new(backend).execute(plan.clone()),
            Err(ExecutionError::Journal(_))
        ));
        assert!(
            events
                .borrow()
                .iter()
                .all(|event| event.starts_with("check:"))
        );
        let pending = JournalStore::new(plan.repository().common_dir.as_path())
            .read(plan.operation_id())
            .unwrap();
        assert_eq!(pending.revision(), 0);
        assert_eq!(pending.status(), OperationStatus::Pending);
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 1);
        let mut backend = FakeBackend::new(&plan);
        backend.inject_after_effect = true;
        let common = plan.repository().common_dir.as_path().to_owned();
        assert!(matches!(
            ExecutionEngine::new(backend).execute(plan.clone()),
            Err(ExecutionError::Journal(_))
        ));
        let journal = JournalStore::new(&common)
            .read(plan.operation_id())
            .unwrap();
        assert_eq!(journal.status(), OperationStatus::Running);
        assert_eq!(
            journal.steps()[0].status(),
            crate::journal::StepStatus::Started
        );
    }

    #[test]
    fn applied_write_fault_on_two_steps_stops_after_first_attempt() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 2);
        let mut backend = FakeBackend::new(&plan);
        backend.inject_after_effect = true;
        let events = backend.events.clone();
        assert!(matches!(
            ExecutionEngine::new(backend).execute(plan.clone()),
            Err(ExecutionError::Journal(_))
        ));
        let events = events.borrow();
        assert_eq!(
            events.iter().filter(|e| e.starts_with("invoke:")).count(),
            1
        );
        assert_eq!(events.iter().filter(|e| e.starts_with("probe:")).count(), 1);
        let persisted = JournalStore::new(plan.repository().common_dir.as_path())
            .read(plan.operation_id())
            .unwrap();
        assert_eq!(persisted.revision(), 1);
        assert_eq!(persisted.status(), OperationStatus::Running);
        assert_eq!(
            persisted.steps()[0].status(),
            crate::journal::StepStatus::Started
        );
        assert_eq!(
            persisted.steps()[1].status(),
            crate::journal::StepStatus::Pending
        );
    }

    #[test]
    fn effect_error_probe_matrix_reconciles_applied_or_attention() {
        let _guard = crate::journal_store::test_fault_guard();
        for verdict in [
            ProbeVerdict::Applied,
            ProbeVerdict::NotApplied,
            ProbeVerdict::Unknown,
        ] {
            let temp = TempDir::new().unwrap();
            let plan = make_plan(&temp, 1);
            let mut backend = FakeBackend::new(&plan);
            backend.effect_error = true;
            backend.probes.push_back(Ok(verdict));
            let events = backend.events.clone();
            let result = ExecutionEngine::new(backend).execute(plan.clone());
            if verdict == ProbeVerdict::Applied {
                assert!(
                    matches!(result, Ok(ExecutionOutcome::Applied { operation_id }) if operation_id == *plan.operation_id())
                );
            } else {
                assert!(
                    matches!(result, Ok(ExecutionOutcome::NeedsAttention { operation_id, .. }) if operation_id == *plan.operation_id())
                );
            }
            assert!(
                events
                    .borrow()
                    .iter()
                    .any(|event| event.contains("executor_succeeded: false"))
            );
            let persisted = JournalStore::new(plan.repository().common_dir.as_path())
                .read(plan.operation_id())
                .unwrap();
            if verdict == ProbeVerdict::Applied {
                assert_eq!(persisted.status(), OperationStatus::Applied);
                assert_eq!(
                    persisted.steps()[0].status(),
                    crate::journal::StepStatus::Applied
                );
            } else {
                assert_eq!(persisted.status(), OperationStatus::NeedsAttention);
                assert_eq!(
                    persisted.steps()[0].status(),
                    crate::journal::StepStatus::NeedsAttention
                );
            }
        }
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 1);
        let mut backend = FakeBackend::new(&plan);
        backend.probes.push_back(Err(FakeError("probe")));
        assert!(
            matches!(ExecutionEngine::new(backend).execute(plan.clone()), Ok(ExecutionOutcome::NeedsAttention { operation_id, .. }) if operation_id == *plan.operation_id())
        );
        let persisted = JournalStore::new(plan.repository().common_dir.as_path())
            .read(plan.operation_id())
            .unwrap();
        assert_eq!(persisted.status(), OperationStatus::NeedsAttention);
        assert_eq!(
            persisted.steps()[0].status(),
            crate::journal::StepStatus::NeedsAttention
        );
    }

    #[test]
    fn post_attempt_matrix_crosses_effect_result_and_probe_verdict() {
        let _guard = crate::journal_store::test_fault_guard();
        let mut cases = 0;
        for effect_error in [false, true] {
            for verdict in [
                Ok(ProbeVerdict::Applied),
                Ok(ProbeVerdict::NotApplied),
                Ok(ProbeVerdict::Unknown),
                Err(FakeError("probe")),
            ] {
                cases += 1;
                let temp = TempDir::new().unwrap();
                let plan = make_plan(&temp, 1);
                let mut backend = FakeBackend::new(&plan);
                backend.effect_error = effect_error;
                backend.probes.push_back(verdict.clone());
                let events = backend.events.clone();
                let result = ExecutionEngine::new(backend).execute(plan.clone()).unwrap();
                let applied = matches!(verdict, Ok(ProbeVerdict::Applied));
                let expected_status = if applied {
                    OperationStatus::Applied
                } else {
                    OperationStatus::NeedsAttention
                };
                assert_eq!(
                    result,
                    if applied {
                        ExecutionOutcome::Applied {
                            operation_id: *plan.operation_id(),
                        }
                    } else {
                        ExecutionOutcome::NeedsAttention {
                            operation_id: *plan.operation_id(),
                            step_id: plan.steps()[0].id().clone(),
                        }
                    }
                );
                let events = events.borrow();
                let expected_context = format!(
                    "probe:step-0:AfterAttempt {{ executor_succeeded: {} }}",
                    !effect_error
                );
                assert!(events.iter().any(|event| event == &expected_context));
                let persisted = JournalStore::new(plan.repository().common_dir.as_path())
                    .read(plan.operation_id())
                    .unwrap();
                assert_eq!(persisted.status(), expected_status);
                assert_eq!(
                    persisted.steps()[0].status(),
                    if applied {
                        crate::journal::StepStatus::Applied
                    } else {
                        crate::journal::StepStatus::NeedsAttention
                    }
                );
            }
        }
        assert_eq!(cases, 8);
    }

    #[test]
    fn startup_reconciliation_never_invokes_effect_and_classifies_existing_operations() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 1);
        let common = plan.repository().common_dir.as_path().to_owned();
        let mut store = LockedJournalStore::acquire(&common).unwrap();
        let initial = journal(&plan);
        store.write_new(&initial).unwrap();
        let mut running = initial.clone();
        let id = running.steps()[0].id().clone();
        running.start_step(&id).unwrap();
        store.update(&initial, &running).unwrap();
        drop(store);
        let mut backend = FakeBackend::new(&plan);
        backend.probes.push_back(Ok(ProbeVerdict::Applied));
        let events = backend.events.clone();
        assert!(matches!(
            ExecutionEngine::new(backend).execute(plan),
            Ok(ExecutionOutcome::AlreadyApplied { .. })
        ));
        assert!(
            events
                .borrow()
                .iter()
                .all(|event| !event.starts_with("invoke:"))
        );
    }

    #[test]
    fn startup_leaves_non_executable_running_journal_byte_identical_and_blocks_request() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let requested = make_plan(&temp, 1);
        let legacy_plan = make_plan(&temp, 2);
        let mut legacy = journal(&legacy_plan);
        let started_id = legacy.steps()[0].id().clone();
        legacy.start_step(&started_id).unwrap();

        let mut wire = serde_json::to_value(&legacy).unwrap();
        wire["schema_version"] = Value::from(1);
        wire["plan"]["plan_schema_version"] = Value::from(1);
        fn strip_archived_artifact_fields(value: &mut Value) {
            match value {
                Value::Object(object) => {
                    if let Some(artifact) = object.get_mut("FileArtifact")
                        && let Some(artifact) = artifact.as_object_mut()
                    {
                        artifact.remove("sensitive");
                        artifact.remove("confirm");
                        artifact.remove("mode_policy");
                    }
                    object.values_mut().for_each(strip_archived_artifact_fields);
                }
                Value::Array(values) => values.iter_mut().for_each(strip_archived_artifact_fields),
                _ => {}
            }
        }
        strip_archived_artifact_fields(&mut wire["plan"]);
        let bytes = serde_json::to_vec_pretty(&wire).unwrap();
        let dir = requested
            .repository()
            .common_dir
            .as_path()
            .join("ewtm/journal");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.json", legacy_plan.operation_id()));
        fs::write(&path, &bytes).unwrap();
        let mut before_files = files(&dir);
        before_files.sort();
        let before = (before_files, fs::read(&path).unwrap());

        let mut backend = FakeBackend::new(&requested);
        backend.probes.push_back(Ok(ProbeVerdict::Applied));
        let events = backend.events.clone();
        let outcome = ExecutionEngine::new(backend)
            .execute(requested.clone())
            .unwrap();

        assert_eq!(
            outcome,
            ExecutionOutcome::ExistingOperation {
                operation_id: *legacy_plan.operation_id(),
                status: OperationStatus::Running,
            }
        );
        assert!(
            events
                .borrow()
                .iter()
                .all(|event| { !event.starts_with("probe:") && !event.starts_with("invoke:") })
        );
        let mut after_files = files(&dir);
        after_files.sort();
        assert_eq!(after_files, before.0);
        assert_eq!(fs::read(&path).unwrap(), before.1);
        let restored = JournalStore::new(requested.repository().common_dir.as_path())
            .read(legacy_plan.operation_id())
            .unwrap();
        assert_eq!(restored.status(), OperationStatus::Running);
        assert!(
            JournalStore::new(requested.repository().common_dir.as_path())
                .read(requested.operation_id())
                .is_err()
        );
    }

    #[test]
    fn schema2_source_manifest_running_journal_is_skipped_during_plan_b_refusal() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let historical = make_plan(&temp, 1);
        let mut wire = serde_json::to_value(&historical).unwrap();
        wire["plan_schema_version"] = Value::from(2);
        let source_manifest = serde_json::json!({
            "SourceManifest": {
                "rule": "legacy-source",
                "source": "/r/source",
                "destination": "/r/w",
                "digest": "0000000000000000000000000000000000000000"
            }
        });
        wire["preconditions"]
            .as_array_mut()
            .unwrap()
            .push(source_manifest.clone());
        wire["steps"][0]["preconditions"]
            .as_array_mut()
            .unwrap()
            .push(source_manifest);
        let historical: OperationPlan = serde_json::from_value(wire).unwrap();
        assert!(historical.validate_persisted().is_ok());
        assert!(historical.validate_executable_plan().is_err());

        let common = historical.repository().common_dir.as_path().to_owned();
        let mut store = LockedJournalStore::acquire(&common).unwrap();
        let initial = journal(&historical);
        store.write_new(&initial).unwrap();
        let mut running = initial.clone();
        running
            .start_step(&running.steps()[0].id().clone())
            .unwrap();
        store.update(&initial, &running).unwrap();
        drop(store);

        let journal_path = common
            .join("ewtm/journal")
            .join(format!("{}.json", historical.operation_id()));
        let before_journal = fs::read(&journal_path).unwrap();
        let before_repository = repository_snapshot(temp.path());
        let mut before_ewtm = files(&common.join("ewtm"));
        before_ewtm.sort();
        let requested = make_plan(&temp, 1);
        assert_ne!(historical.operation_id(), requested.operation_id());
        assert_eq!(historical.repository(), requested.repository());
        let mut backend = FakeBackend::new(&requested);
        backend.fail_check_after = Some(1);
        let events = backend.events.clone();
        let discoveries = backend.discoveries.clone();
        let result = ExecutionEngine::new(backend).execute(requested.clone());

        // Plan B is refused as an unresolved existing operation after A is skipped;
        // the ordinary-precondition guard ensures no B preflight is accidentally reached.
        assert!(matches!(
            result,
            Ok(ExecutionOutcome::ExistingOperation {
                operation_id,
                status: OperationStatus::Running,
            }) if operation_id == *historical.operation_id()
        ));
        let events = events.borrow();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("check:"))
                .count(),
            0
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("probe:"))
                .count(),
            0
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("invoke:"))
                .count(),
            0
        );
        assert_eq!(discoveries.get(), 1);
        let mut after_ewtm = files(&common.join("ewtm"));
        after_ewtm.sort();
        assert_eq!(after_ewtm, before_ewtm);
        assert_eq!(fs::read(&journal_path).unwrap(), before_journal);
        assert_eq!(repository_snapshot(temp.path()), before_repository);
        assert!(
            !common
                .join("ewtm/journal")
                .join(format!("{}.json", requested.operation_id()))
                .exists()
        );
    }

    #[test]
    fn startup_reconciliation_matrix_is_durable_and_effect_free() {
        for (verdict, capability, expect_applied) in [
            (
                Ok(ProbeVerdict::Applied),
                ProbeCapability::Deterministic,
                true,
            ),
            (
                Ok(ProbeVerdict::NotApplied),
                ProbeCapability::Deterministic,
                false,
            ),
            (
                Ok(ProbeVerdict::Unknown),
                ProbeCapability::Deterministic,
                false,
            ),
            (
                Err(FakeError("probe")),
                ProbeCapability::Deterministic,
                false,
            ),
            (
                Ok(ProbeVerdict::Applied),
                ProbeCapability::UnknownAfterCrash,
                false,
            ),
        ] {
            let _guard = crate::journal_store::test_fault_guard();
            let temp = TempDir::new().unwrap();
            let plan = make_plan(&temp, 1);
            let common = plan.repository().common_dir.as_path().to_owned();
            let mut store = LockedJournalStore::acquire(&common).unwrap();
            let initial = journal(&plan);
            store.write_new(&initial).unwrap();
            let mut running = initial.clone();
            let id = running.steps()[0].id().clone();
            running.start_step(&id).unwrap();
            store.update(&initial, &running).unwrap();
            drop(store);
            let mut backend = FakeBackend::new(&plan);
            backend.capability = capability.clone();
            backend.probes.push_back(verdict);
            let events = backend.events.clone();
            let result = ExecutionEngine::new(backend).execute(plan.clone()).unwrap();
            let persisted = JournalStore::new(&common)
                .read(plan.operation_id())
                .unwrap();
            if expect_applied {
                assert!(
                    matches!(result, ExecutionOutcome::AlreadyApplied { operation_id } if operation_id == *plan.operation_id())
                );
                assert_eq!(persisted.status(), OperationStatus::Applied);
            } else {
                assert!(
                    matches!(result, ExecutionOutcome::ExistingOperation { operation_id, status: OperationStatus::NeedsAttention } if operation_id == *plan.operation_id())
                );
                assert_eq!(persisted.status(), OperationStatus::NeedsAttention);
            }
            assert!(
                events
                    .borrow()
                    .iter()
                    .all(|event| !event.starts_with("invoke:"))
            );
            if capability == ProbeCapability::UnknownAfterCrash {
                assert!(
                    events
                        .borrow()
                        .iter()
                        .all(|event| !event.starts_with("probe:"))
                );
            } else {
                assert!(
                    events
                        .borrow()
                        .iter()
                        .any(|event| event.contains("StartupReconciliation"))
                );
            }
        }
    }

    #[test]
    fn existing_state_outcomes_and_identity_refusal_are_typed() {
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 1);
        let common = plan.repository().common_dir.as_path().to_owned();
        let mut store = LockedJournalStore::acquire(&common).unwrap();
        store.write_new(&journal(&plan)).unwrap();
        drop(store);
        let backend = FakeBackend::new(&plan);
        assert!(matches!(
            ExecutionEngine::new(backend).execute(plan.clone()),
            Ok(ExecutionOutcome::ExistingOperation {
                status: OperationStatus::Pending,
                ..
            })
        ));
        let mut replacement_wire = serde_json::to_value(&plan).unwrap();
        replacement_wire["steps"][0]["name"] = Value::from("different-plan");
        let replacement: OperationPlan = serde_json::from_value(replacement_wire).unwrap();
        let backend = FakeBackend::new(&replacement);
        assert!(matches!(
            ExecutionEngine::new(backend).execute(replacement),
            Err(ExecutionError::ImmutableCollision)
        ));
        let temp = TempDir::new().unwrap();
        let plan2 = make_plan(&temp, 1);
        let mut backend = FakeBackend::new(&plan2);
        backend.repository.primary_root = StoredPath::from(PathBuf::from("/different"));
        assert!(matches!(
            ExecutionEngine::new(backend).execute(plan2),
            Err(ExecutionError::RepositoryIdentityMismatch)
        ));
    }

    #[test]
    fn existing_state_matrix_is_isolated_and_reports_the_blocking_identity() {
        let _guard = crate::journal_store::test_fault_guard();
        for (label, expected) in [
            ("pending", OperationStatus::Pending),
            ("attention", OperationStatus::NeedsAttention),
            ("failed_applied_prefix", OperationStatus::Failed),
        ] {
            let temp = TempDir::new().unwrap();
            let blocking = make_plan(
                &temp,
                if label == "failed_applied_prefix" {
                    2
                } else {
                    1
                },
            );
            let requested = alternate_plan(temp.path());
            let mut store =
                LockedJournalStore::acquire(blocking.repository().common_dir.as_path()).unwrap();
            let initial = journal(&blocking);
            store.write_new(&initial).unwrap();
            let mut state = initial.clone();
            if label == "attention" {
                state.start_step(&blocking.steps()[0].id().clone()).unwrap();
                store.update(&initial, &state).unwrap();
                let previous = state.clone();
                state
                    .reconcile_step(&blocking.steps()[0].id().clone(), Reconciliation::Pending)
                    .unwrap();
                store.update(&previous, &state).unwrap();
            } else if label == "failed_applied_prefix" {
                state.start_step(&blocking.steps()[0].id().clone()).unwrap();
                store.update(&initial, &state).unwrap();
                let previous = state.clone();
                state.apply_step(&blocking.steps()[0].id().clone()).unwrap();
                store.update(&previous, &state).unwrap();
                let previous = state.clone();
                state.fail_operation().unwrap();
                store.update(&previous, &state).unwrap();
            }
            drop(store);
            let outcome = ExecutionEngine::new(FakeBackend::new(&requested))
                .execute(requested.clone())
                .unwrap();
            assert_eq!(
                outcome,
                ExecutionOutcome::ExistingOperation {
                    operation_id: *blocking.operation_id(),
                    status: expected,
                }
            );
        }

        let temp = TempDir::new().unwrap();
        let unrelated_failed = make_plan(&temp, 1);
        let requested = alternate_plan(temp.path());
        let mut store =
            LockedJournalStore::acquire(unrelated_failed.repository().common_dir.as_path())
                .unwrap();
        let initial = journal(&unrelated_failed);
        store.write_new(&initial).unwrap();
        let mut failed = initial.clone();
        failed.fail_operation().unwrap();
        store.update(&initial, &failed).unwrap();
        drop(store);
        assert!(matches!(
            ExecutionEngine::new(FakeBackend::new(&requested)).execute(requested.clone()),
            Ok(ExecutionOutcome::Applied { operation_id }) if operation_id == *requested.operation_id()
        ));
        let temp = TempDir::new().unwrap();
        let applied = make_plan(&temp, 1);
        let requested = alternate_plan(temp.path());
        let mut store =
            LockedJournalStore::acquire(applied.repository().common_dir.as_path()).unwrap();
        let initial = journal(&applied);
        store.write_new(&initial).unwrap();
        let mut complete = initial.clone();
        complete
            .start_step(&applied.steps()[0].id().clone())
            .unwrap();
        store.update(&initial, &complete).unwrap();
        let previous = complete.clone();
        complete
            .apply_step(&applied.steps()[0].id().clone())
            .unwrap();
        store.update(&previous, &complete).unwrap();
        drop(store);
        assert!(matches!(
            ExecutionEngine::new(FakeBackend::new(&requested)).execute(requested),
            Ok(ExecutionOutcome::Applied { .. })
        ));
    }

    #[test]
    fn mismatched_historical_repository_is_reconciled_without_probe_or_invoke() {
        let _guard = crate::journal_store::test_fault_guard();
        let requested_temp = TempDir::new().unwrap();
        let requested = make_plan(&requested_temp, 1);
        let historical_temp = TempDir::new().unwrap();
        let historical = make_plan(&historical_temp, 1);
        let requested_common = requested.repository().common_dir.clone();
        let historical_common = historical.repository().common_dir.clone();
        let mut historical_wire = serde_json::to_value(&historical).unwrap();
        fn replace_common_dir(value: &mut Value, old: &str, new: &str) {
            match value {
                Value::Object(values) => {
                    if values.get("common_dir").and_then(Value::as_str) == Some(old) {
                        values.insert("common_dir".into(), Value::String(new.into()));
                    }
                    if values.get("CommonDirectory").and_then(Value::as_str) == Some(old) {
                        values.insert("CommonDirectory".into(), Value::String(new.into()));
                    }
                    values
                        .values_mut()
                        .for_each(|value| replace_common_dir(value, old, new));
                }
                Value::Array(values) => values
                    .iter_mut()
                    .for_each(|value| replace_common_dir(value, old, new)),
                _ => {}
            }
        }
        replace_common_dir(
            &mut historical_wire,
            historical_common.as_path().to_str().unwrap(),
            requested_common.as_path().to_str().unwrap(),
        );
        let historical_plan: OperationPlan = serde_json::from_value(historical_wire).unwrap();
        assert!(historical_plan.validate_executable_plan().is_ok());
        assert!(
            !FakeBackend::new(&requested)
                .repository_matches_plan(requested.repository(), &historical_plan)
        );
        let mut store = LockedJournalStore::acquire(requested_common.as_path()).unwrap();
        let initial = journal(&historical_plan);
        store.write_new(&initial).unwrap();
        let mut running = initial.clone();
        let id = running.steps()[0].id().clone();
        running.start_step(&id).unwrap();
        store.update(&initial, &running).unwrap();
        drop(store);
        let mut backend = FakeBackend::new(&requested);
        backend.probes.push_back(Ok(ProbeVerdict::Applied));
        let events = backend.events.clone();
        assert!(matches!(
            ExecutionEngine::new(backend).execute(requested.clone()),
            Ok(ExecutionOutcome::ExistingOperation {
                operation_id,
                status: OperationStatus::NeedsAttention,
            }) if operation_id == *historical_plan.operation_id()
        ));
        assert!(
            events
                .borrow()
                .iter()
                .all(|event| !event.starts_with("probe:") && !event.starts_with("invoke:"))
        );
        assert_eq!(
            JournalStore::new(requested_common.as_path())
                .read(historical_plan.operation_id())
                .unwrap()
                .status(),
            OperationStatus::NeedsAttention
        );
    }
}
