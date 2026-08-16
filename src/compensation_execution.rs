//! The compensation state machine.  This module is deliberately independent
//! from the forward execution engine: the journal is the only effect authority.
use crate::{
    compensation::{CompensationActionV1, CompensationError, ProposalId, revalidate_proposal},
    compensation_authority::LoadedProposal,
    compensation_backend::{
        ArtifactObservation, BranchObservation, CapabilityRefusal, CompensationBackend,
        WorktreeObservation,
    },
    compensation_journal::{
        AttentionKind, CompensationJournalV1, CompensationStatus, CompensationStepStatus,
    },
    compensation_store::{CompensationStoreError, LockedCompensationStore},
};
use std::{path::Path, sync::Arc};

#[derive(Clone)]
pub struct CompensationCancellation(Arc<dyn Fn() -> bool + Send + Sync>);
impl CompensationCancellation {
    pub fn new(f: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }
    pub fn is_cancelled(&self) -> bool {
        (self.0)()
    }
}
impl Default for CompensationCancellation {
    fn default() -> Self {
        Self::new(|| false)
    }
}

pub enum CompensationExecutionMode<'a> {
    Fresh {
        authority: &'a LoadedProposal,
    },
    Resume {
        authority: &'a LoadedProposal,
        proposal_id: ProposalId,
    },
}
pub struct CompensationExecutionRequest<'a> {
    pub anchor: &'a Path,
    pub mode: CompensationExecutionMode<'a>,
    pub cancellation: CompensationCancellation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableCompensationState {
    pub proposal_id: ProposalId,
    pub status: CompensationStatus,
    pub revision: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionResult {
    pub state: DurableCompensationState,
    pub kind: AttentionKind,
    pub index: Option<u32>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelledResult {
    pub proposal_id: ProposalId,
    pub status: Option<CompensationStatus>,
    pub revision: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageUncertainResult {
    pub proposal_id: ProposalId,
    pub observed: Option<DurableCompensationState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompensationExecutionOutcome {
    Applied(DurableCompensationState),
    AlreadyApplied(DurableCompensationState),
    NeedsAttention(AttentionResult),
    Cancelled(CancelledResult),
    StorageUncertain(StorageUncertainResult),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompensationExecutionRefusal {
    ExistingJournal,
    MissingJournal,
    JournalCorrupt,
    AuthorityMismatch,
    Capability(CapabilityRefusal),
    EvidenceDrift,
    TooLarge,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompensationExecutionError {
    Refused(CompensationExecutionRefusal),
    Storage(String),
    Repository,
    Forward(CompensationError),
}

pub struct CompensationExecutionEngine<B> {
    pub backend: B,
}
impl<B: CompensationBackend> CompensationExecutionEngine<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    pub fn execute<'a>(
        &self,
        request: CompensationExecutionRequest<'a>,
    ) -> Result<CompensationExecutionOutcome, CompensationExecutionError> {
        let (authority, id, fresh) = match request.mode {
            CompensationExecutionMode::Fresh { authority } => {
                (authority, authority.proposal().proposal_id, true)
            }
            CompensationExecutionMode::Resume {
                authority,
                proposal_id,
            } => (authority, proposal_id, false),
        };
        authority
            .proposal()
            .validate()
            .map_err(|_| refused(CompensationExecutionRefusal::AuthorityMismatch))?;
        if !fresh && id != authority.proposal().proposal_id {
            return Err(refused(CompensationExecutionRefusal::AuthorityMismatch));
        }
        if fresh && request.cancellation.is_cancelled() {
            return Ok(cancelled(id, None));
        }

        let discovered = self
            .backend
            .discover_repository(request.anchor)
            .map_err(|_| CompensationExecutionError::Repository)?;
        if discovered != authority.proposal().repository {
            return Err(refused(CompensationExecutionRefusal::EvidenceDrift));
        }
        let mut capability_refusal = None;
        for step in &authority.proposal().steps {
            if let Err(error) = self.backend.check_capability(&step.action) {
                capability_refusal.get_or_insert(error);
            }
        }
        if let Some(error) = capability_refusal {
            return Err(refused(CompensationExecutionRefusal::Capability(error)));
        }
        let locked =
            LockedCompensationStore::acquire(discovered.common_dir.as_path()).map_err(storage)?;
        if fresh {
            self.revalidate(request.anchor, authority, &locked)?;
        }

        let mut journal = if fresh {
            if request.cancellation.is_cancelled() {
                return Ok(cancelled(id, None));
            }
            match locked.create_initial(authority) {
                Ok(value) => value,
                Err(CompensationStoreError::AlreadyUsed) => {
                    return Err(refused(CompensationExecutionRefusal::ExistingJournal));
                }
                Err(CompensationStoreError::CommitUncertain) => {
                    return Ok(self.uncertain(&locked, authority, id));
                }
                Err(CompensationStoreError::TooLarge) => {
                    return Err(refused(CompensationExecutionRefusal::TooLarge));
                }
                Err(e) => return Err(storage(e)),
            }
        } else {
            match locked.read(&id) {
                Ok(value) => value,
                Err(CompensationStoreError::NotFound) => {
                    return Err(refused(CompensationExecutionRefusal::MissingJournal));
                }
                Err(CompensationStoreError::TooLarge) | Err(CompensationStoreError::Corrupt(_)) => {
                    return Err(refused(CompensationExecutionRefusal::JournalCorrupt));
                }
                Err(e) => return Err(storage(e)),
            }
        };
        if journal.proposal_id() != &id
            || journal.proposal() != authority.proposal()
            || journal.proposal_sha256() != authority.raw_sha256()
        {
            return Err(refused(CompensationExecutionRefusal::AuthorityMismatch));
        }
        let global = if fresh {
            Ok(())
        } else {
            self.revalidate(request.anchor, authority, &locked)
        };
        let global_valid = match global {
            Ok(()) => true,
            Err(error) => match journal.status() {
                CompensationStatus::Pending => {
                    let context = ExecutionContext {
                        anchor: request.anchor,
                        authority,
                        store: &locked,
                        cancel: &request.cancellation,
                    };
                    return self.persist_attention(
                        &context,
                        journal,
                        AttentionKind::EvidenceDrift,
                        false,
                        None,
                    );
                }
                CompensationStatus::Applied | CompensationStatus::NeedsAttention => {
                    return Err(error);
                }
                CompensationStatus::Running => false,
            },
        };
        let context = ExecutionContext {
            anchor: request.anchor,
            authority,
            store: &locked,
            cancel: &request.cancellation,
        };
        if request.cancellation.is_cancelled() && journal.status() != CompensationStatus::Running {
            return Ok(cancelled_from(&journal));
        }
        if journal.status() == CompensationStatus::Applied {
            if authority
                .proposal()
                .steps
                .iter()
                .any(|step| self.observe(&step.action) != Scan::After)
            {
                return Err(refused(CompensationExecutionRefusal::EvidenceDrift));
            }
            return Ok(CompensationExecutionOutcome::AlreadyApplied(state(
                &journal,
            )));
        }
        if journal.status() == CompensationStatus::NeedsAttention {
            return Ok(CompensationExecutionOutcome::NeedsAttention(
                attention_result(&journal),
            ));
        }

        let mut reconciled_started = false;
        if let Some(index) = journal.started_step().map(|s| s.index as usize) {
            match self.reconcile_started(&context, journal, index, global_valid, true)? {
                StartedReconciliation::Continue(next) => {
                    journal = *next;
                    reconciled_started = true;
                }
                StartedReconciliation::Done(outcome) => return Ok(outcome),
            }
        }
        if !reconciled_started && let Some((index, kind)) = self.blocked(authority, &journal) {
            if request.cancellation.is_cancelled() {
                return Ok(cancelled_from(&journal));
            }
            return self.persist_attention(&context, journal, kind, false, Some(index as u32));
        }
        if request.cancellation.is_cancelled() {
            return Ok(cancelled_from(&journal));
        }

        loop {
            let Some(index) = journal.pending_step().map(|s| s.index as usize) else {
                return Ok(CompensationExecutionOutcome::Applied(state(&journal)));
            };
            if index > 0 {
                if self.revalidate(request.anchor, authority, &locked).is_err() {
                    return self.persist_attention(
                        &context,
                        journal,
                        AttentionKind::EvidenceDrift,
                        false,
                        Some(index as u32),
                    );
                }
                if let Some((blocked, kind)) = self.blocked(authority, &journal) {
                    if request.cancellation.is_cancelled() {
                        return Ok(cancelled_from(&journal));
                    }
                    return self.persist_attention(
                        &context,
                        journal,
                        kind,
                        false,
                        Some(blocked as u32),
                    );
                }
                if request.cancellation.is_cancelled() {
                    return Ok(cancelled_from(&journal));
                }
            }
            if request.cancellation.is_cancelled() {
                return Ok(cancelled_from(&journal));
            }
            let started = journal
                .start_next()
                .map_err(CompensationExecutionError::Storage)?;
            journal = match locked.update(&journal, started.clone()) {
                Ok(()) => started,
                Err(CompensationStoreError::CommitUncertain) => {
                    return Ok(self.uncertain(&locked, authority, id));
                }
                Err(CompensationStoreError::TooLarge) => {
                    return Err(refused(CompensationExecutionRefusal::TooLarge));
                }
                Err(e) => return Err(storage(e)),
            };
            self.invoke(&authority.proposal().steps[index].action);
            match self.reconcile_started(&context, journal, index, true, false)? {
                StartedReconciliation::Continue(next) => {
                    journal = *next;
                    continue;
                }
                StartedReconciliation::Done(outcome) => return Ok(outcome),
            }
        }
    }

    fn revalidate(
        &self,
        anchor: &Path,
        authority: &LoadedProposal,
        store: &LockedCompensationStore,
    ) -> Result<(), CompensationExecutionError> {
        let repo = self
            .backend
            .discover_repository(anchor)
            .map_err(|_| CompensationExecutionError::Repository)?;
        let raw = store
            .read_forward_raw(&authority.proposal().source.operation_id)
            .map_err(map_store_plain)?;
        if repo != authority.proposal().repository {
            return Err(refused(CompensationExecutionRefusal::EvidenceDrift));
        }
        revalidate_proposal(authority.proposal(), &repo, &raw)
            .map_err(CompensationExecutionError::Forward)
    }
    fn blocked(
        &self,
        authority: &LoadedProposal,
        journal: &CompensationJournalV1,
    ) -> Option<(usize, AttentionKind)> {
        let observations = authority
            .proposal()
            .steps
            .iter()
            .map(|step| self.observe(&step.action))
            .collect::<Vec<_>>();
        authority
            .proposal()
            .steps
            .iter()
            .enumerate()
            .find_map(|(i, _)| {
                let observation = observations[i];
                match journal.steps()[i].status {
                    CompensationStepStatus::Applied if observation != Scan::After => {
                        Some((i, AttentionKind::EvidenceDrift))
                    }
                    CompensationStepStatus::Pending => match observation {
                        Scan::Before => None,
                        Scan::After => Some((i, AttentionKind::PreStartedAbsent)),
                        Scan::Drift | Scan::Err => Some((i, AttentionKind::EvidenceDrift)),
                    },
                    _ => None,
                }
            })
    }
    fn persist_attention(
        &self,
        context: &ExecutionContext<'_>,
        journal: CompensationJournalV1,
        kind: AttentionKind,
        after: bool,
        index: Option<u32>,
    ) -> Result<CompensationExecutionOutcome, CompensationExecutionError> {
        if !after && context.cancel.is_cancelled() {
            return Ok(cancelled_from(&journal));
        }
        let next = journal
            .attention(kind, after, index)
            .map_err(CompensationExecutionError::Storage)?;
        match context.store.update(&journal, next.clone()) {
            Ok(()) => {}
            Err(CompensationStoreError::CommitUncertain) => {
                return Ok(self.uncertain(
                    context.store,
                    context.authority,
                    *journal.proposal_id(),
                ));
            }
            Err(CompensationStoreError::TooLarge) => {
                return Err(if after {
                    CompensationExecutionError::Storage(
                        "post-effect compensation journal exceeded store limit".into(),
                    )
                } else {
                    refused(CompensationExecutionRefusal::TooLarge)
                });
            }
            Err(e) => return Err(storage(e)),
        }
        let result = CompensationExecutionOutcome::NeedsAttention(AttentionResult {
            state: state(&next),
            kind,
            index,
        });
        if context.cancel.is_cancelled() {
            return Ok(cancelled_from(&next));
        }
        Ok(result)
    }
    fn reconcile_started(
        &self,
        context: &ExecutionContext<'_>,
        journal: CompensationJournalV1,
        index: usize,
        pre_global_valid: bool,
        check_pre: bool,
    ) -> Result<StartedReconciliation, CompensationExecutionError> {
        let pre_global = pre_global_valid
            && (!check_pre
                || self
                    .revalidate(context.anchor, context.authority, context.store)
                    .is_ok());
        let observations = context
            .authority
            .proposal()
            .steps
            .iter()
            .map(|step| self.observe(&step.action))
            .collect::<Vec<_>>();
        let post_global = self
            .revalidate(context.anchor, context.authority, context.store)
            .is_ok();
        let violation = context
            .authority
            .proposal()
            .steps
            .iter()
            .enumerate()
            .find_map(|(i, _)| match journal.steps()[i].status {
                CompensationStepStatus::Applied if observations[i] != Scan::After => {
                    Some((AttentionKind::EvidenceDrift, i as u32))
                }
                CompensationStepStatus::Started => match observations[i] {
                    Scan::After => None,
                    Scan::Before => Some((AttentionKind::EffectNotApplied, index as u32)),
                    Scan::Drift | Scan::Err => Some((AttentionKind::EffectUnknown, index as u32)),
                },
                CompensationStepStatus::Pending if observations[i] != Scan::Before => {
                    Some((AttentionKind::EvidenceDrift, i as u32))
                }
                _ => None,
            });
        let next = if !pre_global || !post_global {
            journal.attention(AttentionKind::EvidenceDrift, true, Some(index as u32))
        } else if let Some((kind, observed)) = violation {
            journal.attention(kind, true, Some(observed))
        } else {
            journal.apply_started()
        }
        .map_err(CompensationExecutionError::Storage)?;
        let kind = next.steps().iter().find_map(|s| {
            s.attention
                .as_ref()
                .map(|a| (a.kind, a.observed_step_index))
        });
        match context.store.update(&journal, next.clone()) {
            Ok(()) => {}
            Err(CompensationStoreError::CommitUncertain) => {
                return Ok(StartedReconciliation::Done(self.uncertain(
                    context.store,
                    context.authority,
                    *journal.proposal_id(),
                )));
            }
            Err(CompensationStoreError::TooLarge) => {
                return Err(CompensationExecutionError::Storage(
                    "post-effect compensation journal exceeded store limit".into(),
                ));
            }
            Err(e) => return Err(storage(e)),
        }
        if let Some((kind, observed)) = kind {
            if context.cancel.is_cancelled() {
                return Ok(StartedReconciliation::Done(cancelled_from(&next)));
            }
            return Ok(StartedReconciliation::Done(
                CompensationExecutionOutcome::NeedsAttention(AttentionResult {
                    state: state(&next),
                    kind,
                    index: observed,
                }),
            ));
        }
        if context.cancel.is_cancelled() {
            return Ok(StartedReconciliation::Done(cancelled_from(&next)));
        }
        if next.status() == CompensationStatus::Pending {
            Ok(StartedReconciliation::Continue(Box::new(next)))
        } else {
            Ok(StartedReconciliation::Done(
                CompensationExecutionOutcome::Applied(state(&next)),
            ))
        }
    }
    fn invoke(&self, action: &CompensationActionV1) {
        match action {
            CompensationActionV1::RemoveCreatedArtifactV3(v) => {
                let _ = self.backend.invoke_remove_created_artifact(v);
            }
            CompensationActionV1::RemoveCreatedWorktree(v) => {
                let _ = self.backend.invoke_remove_created_worktree(v);
            }
            CompensationActionV1::DeleteCreatedLocalBranch(v) => {
                let _ = self.backend.invoke_delete_created_local_branch(v);
            }
        }
    }
    fn observe(&self, action: &CompensationActionV1) -> Scan {
        match action {
            CompensationActionV1::RemoveCreatedArtifactV3(v) => self
                .backend
                .observe_created_artifact(v)
                .map_or(Scan::Drift, |o| match o {
                    ArtifactObservation::BeforeExact => Scan::Before,
                    ArtifactObservation::AfterExact => Scan::After,
                    ArtifactObservation::Drift => Scan::Drift,
                }),
            CompensationActionV1::RemoveCreatedWorktree(v) => self
                .backend
                .observe_created_worktree(v)
                .map_or(Scan::Drift, |o| match o {
                    WorktreeObservation::BeforeExact => Scan::Before,
                    WorktreeObservation::AfterExact => Scan::After,
                    WorktreeObservation::Drift => Scan::Drift,
                }),
            CompensationActionV1::DeleteCreatedLocalBranch(v) => self
                .backend
                .observe_created_local_branch(v)
                .map_or(Scan::Drift, |o| match o {
                    BranchObservation::BeforeExact => Scan::Before,
                    BranchObservation::AfterExact => Scan::After,
                    BranchObservation::Drift => Scan::Drift,
                }),
        }
    }
    fn uncertain(
        &self,
        store: &LockedCompensationStore,
        authority: &LoadedProposal,
        id: ProposalId,
    ) -> CompensationExecutionOutcome {
        let observed = store.read(&id).ok().and_then(|journal| {
            (journal.proposal_id() == &id
                && journal.proposal() == authority.proposal()
                && journal.proposal_sha256() == authority.raw_sha256())
            .then(|| state(&journal))
        });
        CompensationExecutionOutcome::StorageUncertain(StorageUncertainResult {
            proposal_id: id,
            observed,
        })
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    Before,
    After,
    Drift,
    Err,
}
struct ExecutionContext<'a> {
    anchor: &'a Path,
    authority: &'a LoadedProposal,
    store: &'a LockedCompensationStore,
    cancel: &'a CompensationCancellation,
}
enum StartedReconciliation {
    Continue(Box<CompensationJournalV1>),
    Done(CompensationExecutionOutcome),
}
fn state(j: &CompensationJournalV1) -> DurableCompensationState {
    DurableCompensationState {
        proposal_id: *j.proposal_id(),
        status: j.status(),
        revision: j.revision(),
    }
}
fn attention_result(j: &CompensationJournalV1) -> AttentionResult {
    let a = j
        .current_step()
        .and_then(|s| s.attention.as_ref())
        .expect("validated attention journal");
    AttentionResult {
        state: state(j),
        kind: a.kind,
        index: a.observed_step_index,
    }
}
fn cancelled(id: ProposalId, j: Option<&CompensationJournalV1>) -> CompensationExecutionOutcome {
    CompensationExecutionOutcome::Cancelled(CancelledResult {
        proposal_id: id,
        status: j.map(CompensationJournalV1::status),
        revision: j.map(CompensationJournalV1::revision),
    })
}
fn cancelled_from(j: &CompensationJournalV1) -> CompensationExecutionOutcome {
    cancelled(*j.proposal_id(), Some(j))
}
fn refused(v: CompensationExecutionRefusal) -> CompensationExecutionError {
    CompensationExecutionError::Refused(v)
}
fn storage(e: impl std::fmt::Debug) -> CompensationExecutionError {
    CompensationExecutionError::Storage(format!("{e:?}"))
}
fn map_store_plain(e: CompensationStoreError) -> CompensationExecutionError {
    match e {
        CompensationStoreError::TooLarge => refused(CompensationExecutionRefusal::TooLarge),
        CompensationStoreError::Corrupt(_) => refused(CompensationExecutionRefusal::EvidenceDrift),
        other => storage(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        compensation::{
            CompensationActionV1, CompensationAllowanceV1, CompensationProposalSourceV1,
            CompensationProposalStepV1, CompensationProposalV1, forward_journal_digest,
            forward_plan_digest,
        },
        compensation_authority::load_bytes,
        compensation_backend::{ArtifactObservation, BranchObservation, WorktreeObservation},
        journal::Journal,
        journal_store::LockedJournalStore,
        lifecycle::{
            Compensation, CreatedLocalBranch, CreatedWorktree, OperationPlan, RepositoryIdentity,
        },
    };
    use sha2::{Digest, Sha256};
    use std::{
        collections::VecDeque,
        fs,
        path::{Path, PathBuf},
        str::FromStr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };
    use tempfile::TempDir;

    #[derive(Clone)]
    struct FakeBackend {
        repository: RepositoryIdentity,
        observations: Arc<Mutex<VecDeque<Scan>>>,
        discovers: Arc<Mutex<usize>>,
        probes: Arc<Mutex<usize>>,
        invokes: Arc<Mutex<usize>>,
        invocation_log: Arc<Mutex<Vec<&'static str>>>,
        discoveries: Arc<Mutex<VecDeque<Result<RepositoryIdentity, ()>>>>,
        capability: Option<CapabilityRefusal>,
        invoke_error: bool,
        cancel_flip: Option<Arc<AtomicBool>>,
        probe_cancel_flip: Option<Arc<AtomicBool>>,
        discovery_cancel_flip: Option<(usize, Arc<AtomicBool>)>,
    }
    impl FakeBackend {
        fn new(
            repository: RepositoryIdentity,
            observations: impl IntoIterator<Item = Scan>,
        ) -> Self {
            Self {
                repository,
                observations: Arc::new(Mutex::new(observations.into_iter().collect())),
                discovers: Arc::new(Mutex::new(0)),
                probes: Arc::new(Mutex::new(0)),
                invokes: Arc::new(Mutex::new(0)),
                invocation_log: Arc::new(Mutex::new(Vec::new())),
                discoveries: Arc::new(Mutex::new(VecDeque::new())),
                capability: None,
                invoke_error: false,
                cancel_flip: None,
                probe_cancel_flip: None,
                discovery_cancel_flip: None,
            }
        }
        fn counts(&self) -> (usize, usize, usize) {
            (
                *self.discovers.lock().unwrap(),
                *self.invokes.lock().unwrap(),
                *self.probes.lock().unwrap(),
            )
        }
        fn with_invoke_error(mut self) -> Self {
            self.invoke_error = true;
            self
        }
        fn with_cancel_flip(mut self, flag: Arc<AtomicBool>) -> Self {
            self.cancel_flip = Some(flag);
            self
        }
        fn with_probe_cancel_flip(mut self, flag: Arc<AtomicBool>) -> Self {
            self.probe_cancel_flip = Some(flag);
            self
        }
        fn with_discovery_cancel_flip(mut self, count: usize, flag: Arc<AtomicBool>) -> Self {
            self.discovery_cancel_flip = Some((count, flag));
            self
        }
        fn with_discoveries(
            mut self,
            values: impl IntoIterator<Item = Result<RepositoryIdentity, ()>>,
        ) -> Self {
            self.discoveries = Arc::new(Mutex::new(values.into_iter().collect()));
            self
        }
    }
    impl CompensationBackend for FakeBackend {
        type Error = ();
        fn discover_repository(&self, _: &Path) -> Result<RepositoryIdentity, Self::Error> {
            let count = {
                let mut discovers = self.discovers.lock().unwrap();
                *discovers += 1;
                *discovers
            };
            if let Some((when, flag)) = &self.discovery_cancel_flip
                && count == *when
            {
                flag.store(true, Ordering::SeqCst);
            }
            self.discoveries
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(self.repository.clone()))
        }
        fn check_capability(&self, _: &CompensationActionV1) -> Result<(), CapabilityRefusal> {
            self.capability.map_or(Ok(()), Err)
        }
        fn observe_created_artifact(
            &self,
            _: &crate::lifecycle::CreatedArtifactV3,
        ) -> Result<ArtifactObservation, Self::Error> {
            self.probe_tick();
            match self.next() {
                Scan::Before => Ok(ArtifactObservation::BeforeExact),
                Scan::After => Ok(ArtifactObservation::AfterExact),
                Scan::Drift => Ok(ArtifactObservation::Drift),
                Scan::Err => Err(()),
            }
        }
        fn invoke_remove_created_artifact(
            &self,
            _: &crate::lifecycle::CreatedArtifactV3,
        ) -> Result<(), Self::Error> {
            self.invoke("artifact")
        }
        fn observe_created_worktree(
            &self,
            _: &CreatedWorktree,
        ) -> Result<WorktreeObservation, Self::Error> {
            self.probe_tick();
            match self.next() {
                Scan::Before => Ok(WorktreeObservation::BeforeExact),
                Scan::After => Ok(WorktreeObservation::AfterExact),
                Scan::Drift => Ok(WorktreeObservation::Drift),
                Scan::Err => Err(()),
            }
        }
        fn invoke_remove_created_worktree(&self, _: &CreatedWorktree) -> Result<(), Self::Error> {
            self.invoke("worktree")
        }
        fn observe_created_local_branch(
            &self,
            _: &CreatedLocalBranch,
        ) -> Result<BranchObservation, Self::Error> {
            self.probe_tick();
            match self.next() {
                Scan::Before => Ok(BranchObservation::BeforeExact),
                Scan::After => Ok(BranchObservation::AfterExact),
                Scan::Drift => Ok(BranchObservation::Drift),
                Scan::Err => Err(()),
            }
        }
        fn invoke_delete_created_local_branch(
            &self,
            _: &CreatedLocalBranch,
        ) -> Result<(), Self::Error> {
            self.invoke("branch")
        }
    }
    impl FakeBackend {
        fn invoke(&self, action: &'static str) -> Result<(), ()> {
            *self.invokes.lock().unwrap() += 1;
            self.invocation_log.lock().unwrap().push(action);
            if let Some(flag) = &self.cancel_flip {
                flag.store(true, Ordering::SeqCst);
            }
            if self.invoke_error { Err(()) } else { Ok(()) }
        }
        fn probe_tick(&self) {
            *self.probes.lock().unwrap() += 1;
            if let Some(flag) = &self.probe_cancel_flip {
                flag.store(true, Ordering::SeqCst);
            }
        }
        fn next(&self) -> Scan {
            self.observations
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Scan::Drift)
        }
    }

    struct Fixture {
        _dir: TempDir,
        authority: LoadedProposal,
        repo: RepositoryIdentity,
        anchor: PathBuf,
        forward_bytes: Vec<u8>,
    }
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let common = root.join(".git");
        fs::create_dir_all(&common).unwrap();
        let mut value = serde_json::to_value(crate::lifecycle::test_plan(1)).unwrap();
        fn rewrite(value: &mut serde_json::Value, root: &str) {
            match value {
                serde_json::Value::String(s) if s.starts_with("/r") => {
                    *s = format!("{root}{}", &s[2..])
                }
                serde_json::Value::Array(a) => a.iter_mut().for_each(|v| rewrite(v, root)),
                serde_json::Value::Object(o) => o.values_mut().for_each(|v| rewrite(v, root)),
                _ => {}
            }
        }
        rewrite(&mut value, root.to_str().unwrap());
        let plan: OperationPlan = serde_json::from_value(value).unwrap();
        let mut forward = Journal::new(plan.clone());
        let mut store = LockedJournalStore::acquire(&common).unwrap();
        store.write_new(&forward).unwrap();
        for step in plan.steps() {
            let mut started = forward.clone();
            started.start_step(step.id()).unwrap();
            store.update(&forward, &started).unwrap();
            let mut applied = started.clone();
            applied.apply_step(step.id()).unwrap();
            store.update(&started, &applied).unwrap();
            forward = applied;
        }
        let repo = plan.repository().clone();
        let action = match plan.steps()[0].compensation().as_ref().unwrap() {
            Compensation::RemoveCreatedWorktree(v) => {
                CompensationActionV1::RemoveCreatedWorktree(v.clone())
            }
            _ => panic!(),
        };
        let branch = match &action {
            CompensationActionV1::RemoveCreatedWorktree(v) => {
                CompensationActionV1::DeleteCreatedLocalBranch(CreatedLocalBranch {
                    branch: v.branch.clone(),
                    expected_oid: v.expected_oid.clone(),
                })
            }
            _ => unreachable!(),
        };
        let raw = fs::read(
            common
                .join("ewtm/journal")
                .join(format!("{}.json", forward.operation_id())),
        )
        .unwrap();
        let proposal = CompensationProposalV1 {
            proposal_schema_version: 1,
            proposal_id: ProposalId::from_str("00000000-0000-4000-8000-000000000001").unwrap(),
            executable: false,
            repository: repo.clone(),
            source: CompensationProposalSourceV1 {
                operation_id: *forward.operation_id(),
                plan_schema_version: plan.plan_schema_version(),
                journal_schema_version: forward.schema_version(),
                journal_revision: forward.revision(),
                forward_plan_digest: forward_plan_digest(&plan).unwrap(),
                forward_journal_digest: forward_journal_digest(&raw),
            },
            allowed_categories: vec![
                CompensationAllowanceV1::Worktree,
                CompensationAllowanceV1::LocalBranch,
            ],
            steps: vec![
                CompensationProposalStepV1 {
                    forward_step_id: plan.steps()[0].id().clone(),
                    action,
                },
                CompensationProposalStepV1 {
                    forward_step_id: plan.steps()[0].id().clone(),
                    action: branch,
                },
            ],
        };
        let proposal_raw = serde_json::to_vec(&proposal).unwrap();
        let confirmation = format!("{:x}", Sha256::digest(&proposal_raw));
        let authority = load_bytes(proposal_raw, &confirmation).unwrap();
        Fixture {
            _dir: dir,
            authority,
            repo,
            anchor: root,
            forward_bytes: raw,
        }
    }
    fn run(
        observations: impl IntoIterator<Item = Scan>,
    ) -> (CompensationExecutionOutcome, FakeBackend, Fixture) {
        let f = fixture();
        let backend = FakeBackend::new(f.repo.clone(), observations);
        let check = CompensationExecutionEngine::new(backend.clone()).revalidate(
            &f.anchor,
            &f.authority,
            &LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap(),
        );
        debug_assert!(check.is_ok(), "{check:?}");
        let outcome = CompensationExecutionEngine::new(backend.clone())
            .execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Fresh {
                    authority: &f.authority,
                },
                cancellation: CompensationCancellation::default(),
            })
            .unwrap();
        (outcome, backend, f)
    }
    fn execute_with(
        f: &Fixture,
        backend: FakeBackend,
        cancellation: CompensationCancellation,
    ) -> Result<CompensationExecutionOutcome, CompensationExecutionError> {
        CompensationExecutionEngine::new(backend).execute(CompensationExecutionRequest {
            anchor: &f.anchor,
            mode: CompensationExecutionMode::Fresh {
                authority: &f.authority,
            },
            cancellation,
        })
    }

    #[test]
    fn fresh_exact_before_executes_all_steps_to_applied() {
        let (outcome, backend, _) = run([
            Scan::Before,
            Scan::Before,
            Scan::After,
            Scan::Before,
            Scan::After,
            Scan::Before,
            Scan::After,
            Scan::After,
        ]);
        assert!(
            matches!(
                outcome,
                CompensationExecutionOutcome::AlreadyApplied(_)
                    | CompensationExecutionOutcome::Applied(_)
            ),
            "{outcome:?}"
        );
        assert_eq!(backend.counts().1, 2);
    }
    #[test]
    fn pending_after_is_prestarted_absent() {
        let (outcome, backend, _) = run([Scan::After]);
        assert!(matches!(
            outcome,
            CompensationExecutionOutcome::NeedsAttention(AttentionResult {
                kind: AttentionKind::PreStartedAbsent,
                ..
            })
        ));
        assert_eq!(backend.counts().1, 0);
    }
    #[test]
    fn pending_drift_is_evidence_drift_after_revision_zero() {
        let (outcome, _, _) = run([Scan::Drift]);
        assert!(matches!(
            outcome,
            CompensationExecutionOutcome::NeedsAttention(AttentionResult {
                kind: AttentionKind::EvidenceDrift,
                ..
            })
        ));
    }
    #[test]
    fn unsafe_later_step_blocks_earlier_started_invoke() {
        let (outcome, backend, _) = run([Scan::Before, Scan::Drift]);
        assert!(matches!(
            outcome,
            CompensationExecutionOutcome::NeedsAttention(_)
        ));
        assert_eq!(backend.counts().1, 0);
    }
    #[test]
    fn static_unsupported_creates_no_compensation_journal() {
        let f = fixture();
        let mut backend = FakeBackend::new(f.repo.clone(), []);
        backend.capability = Some(CapabilityRefusal::Unsupported);
        let result = CompensationExecutionEngine::new(backend.clone()).execute(
            CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Fresh {
                    authority: &f.authority,
                },
                cancellation: CompensationCancellation::default(),
            },
        );
        assert!(matches!(
            result,
            Err(CompensationExecutionError::Refused(
                CompensationExecutionRefusal::Capability(_)
            ))
        ));
        assert_eq!(backend.counts().1, 0);
    }
    #[test]
    fn invoke_probe_counts_are_exact_for_before_after_drift() {
        for scan in [Scan::Before, Scan::After, Scan::Drift] {
            let (_, backend, _) = run([Scan::Before, Scan::Before, scan]);
            let (_, invoke, probe) = backend.counts();
            assert_eq!(invoke, 1);
            assert!(probe >= 3);
        }
    }
    #[test]
    fn cancellation_before_started_leaves_pending() {
        let f = fixture();
        let result =
            CompensationExecutionEngine::new(FakeBackend::new(f.repo.clone(), [Scan::Before]))
                .execute(CompensationExecutionRequest {
                    anchor: &f.anchor,
                    mode: CompensationExecutionMode::Fresh {
                        authority: &f.authority,
                    },
                    cancellation: CompensationCancellation::new(|| true),
                })
                .unwrap();
        assert!(matches!(
            result,
            CompensationExecutionOutcome::Cancelled(CancelledResult { status: None, .. })
        ));
    }
    #[test]
    fn applied_terminal_validates_after() {
        let (outcome, _, _) = run([
            Scan::Before,
            Scan::Before,
            Scan::After,
            Scan::Before,
            Scan::After,
            Scan::Before,
            Scan::After,
            Scan::After,
        ]);
        assert!(matches!(
            outcome,
            CompensationExecutionOutcome::Applied(_)
                | CompensationExecutionOutcome::AlreadyApplied(_)
        ));
    }
    #[test]
    fn forward_journal_bytes_remain_unchanged() {
        let (_, _, f) = run([
            Scan::Before,
            Scan::Before,
            Scan::After,
            Scan::Before,
            Scan::After,
            Scan::Before,
            Scan::After,
            Scan::After,
        ]);
        let path = f
            .repo
            .common_dir
            .as_path()
            .join("ewtm/journal")
            .join(format!(
                "{}.json",
                f.authority.proposal().source.operation_id
            ));
        assert_eq!(fs::read(path).unwrap(), f.forward_bytes);
    }
    #[test]
    fn fresh_existing_node_is_single_use() {
        let f = fixture();
        let path = f
            .repo
            .common_dir
            .as_path()
            .join("ewtm/compensation/v1/operations")
            .join(format!("{}.json", f.authority.proposal().proposal_id));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"broken").unwrap();
        let result = CompensationExecutionEngine::new(FakeBackend::new(f.repo.clone(), []))
            .execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Fresh {
                    authority: &f.authority,
                },
                cancellation: CompensationCancellation::default(),
            });
        assert!(matches!(
            result,
            Err(CompensationExecutionError::Refused(
                CompensationExecutionRefusal::ExistingJournal
            ))
        ));
    }
    #[test]
    fn resume_missing_is_refused() {
        let f = fixture();
        let result = CompensationExecutionEngine::new(FakeBackend::new(f.repo.clone(), []))
            .execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::default(),
            });
        assert!(matches!(
            result,
            Err(CompensationExecutionError::Refused(
                CompensationExecutionRefusal::MissingJournal
            ))
        ));
    }
    #[test]
    fn commit_uncertain_does_not_invoke() {
        let f = fixture();
        crate::compensation_store::inject_fault(
            crate::compensation_store::StoreFault::InitialParentSync,
        );
        let backend = FakeBackend::new(f.repo.clone(), []);
        let result = CompensationExecutionEngine::new(backend.clone())
            .execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Fresh {
                    authority: &f.authority,
                },
                cancellation: CompensationCancellation::default(),
            })
            .unwrap();
        assert!(matches!(
            result,
            CompensationExecutionOutcome::StorageUncertain(_)
        ));
        assert_eq!(backend.counts().1, 0);
    }
    #[test]
    fn started_resume_probes_without_reinvoke() {
        let f = fixture();
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        let initial = store.create_initial(&f.authority).unwrap();
        let started = initial.start_next().unwrap();
        store.update(&initial, started).unwrap();
        drop(store);
        let backend = FakeBackend::new(
            f.repo.clone(),
            [
                Scan::After,
                Scan::Before,
                Scan::After,
                Scan::Before,
                Scan::After,
                Scan::After,
            ],
        );
        let result = CompensationExecutionEngine::new(backend.clone()).execute(
            CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::default(),
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(backend.counts().1, 1);
    }
    #[test]
    fn resume_explicit_id_mismatch_has_no_effect() {
        let f = fixture();
        let backend = FakeBackend::new(f.repo.clone(), []);
        let wrong = ProposalId::from_str("00000000-0000-4000-8000-000000000002").unwrap();
        let result = CompensationExecutionEngine::new(backend.clone()).execute(
            CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: wrong,
                },
                cancellation: CompensationCancellation::default(),
            },
        );
        assert!(matches!(
            result,
            Err(CompensationExecutionError::Refused(
                CompensationExecutionRefusal::AuthorityMismatch
            ))
        ));
        assert_eq!(backend.counts().1, 0);
    }
    #[test]
    fn full_scan_observes_every_action_before_selecting_earliest_blocker() {
        let (outcome, backend, _) = run([Scan::Before, Scan::Drift]);
        assert!(matches!(
            outcome,
            CompensationExecutionOutcome::NeedsAttention(_)
        ));
        assert_eq!(backend.counts().2, 2);
        assert_eq!(backend.counts().1, 0);
    }
    #[test]
    fn observation_error_is_effect_unknown_after_started() {
        let (outcome, backend, _) = run([Scan::Before, Scan::Before, Scan::Err]);
        assert!(matches!(
            outcome,
            CompensationExecutionOutcome::NeedsAttention(AttentionResult {
                kind: AttentionKind::EffectUnknown,
                ..
            })
        ));
        assert_eq!(backend.counts().1, 1);
    }
    #[test]
    fn started_resume_continues_pending_steps_without_reentry() {
        let f = fixture();
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        let initial = store.create_initial(&f.authority).unwrap();
        let started = initial.start_next().unwrap();
        store.update(&initial, started).unwrap();
        drop(store);
        let backend = FakeBackend::new(
            f.repo.clone(),
            [
                Scan::After,
                Scan::Before,
                Scan::After,
                Scan::Before,
                Scan::After,
                Scan::After,
            ],
        );
        let outcome = CompensationExecutionEngine::new(backend.clone())
            .execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::default(),
            })
            .unwrap();
        assert!(
            matches!(outcome, CompensationExecutionOutcome::Applied(_)),
            "{outcome:?}"
        );
        assert_eq!(backend.counts().1, 1);
    }
    #[test]
    fn invoke_error_with_after_probe_still_applies() {
        let f = fixture();
        let backend = FakeBackend::new(
            f.repo.clone(),
            [
                Scan::Before,
                Scan::Before,
                Scan::After,
                Scan::Before,
                Scan::After,
                Scan::Before,
                Scan::After,
                Scan::After,
            ],
        )
        .with_invoke_error();
        let result =
            execute_with(&f, backend.clone(), CompensationCancellation::default()).unwrap();
        assert!(matches!(result, CompensationExecutionOutcome::Applied(_)));
        assert_eq!(backend.counts().1, 2);
    }
    #[test]
    fn cancellation_flips_during_invoke_after_durable_boundary() {
        let f = fixture();
        let flag = Arc::new(AtomicBool::new(false));
        let backend = FakeBackend::new(
            f.repo.clone(),
            [Scan::Before, Scan::Before, Scan::After, Scan::Before],
        )
        .with_cancel_flip(flag.clone());
        let result = execute_with(
            &f,
            backend.clone(),
            CompensationCancellation::new(move || flag.load(Ordering::SeqCst)),
        )
        .unwrap();
        assert!(matches!(
            result,
            CompensationExecutionOutcome::Cancelled(CancelledResult {
                status: Some(CompensationStatus::Pending),
                revision: Some(2),
                ..
            })
        ));
        assert_eq!(backend.counts().1, 1);
    }
    #[test]
    fn cancellation_after_full_scan_preserves_revision_zero() {
        let f = fixture();
        let flag = Arc::new(AtomicBool::new(false));
        let backend = FakeBackend::new(f.repo.clone(), [Scan::Before, Scan::Before])
            .with_probe_cancel_flip(flag.clone());
        let result = execute_with(
            &f,
            backend,
            CompensationCancellation::new(move || flag.load(Ordering::SeqCst)),
        )
        .unwrap();
        assert!(matches!(
            result,
            CompensationExecutionOutcome::Cancelled(CancelledResult {
                status: Some(CompensationStatus::Pending),
                revision: Some(0),
                ..
            })
        ));
    }
    #[test]
    fn discovery_change_before_lock_has_no_compensation_journal() {
        let f = fixture();
        let changed = RepositoryIdentity {
            common_dir: f.repo.common_dir.clone(),
            primary_root: f.repo.primary_root.clone(),
            repository_oid: crate::lifecycle::ObjectId::new(
                "1111111111111111111111111111111111111111",
            )
            .unwrap(),
        };
        let backend = FakeBackend::new(f.repo.clone(), []).with_discoveries([Ok(changed)]);
        let result = execute_with(&f, backend, CompensationCancellation::default());
        assert!(matches!(
            result,
            Err(CompensationExecutionError::Refused(
                CompensationExecutionRefusal::EvidenceDrift
            ))
        ));
    }
    #[test]
    fn too_large_started_update_is_pre_effect_and_does_not_invoke() {
        let f = fixture();
        let initial = CompensationJournalV1::from_loaded(&f.authority).unwrap();
        let started = initial.start_next().unwrap();
        crate::compensation_store::inject_max_bytes(
            serde_json::to_vec_pretty(&started).unwrap().len() - 1,
        );
        let backend = FakeBackend::new(f.repo.clone(), [Scan::Before, Scan::Before]);
        let result = execute_with(&f, backend.clone(), CompensationCancellation::default());
        assert!(
            matches!(
                result,
                Err(CompensationExecutionError::Refused(
                    CompensationExecutionRefusal::TooLarge
                ))
            ),
            "{result:?}"
        );
        assert_eq!(backend.counts().1, 0);
        assert_eq!(started.revision(), 1);
    }
    #[test]
    fn too_large_terminal_update_is_post_effect_with_durable_started() {
        let f = fixture();
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        let initial = store.create_initial(&f.authority).unwrap();
        let first_started = initial.start_next().unwrap();
        store.update(&initial, first_started.clone()).unwrap();
        let first_applied = first_started.apply_started().unwrap();
        store.update(&first_started, first_applied.clone()).unwrap();
        let second_started = first_applied.start_next().unwrap();
        store
            .update(&first_applied, second_started.clone())
            .unwrap();
        drop(store);
        crate::compensation_store::inject_max_bytes(
            serde_json::to_vec_pretty(&second_started).unwrap().len(),
        );
        let backend = FakeBackend::new(f.repo.clone(), [Scan::After, Scan::Drift]);
        let result = CompensationExecutionEngine::new(backend.clone()).execute(
            CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::default(),
            },
        );
        assert!(
            matches!(result, Err(CompensationExecutionError::Storage(_))),
            "{result:?}"
        );
        assert_eq!(backend.counts().1, 0);
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        assert_eq!(
            store
                .read(&f.authority.proposal().proposal_id)
                .unwrap()
                .status(),
            CompensationStatus::Running
        );
        drop(store);
        let backend = FakeBackend::new(f.repo.clone(), [Scan::After, Scan::Drift]);
        let resumed = CompensationExecutionEngine::new(backend.clone())
            .execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::default(),
            })
            .unwrap();
        assert!(matches!(
            resumed,
            CompensationExecutionOutcome::NeedsAttention(_)
        ));
        assert_eq!(backend.counts().1, 0);
    }
    #[test]
    fn commit_uncertain_create_and_started_update_reread_matching_authority() {
        let f = fixture();
        crate::compensation_store::inject_fault(
            crate::compensation_store::StoreFault::InitialParentSync,
        );
        let backend = FakeBackend::new(f.repo.clone(), []);
        let result =
            execute_with(&f, backend.clone(), CompensationCancellation::default()).unwrap();
        assert!(matches!(
            result,
            CompensationExecutionOutcome::StorageUncertain(StorageUncertainResult {
                observed: Some(DurableCompensationState { revision: 0, .. }),
                ..
            })
        ));
        let f = fixture();
        crate::compensation_store::inject_fault(
            crate::compensation_store::StoreFault::UpdateParentSync,
        );
        let backend = FakeBackend::new(f.repo.clone(), [Scan::Before, Scan::Before]);
        let result =
            execute_with(&f, backend.clone(), CompensationCancellation::default()).unwrap();
        assert!(matches!(
            result,
            CompensationExecutionOutcome::StorageUncertain(StorageUncertainResult {
                observed: Some(DurableCompensationState { revision: 1, .. }),
                ..
            })
        ));
        assert_eq!(backend.counts().1, 0);
    }
    #[test]
    fn commit_uncertain_terminal_rereads_applied_without_retry() {
        let f = fixture();
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        let initial = store.create_initial(&f.authority).unwrap();
        let first_started = initial.start_next().unwrap();
        store.update(&initial, first_started.clone()).unwrap();
        let first_applied = first_started.apply_started().unwrap();
        store.update(&first_started, first_applied.clone()).unwrap();
        let second_started = first_applied.start_next().unwrap();
        store.update(&first_applied, second_started).unwrap();
        drop(store);
        crate::compensation_store::inject_fault(
            crate::compensation_store::StoreFault::UpdateParentSync,
        );
        let backend = FakeBackend::new(f.repo.clone(), [Scan::After, Scan::After]);
        let result = CompensationExecutionEngine::new(backend.clone())
            .execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::default(),
            })
            .unwrap();
        assert!(matches!(
            result,
            CompensationExecutionOutcome::StorageUncertain(StorageUncertainResult {
                observed: Some(DurableCompensationState {
                    status: CompensationStatus::Applied,
                    revision: 4,
                    ..
                }),
                ..
            })
        ));
        assert_eq!(backend.counts().1, 0);
    }
    #[test]
    fn uncertain_reread_rejects_mismatched_authority_and_corrupt_bytes() {
        let f = fixture();
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        store.create_initial(&f.authority).unwrap();
        let other = fixture();
        let engine = CompensationExecutionEngine::new(FakeBackend::new(f.repo.clone(), []));
        let mismatch =
            engine.uncertain(&store, &other.authority, f.authority.proposal().proposal_id);
        assert!(matches!(
            mismatch,
            CompensationExecutionOutcome::StorageUncertain(StorageUncertainResult {
                observed: None,
                ..
            })
        ));
        let path = f
            .repo
            .common_dir
            .as_path()
            .join("ewtm/compensation/v1/operations")
            .join(format!("{}.json", f.authority.proposal().proposal_id));
        fs::write(path, b"corrupt").unwrap();
        let corrupt = engine.uncertain(&store, &f.authority, f.authority.proposal().proposal_id);
        assert!(matches!(
            corrupt,
            CompensationExecutionOutcome::StorageUncertain(StorageUncertainResult {
                observed: None,
                ..
            })
        ));
    }
    #[test]
    fn resume_pending_global_drift_is_durable_pre_started_attention() {
        let f = fixture();
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        store.create_initial(&f.authority).unwrap();
        drop(store);
        let changed = RepositoryIdentity {
            common_dir: f.repo.common_dir.clone(),
            primary_root: f.repo.primary_root.clone(),
            repository_oid: crate::lifecycle::ObjectId::new(
                "1111111111111111111111111111111111111111",
            )
            .unwrap(),
        };
        let backend = FakeBackend::new(f.repo.clone(), [])
            .with_discoveries([Ok(f.repo.clone()), Ok(changed)]);
        let result = CompensationExecutionEngine::new(backend)
            .execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::default(),
            })
            .unwrap();
        assert!(matches!(
            result,
            CompensationExecutionOutcome::NeedsAttention(AttentionResult {
                kind: AttentionKind::EvidenceDrift,
                state: DurableCompensationState { revision: 1, .. },
                ..
            })
        ));
    }
    #[test]
    fn resume_started_global_drift_probes_and_persists_after_started_attention() {
        let f = fixture();
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        let initial = store.create_initial(&f.authority).unwrap();
        let started = initial.start_next().unwrap();
        store.update(&initial, started).unwrap();
        drop(store);
        let backend = FakeBackend::new(f.repo.clone(), [Scan::After, Scan::Before])
            .with_discoveries([Ok(f.repo.clone()), Err(())]);
        let result = CompensationExecutionEngine::new(backend.clone())
            .execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::default(),
            })
            .unwrap();
        assert!(matches!(
            result,
            CompensationExecutionOutcome::NeedsAttention(AttentionResult {
                kind: AttentionKind::EvidenceDrift,
                state: DurableCompensationState { revision: 2, .. },
                ..
            })
        ));
        assert_eq!(backend.counts().1, 0);
        assert_eq!(backend.counts().2, 2);
    }
    #[test]
    fn applied_global_drift_is_frozen_and_refused() {
        let f = fixture();
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        let initial = store.create_initial(&f.authority).unwrap();
        let first = initial.start_next().unwrap();
        store.update(&initial, first.clone()).unwrap();
        let first = first.apply_started().unwrap();
        store
            .update(
                &store.read(&f.authority.proposal().proposal_id).unwrap(),
                first.clone(),
            )
            .unwrap();
        let second = first.start_next().unwrap();
        store.update(&first, second.clone()).unwrap();
        let applied = second.apply_started().unwrap();
        store.update(&second, applied).unwrap();
        drop(store);
        let changed = RepositoryIdentity {
            common_dir: f.repo.common_dir.clone(),
            primary_root: f.repo.primary_root.clone(),
            repository_oid: crate::lifecycle::ObjectId::new(
                "1111111111111111111111111111111111111111",
            )
            .unwrap(),
        };
        let backend = FakeBackend::new(f.repo.clone(), [])
            .with_discoveries([Ok(f.repo.clone()), Ok(changed)]);
        let result =
            CompensationExecutionEngine::new(backend).execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::default(),
            });
        assert!(matches!(
            result,
            Err(CompensationExecutionError::Refused(
                CompensationExecutionRefusal::EvidenceDrift
            ))
        ));
    }
    #[test]
    fn already_cancelled_started_resume_still_probes_and_persists() {
        let f = fixture();
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        let initial = store.create_initial(&f.authority).unwrap();
        let started = initial.start_next().unwrap();
        store.update(&initial, started).unwrap();
        drop(store);
        let backend = FakeBackend::new(f.repo.clone(), [Scan::After, Scan::Before]);
        let result = CompensationExecutionEngine::new(backend.clone())
            .execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::new(|| true),
            })
            .unwrap();
        assert!(matches!(
            result,
            CompensationExecutionOutcome::Cancelled(CancelledResult {
                status: Some(CompensationStatus::Pending),
                revision: Some(2),
                ..
            })
        ));
        assert_eq!(backend.counts().2, 2);
    }
    #[test]
    fn resume_pending_global_drift_already_cancelled_keeps_pending_bytes() {
        let f = fixture();
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        store.create_initial(&f.authority).unwrap();
        drop(store);
        let path = f
            .repo
            .common_dir
            .as_path()
            .join("ewtm/compensation/v1/operations")
            .join(format!("{}.json", f.authority.proposal().proposal_id));
        let before = fs::read(&path).unwrap();
        let changed = RepositoryIdentity {
            common_dir: f.repo.common_dir.clone(),
            primary_root: f.repo.primary_root.clone(),
            repository_oid: crate::lifecycle::ObjectId::new(
                "1111111111111111111111111111111111111111",
            )
            .unwrap(),
        };
        let backend = FakeBackend::new(f.repo.clone(), [])
            .with_discoveries([Ok(f.repo.clone()), Ok(changed)]);
        let result = CompensationExecutionEngine::new(backend.clone())
            .execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::new(|| true),
            })
            .unwrap();
        assert!(matches!(
            result,
            CompensationExecutionOutcome::Cancelled(CancelledResult {
                status: Some(CompensationStatus::Pending),
                revision: Some(0),
                ..
            })
        ));
        assert_eq!(backend.counts().1, 0);
        assert_eq!(fs::read(path).unwrap(), before);
    }
    #[test]
    fn later_step_global_drift_cancellation_keeps_pending_revision_and_bytes() {
        let f = fixture();
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        let initial = store.create_initial(&f.authority).unwrap();
        let first_started = initial.start_next().unwrap();
        store.update(&initial, first_started.clone()).unwrap();
        let first_applied = first_started.apply_started().unwrap();
        store.update(&first_started, first_applied).unwrap();
        drop(store);
        let path = f
            .repo
            .common_dir
            .as_path()
            .join("ewtm/compensation/v1/operations")
            .join(format!("{}.json", f.authority.proposal().proposal_id));
        let before = fs::read(&path).unwrap();
        let changed = RepositoryIdentity {
            common_dir: f.repo.common_dir.clone(),
            primary_root: f.repo.primary_root.clone(),
            repository_oid: crate::lifecycle::ObjectId::new(
                "1111111111111111111111111111111111111111",
            )
            .unwrap(),
        };
        let flag = Arc::new(AtomicBool::new(false));
        let backend = FakeBackend::new(f.repo.clone(), [Scan::After, Scan::Before])
            .with_discoveries([Ok(f.repo.clone()), Ok(f.repo.clone()), Ok(changed)])
            .with_discovery_cancel_flip(3, flag.clone());
        let result = CompensationExecutionEngine::new(backend.clone())
            .execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::new(move || flag.load(Ordering::SeqCst)),
            })
            .unwrap();
        assert!(matches!(
            result,
            CompensationExecutionOutcome::Cancelled(CancelledResult {
                status: Some(CompensationStatus::Pending),
                revision: Some(2),
                ..
            })
        ));
        assert_eq!(backend.counts().1, 0);
        assert_eq!(fs::read(path).unwrap(), before);
    }
    #[test]
    fn started_resume_post_global_drift_probes_all_and_persists_after_started_attention() {
        let f = fixture();
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        let initial = store.create_initial(&f.authority).unwrap();
        let started = initial.start_next().unwrap();
        store.update(&initial, started).unwrap();
        drop(store);
        let changed = RepositoryIdentity {
            common_dir: f.repo.common_dir.clone(),
            primary_root: f.repo.primary_root.clone(),
            repository_oid: crate::lifecycle::ObjectId::new(
                "1111111111111111111111111111111111111111",
            )
            .unwrap(),
        };
        let backend = FakeBackend::new(f.repo.clone(), [Scan::After, Scan::Before])
            .with_discoveries([
                Ok(f.repo.clone()),
                Ok(f.repo.clone()),
                Ok(f.repo.clone()),
                Ok(changed),
            ]);
        let result = CompensationExecutionEngine::new(backend.clone())
            .execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::default(),
            })
            .unwrap();
        assert!(matches!(
            result,
            CompensationExecutionOutcome::NeedsAttention(AttentionResult {
                kind: AttentionKind::EvidenceDrift,
                state: DurableCompensationState { revision: 2, .. },
                ..
            })
        ));
        assert_eq!(backend.counts().1, 0);
        assert_eq!(backend.counts().2, 2);
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        assert!(
            store
                .read(&f.authority.proposal().proposal_id)
                .unwrap()
                .current_step()
                .unwrap()
                .attention
                .as_ref()
                .unwrap()
                .after_started
        );
    }
    #[test]
    fn full_durable_invoke_ok_err_probe_table_normalizes_started_action() {
        for invoke_error in [false, true] {
            for probe in [Scan::After, Scan::Before, Scan::Drift, Scan::Err] {
                let f = fixture();
                let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
                let initial = store.create_initial(&f.authority).unwrap();
                let first_started = initial.start_next().unwrap();
                store.update(&initial, first_started.clone()).unwrap();
                let first_applied = first_started.apply_started().unwrap();
                store.update(&first_started, first_applied.clone()).unwrap();
                let second_started = first_applied.start_next().unwrap();
                store
                    .update(&first_applied, second_started.clone())
                    .unwrap();
                let backend = FakeBackend::new(f.repo.clone(), [Scan::After, probe]);
                let backend = if invoke_error {
                    backend.with_invoke_error()
                } else {
                    backend
                };
                let engine = CompensationExecutionEngine::new(backend.clone());
                let context = ExecutionContext {
                    anchor: &f.anchor,
                    authority: &f.authority,
                    store: &store,
                    cancel: &CompensationCancellation::default(),
                };
                engine.invoke(&f.authority.proposal().steps[1].action);
                let result = engine
                    .reconcile_started(&context, second_started, 1, true, false)
                    .unwrap();
                match probe {
                    Scan::After => assert!(matches!(
                        result,
                        StartedReconciliation::Done(CompensationExecutionOutcome::Applied(_))
                    )),
                    Scan::Before => assert!(matches!(
                        result,
                        StartedReconciliation::Done(CompensationExecutionOutcome::NeedsAttention(
                            AttentionResult {
                                kind: AttentionKind::EffectNotApplied,
                                ..
                            }
                        ))
                    )),
                    Scan::Drift | Scan::Err => assert!(matches!(
                        result,
                        StartedReconciliation::Done(CompensationExecutionOutcome::NeedsAttention(
                            AttentionResult {
                                kind: AttentionKind::EffectUnknown,
                                ..
                            }
                        ))
                    )),
                }
                assert_eq!(backend.counts().1, 1);
                assert_eq!(backend.counts().2, 2);
                assert_eq!(
                    backend.invocation_log.lock().unwrap().as_slice(),
                    ["branch"]
                );
            }
        }
    }
    #[test]
    fn needs_attention_global_drift_is_frozen_and_refused() {
        let f = fixture();
        let store = LockedCompensationStore::acquire(f.repo.common_dir.as_path()).unwrap();
        let initial = store.create_initial(&f.authority).unwrap();
        let attention = initial
            .attention(AttentionKind::PreStartedAbsent, false, Some(0))
            .unwrap();
        store.update(&initial, attention).unwrap();
        drop(store);
        let changed = RepositoryIdentity {
            common_dir: f.repo.common_dir.clone(),
            primary_root: f.repo.primary_root.clone(),
            repository_oid: crate::lifecycle::ObjectId::new(
                "1111111111111111111111111111111111111111",
            )
            .unwrap(),
        };
        let backend = FakeBackend::new(f.repo.clone(), [])
            .with_discoveries([Ok(f.repo.clone()), Ok(changed)]);
        let result =
            CompensationExecutionEngine::new(backend).execute(CompensationExecutionRequest {
                anchor: &f.anchor,
                mode: CompensationExecutionMode::Resume {
                    authority: &f.authority,
                    proposal_id: f.authority.proposal().proposal_id,
                },
                cancellation: CompensationCancellation::default(),
            });
        assert!(matches!(
            result,
            Err(CompensationExecutionError::Refused(
                CompensationExecutionRefusal::EvidenceDrift
            ))
        ));
    }
    #[test]
    fn primitive_dispatch_normalizes_all_probe_states_for_all_actions() {
        let f = fixture();
        let plan = crate::lifecycle::v3_test_plan(2);
        let worktree = match plan.steps()[0].compensation().as_ref().unwrap() {
            Compensation::RemoveCreatedWorktree(value) => {
                CompensationActionV1::RemoveCreatedWorktree(value.clone())
            }
            _ => unreachable!(),
        };
        let artifact = match plan.steps()[1].compensation().as_ref().unwrap() {
            Compensation::RemoveCreatedArtifactV3(value) => {
                CompensationActionV1::RemoveCreatedArtifactV3(value.clone())
            }
            _ => unreachable!(),
        };
        let branch = match &worktree {
            CompensationActionV1::RemoveCreatedWorktree(value) => {
                CompensationActionV1::DeleteCreatedLocalBranch(CreatedLocalBranch {
                    branch: value.branch.clone(),
                    expected_oid: value.expected_oid.clone(),
                })
            }
            _ => unreachable!(),
        };
        for action in [artifact, worktree, branch] {
            for scan in [Scan::Before, Scan::After, Scan::Drift, Scan::Err] {
                let backend = FakeBackend::new(f.repo.clone(), [scan]).with_invoke_error();
                let engine = CompensationExecutionEngine::new(backend.clone());
                engine.invoke(&action);
                let expected = if scan == Scan::Err { Scan::Drift } else { scan };
                assert_eq!(engine.observe(&action), expected);
                assert_eq!(backend.counts().1, 1);
                assert_eq!(backend.counts().2, 1);
                assert_eq!(backend.invocation_log.lock().unwrap().len(), 1);
            }
        }
    }
}
