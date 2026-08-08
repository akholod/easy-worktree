use crate::{
    journal::{Journal, OperationStatus, Reconciliation},
    journal_store::{JournalError, LockedJournalStore},
    lifecycle::{OperationPlan, PlanStep, Precondition, StepAction},
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
    fn supports_precondition(&self, precondition: &Precondition) -> bool;
    fn supports_action(&self, action: &StepAction) -> bool;
    fn probe_capability(&self, step: &PlanStep) -> ProbeCapability;
    fn check_precondition(
        &mut self,
        precondition: &Precondition,
    ) -> Result<ConditionResult, Self::Error>;
    fn invoke(&mut self, step: &PlanStep) -> Result<(), Self::Error>;
    fn probe(
        &mut self,
        step: &PlanStep,
        context: ProbeContext,
    ) -> Result<ProbeVerdict, Self::Error>;
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
    pub fn execute(
        &mut self,
        plan: OperationPlan,
    ) -> Result<ExecutionOutcome, ExecutionError<B::Error>> {
        plan.validate_persisted()
            .map_err(ExecutionError::UnsupportedPlan)?;
        if let Some(consent) = plan
            .required_consents()
            .iter()
            .find(|consent| !plan.granted_consents().contains(&consent.id))
        {
            return Err(ExecutionError::MissingConsent(consent.id.clone()));
        }
        let discovered = self
            .backend
            .discover_repository()
            .map_err(ExecutionError::Backend)?;
        if !self.backend.repository_matches_plan(&discovered, &plan) {
            return Err(ExecutionError::RepositoryIdentityMismatch);
        }
        self.scan_support(&plan)?;
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
            if let Some(condition) = self.check_all(step.preconditions())? {
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
            let effect_error = self.backend.invoke(step);
            let probe = self.backend.probe(
                step,
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
            if !self.backend.supports_precondition(condition) {
                return Err(ExecutionError::UnsupportedPlan(
                    "unsupported plan precondition".into(),
                ));
            }
        }
        for step in plan.steps() {
            if step
                .preconditions()
                .iter()
                .any(|condition| !self.backend.supports_precondition(condition))
            {
                return Err(ExecutionError::UnsupportedPlan(
                    "unsupported step precondition".into(),
                ));
            }
            if !self.backend.supports_action(step.action()) {
                return Err(ExecutionError::UnsupportedPlan(format!(
                    "unsupported step {}",
                    step.name()
                )));
            }
            if self.backend.probe_capability(step) == ProbeCapability::Unsupported {
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
        conditions: &[Precondition],
    ) -> Result<Option<Precondition>, ExecutionError<B::Error>> {
        for condition in conditions {
            match self
                .backend
                .check_precondition(condition)
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
        if let Some(condition) = self.check_all(plan.preconditions())? {
            return Ok(Some(condition));
        }
        for step in plan.steps() {
            if let Some(condition) = self.check_all(step.preconditions())? {
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
            } else if self.backend.probe_capability(plan_step) == ProbeCapability::Deterministic {
                match self
                    .backend
                    .probe(plan_step, ProbeContext::StartupReconciliation)
                {
                    Ok(value) => value,
                    Err(_) => ProbeVerdict::Unknown,
                }
            } else {
                ProbeVerdict::Unknown
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
        lifecycle::{OperationId, OperationPlan, RepositoryIdentity},
    };
    use serde_json::Value;
    use std::{
        cell::RefCell,
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
        fn supports_precondition(&self, _condition: &Precondition) -> bool {
            self.supports
        }
        fn supports_action(&self, _action: &StepAction) -> bool {
            self.supports
                && !(self.reject_second_action
                    && matches!(_action, StepAction::CreateWorktree { destination, .. } if destination.as_path().to_string_lossy().ends_with("/1")))
        }
        fn probe_capability(&self, step: &PlanStep) -> ProbeCapability {
            if !self.supports {
                ProbeCapability::Unsupported
            } else if self.run_task_unknown && matches!(step.action(), StepAction::RunTask { .. }) {
                ProbeCapability::UnknownAfterCrash
            } else {
                self.capability.clone()
            }
        }
        fn check_precondition(
            &mut self,
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
        fn invoke(&mut self, step: &PlanStep) -> Result<(), Self::Error> {
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
            step: &PlanStep,
            context: ProbeContext,
        ) -> Result<ProbeVerdict, Self::Error> {
            self.events
                .borrow_mut()
                .push(format!("probe:{}:{context:?}", step.name()));
            self.probes.pop_front().unwrap_or(Ok(ProbeVerdict::Applied))
        }
    }

    fn make_plan(temp: &TempDir, count: usize) -> OperationPlan {
        let root = temp.path().to_string_lossy().to_string();
        fs::create_dir(temp.path().join(".git")).unwrap();
        let mut value = serde_json::to_value(crate::lifecycle::test_plan(count)).unwrap();
        fn replace(value: &mut Value, root: &str) {
            match value {
                Value::String(text) if text == "/r/.git" => {
                    *value = Value::String(format!("{root}/.git"))
                }
                Value::String(text) if text == "/r" => *value = Value::String(root.into()),
                Value::String(text) if text.starts_with("/w/") => {
                    *value = Value::String(format!("{root}{}", text.strip_prefix("/w").unwrap()))
                }
                Value::Array(values) => values.iter_mut().for_each(|value| replace(value, root)),
                Value::Object(values) => values.values_mut().for_each(|value| replace(value, root)),
                _ => {}
            }
        }
        replace(&mut value, &root);
        serde_json::from_value(value).unwrap()
    }
    fn alternate_plan(root: &Path) -> OperationPlan {
        let root_string = root.to_string_lossy().to_string();
        let mut value = serde_json::to_value(crate::lifecycle::test_plan(1)).unwrap();
        fn replace(value: &mut Value, root: &str) {
            match value {
                Value::String(text) if text == "/r/.git" => {
                    *value = Value::String(format!("{root}/.git"))
                }
                Value::String(text) if text == "/r" => *value = Value::String(root.into()),
                Value::String(text) if text.starts_with("/w/") => {
                    *value = Value::String(format!("{root}{}", text.strip_prefix("/w").unwrap()))
                }
                Value::Array(values) => values.iter_mut().for_each(|value| replace(value, root)),
                Value::Object(values) => values.values_mut().for_each(|value| replace(value, root)),
                _ => {}
            }
        }
        replace(&mut value, &root_string);
        serde_json::from_value(value).unwrap()
    }
    fn journal(plan: &OperationPlan) -> Journal {
        Journal::new(plan.clone())
    }
    fn guarded_plan(temp: &TempDir, count: usize) -> OperationPlan {
        let plan = make_plan(temp, count);
        let mut value = serde_json::to_value(&plan).unwrap();
        for step in value["steps"].as_array_mut().unwrap() {
            step["preconditions"] = serde_json::json!(["ExactlyOnePrimary"]);
        }
        serde_json::from_value(value).unwrap()
    }
    fn files(path: &Path) -> Vec<PathBuf> {
        fs::read_dir(path)
            .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn probe_capability_is_step_specific_even_without_postconditions() {
        let temp = TempDir::new().unwrap();
        let plan = make_plan(&temp, 2);
        let mut wire = serde_json::to_value(&plan).unwrap();
        wire["steps"][1]["action"] = serde_json::json!({"RunTask": {"name":"task","argv":["true"],"cwd":temp.path().join("task").to_string_lossy(),"required":false,"environment_allowlist":[]}});
        let plan: OperationPlan = serde_json::from_value(wire).unwrap();
        assert!(plan.steps()[0].postconditions().is_empty());
        assert!(plan.steps()[1].postconditions().is_empty());
        let mut backend = FakeBackend::new(&plan);
        backend.run_task_unknown = true;
        assert_eq!(
            backend.probe_capability(&plan.steps()[0]),
            ProbeCapability::Deterministic
        );
        assert_eq!(
            backend.probe_capability(&plan.steps()[1]),
            ProbeCapability::UnknownAfterCrash
        );
    }

    #[test]
    fn unsupported_final_step_and_missing_consent_reject_before_lock_or_effect() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let plan = guarded_plan(&temp, 2);
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
        consent_wire["required_consents"] = serde_json::json!([{"id":"execute","risks":[{"kind":"execute_task","message":"test"}]}]);
        let consent_plan: OperationPlan = serde_json::from_value(consent_wire).unwrap();
        let backend = FakeBackend::new(&consent_plan);
        assert!(matches!(
            ExecutionEngine::new(backend).execute(consent_plan),
            Err(ExecutionError::MissingConsent(_))
        ));
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
    fn preflight_failure_creates_no_journal_and_late_guard_pauses_with_prefix() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let mut plan_value = serde_json::to_value(crate::lifecycle::test_plan(2)).unwrap();
        plan_value["steps"][1]["preconditions"] = serde_json::json!(["ExactlyOnePrimary"]);
        let mut plan = make_plan(&temp, 2);
        let mut serialized = serde_json::to_value(&plan).unwrap();
        serialized["steps"][1]["preconditions"] = plan_value["steps"][1]["preconditions"].clone();
        plan = serde_json::from_value(serialized).unwrap();
        let mut backend = FakeBackend::new(&plan);
        backend.fail_check_after = Some(1);
        let common = plan.repository().common_dir.as_path().to_owned();
        let outcome = ExecutionEngine::new(backend).execute(plan.clone()).unwrap();
        assert!(matches!(
            outcome,
            ExecutionOutcome::PreflightRefused {
                condition: Precondition::ExactlyOnePrimary,
                ..
            }
        ));
        assert!(files(&common.join("ewtm/journal")).is_empty());
        let mut backend = FakeBackend::new(&plan);
        backend.fail_check_after = Some(2);
        let outcome = ExecutionEngine::new(backend).execute(plan.clone()).unwrap();
        assert!(
            matches!(outcome, ExecutionOutcome::Paused { operation_id, ref step_id, condition: Precondition::ExactlyOnePrimary } if operation_id == *plan.operation_id() && step_id.as_str() == "step-1")
        );
        let paused = JournalStore::new(&common)
            .read(plan.operation_id())
            .unwrap();
        assert_eq!(paused.revision(), 2);
        assert_eq!(
            paused.steps()[0].status(),
            crate::journal::StepStatus::Applied
        );
        assert_eq!(
            paused.steps()[1].status(),
            crate::journal::StepStatus::Pending
        );
    }

    #[test]
    fn effects_are_bracketed_by_durable_started_and_applied_states() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let plan = guarded_plan(&temp, 2);
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
        assert_eq!(
            &*events.borrow(),
            &[
                "check:1",
                "check:2",
                "check:3",
                "invoke:step-0:running:step-0",
                "probe:step-0:AfterAttempt { executor_succeeded: true }",
                "check:4",
                "invoke:step-1:running:step-1",
                "probe:step-1:AfterAttempt { executor_succeeded: true }"
            ]
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
        assert!(events.borrow().is_empty());
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
    fn mismatched_historical_repository_is_reconciled_without_probe_or_invoke() {
        let _guard = crate::journal_store::test_fault_guard();
        let temp = TempDir::new().unwrap();
        let requested = make_plan(&temp, 1);
        let mut historical_wire = serde_json::to_value(alternate_plan(temp.path())).unwrap();
        fn replace_root(value: &mut Value, root: &str) {
            match value {
                Value::String(text) if text == root => {
                    *value = Value::String(format!("{root}-other"))
                }
                Value::Array(values) => values
                    .iter_mut()
                    .for_each(|value| replace_root(value, root)),
                Value::Object(values) => values
                    .values_mut()
                    .for_each(|value| replace_root(value, root)),
                _ => {}
            }
        }
        let root = temp.path().to_string_lossy().to_string();
        replace_root(&mut historical_wire, &root);
        let historical_plan: OperationPlan = serde_json::from_value(historical_wire).unwrap();
        let common = requested.repository().common_dir.as_path().to_owned();
        let mut store = LockedJournalStore::acquire(&common).unwrap();
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
            ExecutionEngine::new(backend).execute(requested),
            Ok(ExecutionOutcome::ExistingOperation { .. })
        ));
        assert!(
            events
                .borrow()
                .iter()
                .all(|event| !event.starts_with("probe:") && !event.starts_with("invoke:"))
        );
        assert_eq!(
            JournalStore::new(&common)
                .read(historical_plan.operation_id())
                .unwrap()
                .status(),
            OperationStatus::NeedsAttention
        );
    }
}
