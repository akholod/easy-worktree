use crate::{
    compensation::{CompensationProposalV1, ProposalId, Sha256Digest},
    lifecycle::StepId,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompensationStatus {
    Pending,
    Running,
    NeedsAttention,
    Applied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompensationStepStatus {
    Pending,
    Started,
    Applied,
    NeedsAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    EvidenceDrift,
    PreStartedAbsent,
    EffectNotApplied,
    EffectUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attention {
    pub kind: AttentionKind,
    pub after_started: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_step_index: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompensationStep {
    pub index: u32,
    pub forward_step_id: StepId,
    pub status: CompensationStepStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attention: Option<Attention>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompensationJournalV1 {
    compensation_journal_schema_version: u8,
    revision: u64,
    proposal_id: ProposalId,
    proposal_sha256: Sha256Digest,
    proposal: CompensationProposalV1,
    status: CompensationStatus,
    steps: Vec<CompensationStep>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    compensation_journal_schema_version: u8,
    revision: u64,
    proposal_id: ProposalId,
    proposal_sha256: Sha256Digest,
    proposal: CompensationProposalV1,
    status: CompensationStatus,
    steps: Vec<CompensationStep>,
}

impl<'de> Deserialize<'de> for CompensationJournalV1 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let w = Wire::deserialize(d)?;
        let value = Self {
            compensation_journal_schema_version: w.compensation_journal_schema_version,
            revision: w.revision,
            proposal_id: w.proposal_id,
            proposal_sha256: w.proposal_sha256,
            proposal: w.proposal,
            status: w.status,
            steps: w.steps,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

impl CompensationJournalV1 {
    pub fn from_loaded(
        loaded: &crate::compensation_authority::LoadedProposal,
    ) -> Result<Self, String> {
        Self::construct(loaded.proposal().clone(), loaded.raw_sha256().clone())
    }

    #[cfg(test)]
    pub(crate) fn new(
        proposal: CompensationProposalV1,
        digest: Sha256Digest,
    ) -> Result<Self, String> {
        Self::construct(proposal, digest)
    }

    fn construct(proposal: CompensationProposalV1, digest: Sha256Digest) -> Result<Self, String> {
        proposal.validate()?;
        let steps = proposal
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                Ok(CompensationStep {
                    index: u32::try_from(index).map_err(|_| "too many compensation steps")?,
                    forward_step_id: step.forward_step_id.clone(),
                    status: CompensationStepStatus::Pending,
                    attention: None,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let value = Self {
            compensation_journal_schema_version: 1,
            revision: 0,
            proposal_id: proposal.proposal_id,
            proposal_sha256: digest,
            proposal,
            status: CompensationStatus::Pending,
            steps,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn schema_version(&self) -> u8 {
        self.compensation_journal_schema_version
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn proposal_id(&self) -> &ProposalId {
        &self.proposal_id
    }
    pub fn proposal_sha256(&self) -> &Sha256Digest {
        &self.proposal_sha256
    }
    pub fn proposal(&self) -> &CompensationProposalV1 {
        &self.proposal
    }
    pub fn status(&self) -> CompensationStatus {
        self.status
    }
    pub fn steps(&self) -> &[CompensationStep] {
        &self.steps
    }

    pub fn is_canonical_initial(&self) -> bool {
        self.revision == 0
            && self.status == CompensationStatus::Pending
            && self.steps.iter().all(|step| {
                step.status == CompensationStepStatus::Pending && step.attention.is_none()
            })
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.compensation_journal_schema_version != 1
            || self.proposal_id != self.proposal.proposal_id
        {
            return Err("journal identity mismatch".into());
        }
        self.proposal.validate()?;
        if self.steps.len() != self.proposal.steps.len() {
            return Err("step count mismatch".into());
        }
        let mut active = false;
        let mut attention = false;
        let mut pending = false;
        for (position, step) in self.steps.iter().enumerate() {
            if step.index != u32::try_from(position).map_err(|_| "step index overflow")?
                || step.forward_step_id != self.proposal.steps[position].forward_step_id
            {
                return Err("step binding mismatch".into());
            }
            match step.status {
                CompensationStepStatus::Applied if pending || active || attention => {
                    return Err("invalid step order".into());
                }
                CompensationStepStatus::Applied => {
                    if step.attention.is_some() {
                        return Err("attention on applied step".into());
                    }
                }
                CompensationStepStatus::Pending => {
                    pending = true;
                    if step.attention.is_some() {
                        return Err("attention on pending step".into());
                    }
                }
                CompensationStepStatus::Started => {
                    if pending || active || attention || step.attention.is_some() {
                        return Err("invalid started step".into());
                    };
                    active = true;
                }
                CompensationStepStatus::NeedsAttention => {
                    if pending || active || attention || step.attention.is_none() {
                        return Err("invalid attention step".into());
                    };
                    attention = true;
                }
            }
            if let Some(value) = &step.attention {
                if step.status != CompensationStepStatus::NeedsAttention {
                    return Err("attention on inactive step".into());
                }
                if value
                    .observed_step_index
                    .is_some_and(|i| usize::try_from(i).map_or(true, |i| i >= self.steps.len()))
                {
                    return Err("attention index is out of range".into());
                }
                let valid_kind = match value.kind {
                    AttentionKind::PreStartedAbsent => !value.after_started,
                    AttentionKind::EffectNotApplied | AttentionKind::EffectUnknown => {
                        value.after_started
                    }
                    AttentionKind::EvidenceDrift => true,
                };
                if !valid_kind {
                    return Err("attention kind has invalid phase".into());
                }
            }
        }
        let expected = if attention {
            CompensationStatus::NeedsAttention
        } else if active {
            CompensationStatus::Running
        } else if self
            .steps
            .iter()
            .all(|s| s.status == CompensationStepStatus::Applied)
        {
            CompensationStatus::Applied
        } else {
            CompensationStatus::Pending
        };
        if self.status != expected {
            return Err("status mismatch".into());
        }
        let applied = self
            .steps
            .iter()
            .filter(|s| s.status == CompensationStepStatus::Applied)
            .count() as u64;
        let expected_revision = match self.status {
            CompensationStatus::Pending => applied.checked_mul(2),
            CompensationStatus::Running => applied.checked_mul(2).and_then(|v| v.checked_add(1)),
            CompensationStatus::NeedsAttention => self
                .steps
                .iter()
                .find(|s| s.status == CompensationStepStatus::NeedsAttention)
                .and_then(|s| s.attention.as_ref())
                .and_then(|a| {
                    if a.after_started {
                        applied.checked_mul(2).and_then(|v| v.checked_add(2))
                    } else {
                        applied.checked_mul(2).and_then(|v| v.checked_add(1))
                    }
                }),
            CompensationStatus::Applied => applied.checked_mul(2),
        }
        .ok_or("revision overflow")?;
        if self.revision != expected_revision {
            return Err("revision does not match state".into());
        }
        Ok(())
    }

    pub fn start_next(&self) -> Result<Self, String> {
        let index = self
            .steps
            .iter()
            .position(|s| s.status == CompensationStepStatus::Pending)
            .ok_or("no pending step")?;
        let mut next = self.clone();
        next.revision = self.revision.checked_add(1).ok_or("revision overflow")?;
        next.steps[index].status = CompensationStepStatus::Started;
        next.status = CompensationStatus::Running;
        next.validate()?;
        Ok(next)
    }
    pub fn apply_started(&self) -> Result<Self, String> {
        let index = self
            .steps
            .iter()
            .position(|s| s.status == CompensationStepStatus::Started)
            .ok_or("no started step")?;
        let mut next = self.clone();
        next.revision = self.revision.checked_add(1).ok_or("revision overflow")?;
        next.steps[index].status = CompensationStepStatus::Applied;
        next.status = if next
            .steps
            .iter()
            .all(|s| s.status == CompensationStepStatus::Applied)
        {
            CompensationStatus::Applied
        } else {
            CompensationStatus::Pending
        };
        next.validate()?;
        Ok(next)
    }
    pub fn attention(
        &self,
        kind: AttentionKind,
        after_started: bool,
        observed_step_index: Option<u32>,
    ) -> Result<Self, String> {
        let status = if after_started {
            CompensationStepStatus::Started
        } else {
            CompensationStepStatus::Pending
        };
        let index = self
            .steps
            .iter()
            .position(|s| s.status == status)
            .ok_or("no eligible step")?;
        let mut next = self.clone();
        next.revision = self.revision.checked_add(1).ok_or("revision overflow")?;
        next.steps[index].status = CompensationStepStatus::NeedsAttention;
        next.steps[index].attention = Some(Attention {
            kind,
            after_started,
            observed_step_index,
        });
        next.status = CompensationStatus::NeedsAttention;
        next.validate()?;
        Ok(next)
    }
    pub fn successor(&self, next: Self) -> Result<Self, String> {
        self.validate()?;
        next.validate()?;
        if next.proposal_id != self.proposal_id
            || next.proposal != self.proposal
            || next.proposal_sha256 != self.proposal_sha256
            || next.revision != self.revision.checked_add(1).ok_or("revision overflow")?
        {
            return Err("immutable or revision mismatch".into());
        }
        if matches!(
            self.status,
            CompensationStatus::Applied | CompensationStatus::NeedsAttention
        ) {
            return Err("terminal journal frozen".into());
        }
        if legal(self, &next) {
            Ok(next)
        } else {
            Err("illegal successor".into())
        }
    }
}

fn legal(previous: &CompensationJournalV1, next: &CompensationJournalV1) -> bool {
    let changed = previous
        .steps
        .iter()
        .zip(&next.steps)
        .enumerate()
        .filter(|(_, (left, right))| left != right)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if changed.len() != 1 {
        return false;
    }
    let index = changed[0];
    let first_pending = previous
        .steps
        .iter()
        .position(|step| step.status == CompensationStepStatus::Pending);
    match (
        previous.status,
        next.status,
        previous.steps[index].status,
        next.steps[index].status,
    ) {
        (
            CompensationStatus::Pending,
            CompensationStatus::Running,
            CompensationStepStatus::Pending,
            CompensationStepStatus::Started,
        ) => first_pending == Some(index),
        (
            CompensationStatus::Running,
            CompensationStatus::Pending | CompensationStatus::Applied,
            CompensationStepStatus::Started,
            CompensationStepStatus::Applied,
        ) => true,
        (
            CompensationStatus::Pending,
            CompensationStatus::NeedsAttention,
            CompensationStepStatus::Pending,
            CompensationStepStatus::NeedsAttention,
        ) => {
            first_pending == Some(index)
                && next.steps[index]
                    .attention
                    .as_ref()
                    .is_some_and(|a| !a.after_started)
        }
        (
            CompensationStatus::Running,
            CompensationStatus::NeedsAttention,
            CompensationStepStatus::Started,
            CompensationStepStatus::NeedsAttention,
        ) => next.steps[index]
            .attention
            .as_ref()
            .is_some_and(|a| a.after_started),
        _ => false,
    }
}

pub type CompensationStepState = CompensationStepStatus;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compensation::*;
    use crate::domain::StoredPath;
    use crate::lifecycle::*;
    use std::str::FromStr;

    pub(crate) fn test_sample() -> CompensationProposalV1 {
        let id = ProposalId::from_str("00000000-0000-4000-8000-000000000000").unwrap();
        let digest = Sha256Digest::new("a".repeat(64)).unwrap();
        let step = CompensationProposalStepV1 {
            forward_step_id: StepId::new("s").unwrap(),
            action: CompensationActionV1::RemoveCreatedWorktree(CreatedWorktree {
                path: StoredPath::new("/repo/w".into()),
                branch: BranchName::new("b").unwrap(),
                expected_oid: ObjectId::new("0123456789012345678901234567890123456789").unwrap(),
                branch_was_created: false,
            }),
        };
        CompensationProposalV1 {
            proposal_schema_version: 1,
            proposal_id: id,
            executable: false,
            repository: RepositoryIdentity {
                common_dir: StoredPath::new("/repo/.git".into()),
                primary_root: StoredPath::new("/repo".into()),
                repository_oid: ObjectId::new("0123456789012345678901234567890123456789").unwrap(),
            },
            source: CompensationProposalSourceV1 {
                operation_id: OperationId::new(uuid::Uuid::new_v4()),
                plan_schema_version: 3,
                journal_schema_version: 1,
                journal_revision: 2,
                forward_plan_digest: digest.clone(),
                forward_journal_digest: digest,
            },
            allowed_categories: vec![CompensationAllowanceV1::Worktree],
            steps: vec![step],
        }
    }
    pub(crate) fn test_three_step() -> CompensationProposalV1 {
        let mut proposal = test_sample();
        proposal.allowed_categories = vec![
            CompensationAllowanceV1::FileArtifact,
            CompensationAllowanceV1::Worktree,
        ];
        for (name, file) in [("a", "a.txt"), ("b", "b.txt")] {
            proposal.steps.push(CompensationProposalStepV1 {
                forward_step_id: StepId::new(name).unwrap(),
                action: CompensationActionV1::RemoveCreatedArtifactV3(CreatedArtifactV3 {
                    path: StoredPath::new(format!("/repo/w/{file}").into()),
                    expected: ArtifactStateV3::Regular(RegularFileStateV3 {
                        bytes: 1,
                        digest: ObjectId::new("0123456789012345678901234567890123456789").unwrap(),
                        mode: 0o600,
                    }),
                    staging: None,
                }),
            });
        }
        proposal
    }
    #[test]
    fn initial_and_terminal_revisions_are_exact() {
        let j =
            CompensationJournalV1::new(test_sample(), Sha256Digest::new("b".repeat(64)).unwrap())
                .unwrap();
        let j = j.start_next().unwrap();
        assert_eq!(j.revision, 1);
        let j = j.apply_started().unwrap();
        assert_eq!(j.status, CompensationStatus::Applied);
        assert_eq!(j.revision, 2);
        assert!(j.start_next().is_err());
    }
    #[test]
    fn attention_is_typed_and_terminal() {
        let j =
            CompensationJournalV1::new(test_sample(), Sha256Digest::new("b".repeat(64)).unwrap())
                .unwrap();
        let j = j
            .attention(AttentionKind::PreStartedAbsent, false, Some(0))
            .unwrap();
        assert_eq!(j.revision, 1);
        assert!(
            j.attention(AttentionKind::EffectUnknown, false, None)
                .is_err()
        );
    }
    #[test]
    fn serde_rejects_unknown_and_bad_revision() {
        let j =
            CompensationJournalV1::new(test_sample(), Sha256Digest::new("b".repeat(64)).unwrap())
                .unwrap();
        let mut v = serde_json::to_value(j).unwrap();
        v["extra"] = true.into();
        assert!(serde_json::from_value::<CompensationJournalV1>(v).is_err());
    }
    #[test]
    fn successor_rejects_immutable_mutation() {
        let j =
            CompensationJournalV1::new(test_sample(), Sha256Digest::new("b".repeat(64)).unwrap())
                .unwrap();
        let mut n = j.start_next().unwrap();
        n.proposal_sha256 = Sha256Digest::new("c".repeat(64)).unwrap();
        assert!(j.successor(n).is_err());
    }
    #[test]
    fn three_step_lifecycle_has_exact_revision_and_successors() {
        let initial = CompensationJournalV1::new(
            test_three_step(),
            Sha256Digest::new("b".repeat(64)).unwrap(),
        )
        .unwrap();
        let started = initial.start_next().unwrap();
        assert!(initial.successor(started.clone()).is_ok());
        let pending = started.apply_started().unwrap();
        assert_eq!(
            (pending.revision, pending.status),
            (2, CompensationStatus::Pending)
        );
        let started = pending.start_next().unwrap();
        let pending = started.apply_started().unwrap();
        assert_eq!(
            (pending.revision, pending.status),
            (4, CompensationStatus::Pending)
        );
        let started = pending.start_next().unwrap();
        let applied = started.apply_started().unwrap();
        assert_eq!(
            (applied.revision, applied.status),
            (6, CompensationStatus::Applied)
        );
        assert!(pending.successor(started).is_ok());
        assert!(applied.start_next().is_err());
    }
    #[test]
    fn direct_successor_matrix_rejects_order_revision_and_attention_mutations() {
        let initial = CompensationJournalV1::new(
            test_three_step(),
            Sha256Digest::new("b".repeat(64)).unwrap(),
        )
        .unwrap();
        let mut forged = initial.start_next().unwrap();
        forged.revision += 1;
        assert!(initial.successor(forged).is_err());
        let mut forged = initial.start_next().unwrap();
        forged.steps[1] = forged.steps[0].clone();
        assert!(initial.successor(forged).is_err());
        let started = initial.start_next().unwrap();
        let mut forged = started.apply_started().unwrap();
        forged.status = CompensationStatus::Applied;
        assert!(started.successor(forged).is_err());
        assert!(
            initial
                .attention(AttentionKind::EffectUnknown, false, None)
                .is_err()
        );
        assert!(
            started
                .attention(AttentionKind::PreStartedAbsent, true, None)
                .is_err()
        );
        assert!(
            started
                .attention(AttentionKind::EffectUnknown, true, Some(99))
                .is_err()
        );
    }

    #[test]
    fn malformed_order_and_attention_matrix_is_rejected() {
        let initial = CompensationJournalV1::new(
            test_three_step(),
            Sha256Digest::new("b".repeat(64)).unwrap(),
        )
        .unwrap();
        let started = initial.start_next().unwrap();
        let mut started_then_attention = started.clone();
        started_then_attention.steps[1].status = CompensationStepStatus::NeedsAttention;
        started_then_attention.steps[1].attention = Some(Attention {
            kind: AttentionKind::EffectUnknown,
            after_started: true,
            observed_step_index: Some(1),
        });
        assert!(started_then_attention.validate().is_err());

        let mut multiple_started = started.clone();
        multiple_started.steps[1].status = CompensationStepStatus::Started;
        assert!(multiple_started.validate().is_err());

        let attention = initial
            .attention(AttentionKind::PreStartedAbsent, false, Some(0))
            .unwrap();
        let mut multiple_attention = attention.clone();
        multiple_attention.steps[1].status = CompensationStepStatus::NeedsAttention;
        multiple_attention.steps[1].attention = Some(Attention {
            kind: AttentionKind::PreStartedAbsent,
            after_started: false,
            observed_step_index: Some(1),
        });
        assert!(multiple_attention.validate().is_err());

        let mut pending_before_started = started.clone();
        pending_before_started.steps[0].status = CompensationStepStatus::Pending;
        assert!(pending_before_started.validate().is_err());
        let mut pending_before_applied = started.apply_started().unwrap();
        pending_before_applied.steps[0].status = CompensationStepStatus::Pending;
        assert!(pending_before_applied.validate().is_err());

        let mut attention_on_applied = started.apply_started().unwrap();
        attention_on_applied.steps[0].attention = Some(Attention {
            kind: AttentionKind::EffectUnknown,
            after_started: true,
            observed_step_index: Some(0),
        });
        assert!(attention_on_applied.validate().is_err());

        let mut overflow = initial.clone();
        overflow.revision = u64::MAX;
        assert!(overflow.validate().is_err());
        assert!(initial.start_next().unwrap().start_next().is_err());
        assert!(attention.clone().successor(attention).is_err());
        let applied = initial.start_next().unwrap().apply_started().unwrap();
        assert!(applied.clone().successor(applied).is_err());
    }
}

#[cfg(test)]
pub(crate) use tests::test_sample;
#[cfg(test)]
pub(crate) use tests::test_three_step;
