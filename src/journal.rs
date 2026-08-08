use crate::lifecycle::{OperationId, OperationPlan, StepId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Pending,
    Running,
    NeedsAttention,
    Applied,
    Failed,
}
impl OperationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::NeedsAttention => "needs_attention",
            Self::Applied => "applied",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Started,
    Applied,
    NeedsAttention,
}
impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Started => "started",
            Self::Applied => "applied",
            Self::NeedsAttention => "needs_attention",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalStep {
    id: StepId,
    status: StepStatus,
}
impl JournalStep {
    pub fn id(&self) -> &StepId {
        &self.id
    }
    pub fn status(&self) -> StepStatus {
        self.status
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Journal {
    schema_version: u8,
    revision: u64,
    operation_id: OperationId,
    plan: OperationPlan,
    status: OperationStatus,
    steps: Vec<JournalStep>,
}
impl Journal {
    pub fn new(plan: OperationPlan) -> Self {
        let operation_id = *plan.operation_id();
        let steps = plan
            .steps()
            .iter()
            .map(|step| JournalStep {
                id: step.id().clone(),
                status: StepStatus::Pending,
            })
            .collect();
        Self {
            schema_version: 1,
            revision: 0,
            operation_id,
            plan,
            status: OperationStatus::Pending,
            steps,
        }
    }
    pub fn schema_version(&self) -> u8 {
        self.schema_version
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }
    pub fn plan(&self) -> &OperationPlan {
        &self.plan
    }
    pub fn status(&self) -> OperationStatus {
        self.status
    }
    pub fn steps(&self) -> &[JournalStep] {
        &self.steps
    }
    pub fn started_step(&self) -> Option<&JournalStep> {
        self.steps
            .iter()
            .find(|step| step.status == StepStatus::Started)
    }
    pub fn has_applied_steps(&self) -> bool {
        self.steps
            .iter()
            .any(|step| step.status == StepStatus::Applied)
    }
    pub fn is_unresolved(&self) -> bool {
        matches!(
            self.status,
            OperationStatus::Pending | OperationStatus::Running | OperationStatus::NeedsAttention
        ) || (self.status == OperationStatus::Failed && self.has_applied_steps())
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 || self.operation_id != *self.plan.operation_id() {
            return Err("journal schema or identity mismatch".into());
        }
        self.plan.validate_persisted()?;
        if self.steps.len() != self.plan.steps().len()
            || self
                .steps
                .iter()
                .zip(self.plan.steps())
                .any(|(a, b)| a.id != *b.id())
        {
            return Err("journal steps do not match plan".into());
        }
        if self
            .steps
            .iter()
            .map(|s| &s.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != self.steps.len()
        {
            return Err("journal step ids are not unique".into());
        }
        let mut started = 0;
        let mut seen_pending = false;
        let mut seen_attention = false;
        for step in &self.steps {
            match step.status {
                StepStatus::Applied if seen_pending || seen_attention || started > 0 => {
                    return Err("applied step follows incomplete step".into());
                }
                StepStatus::Pending => seen_pending = true,
                StepStatus::Started => {
                    started += 1;
                    if seen_pending || seen_attention {
                        return Err("started step is out of order".into());
                    }
                }
                StepStatus::NeedsAttention => {
                    if seen_attention {
                        return Err("multiple attention steps".into());
                    }
                    seen_attention = true;
                    if seen_pending || started > 0 {
                        return Err("attention step is out of order".into());
                    }
                }
                StepStatus::Applied => {}
            }
        }
        if started > 1 {
            return Err("multiple started steps".into());
        }
        let expected = if seen_attention {
            OperationStatus::NeedsAttention
        } else if started == 1 {
            OperationStatus::Running
        } else if self.steps.iter().all(|s| s.status == StepStatus::Applied) {
            OperationStatus::Applied
        } else {
            OperationStatus::Pending
        };
        if self.status != expected
            && !(self.status == OperationStatus::Failed && expected == OperationStatus::Pending)
        {
            return Err("journal status does not match steps".into());
        }
        Ok(())
    }
    pub fn fail_operation(&mut self) -> Result<(), String> {
        let mut next = self.clone();
        next.validate()?;
        if next.status != OperationStatus::Pending
            || next.steps.iter().any(|step| {
                matches!(
                    step.status,
                    StepStatus::Started | StepStatus::NeedsAttention
                )
            })
        {
            return Err("only a pending operation without an active step may fail".into());
        }
        next.set_revision()?;
        next.status = OperationStatus::Failed;
        next.validate()?;
        *self = next;
        Ok(())
    }
    pub fn validate_successor(&self, next: &Journal) -> Result<(), String> {
        self.validate()?;
        next.validate()?;
        if self.schema_version != next.schema_version
            || self.operation_id != next.operation_id
            || self.plan != next.plan
            || next.revision
                != self
                    .revision
                    .checked_add(1)
                    .ok_or("journal revision overflow")?
        {
            return Err("journal is not a direct legal successor".into());
        }
        let mut candidates = Vec::new();
        match self.status {
            OperationStatus::Pending => {
                if let Some(step) = self
                    .steps
                    .iter()
                    .find(|step| step.status == StepStatus::Pending)
                {
                    let mut candidate = self.clone();
                    candidate.start_step(&step.id)?;
                    candidates.push(candidate);
                }
                let mut candidate = self.clone();
                if candidate.fail_operation().is_ok() {
                    candidates.push(candidate);
                }
            }
            OperationStatus::Running => {
                if let Some(step) = self
                    .steps
                    .iter()
                    .find(|step| step.status == StepStatus::Started)
                {
                    for outcome in [
                        Reconciliation::Applied,
                        Reconciliation::NeedsAttention,
                        Reconciliation::Pending,
                    ] {
                        let mut candidate = self.clone();
                        if outcome == Reconciliation::Pending {
                            if candidate.reconcile_step(&step.id, outcome).is_ok() {
                                candidates.push(candidate);
                            }
                        } else if candidate.reconcile_step(&step.id, outcome).is_ok() {
                            candidates.push(candidate);
                        }
                    }
                }
            }
            OperationStatus::NeedsAttention
            | OperationStatus::Applied
            | OperationStatus::Failed => {}
        }
        if candidates.iter().any(|candidate| candidate == next) {
            Ok(())
        } else {
            Err("journal is not a direct legal successor".into())
        }
    }
    pub fn start_step(&mut self, id: &StepId) -> Result<(), String> {
        let mut next = self.clone();
        next.validate()?;
        if next.status != OperationStatus::Pending {
            return Err("only pending operations may start a step".into());
        }
        let index = next.next_index(id)?;
        next.set_revision()?;
        next.steps[index].status = StepStatus::Started;
        next.status = OperationStatus::Running;
        next.validate()?;
        *self = next;
        Ok(())
    }
    pub fn apply_step(&mut self, id: &StepId) -> Result<(), String> {
        let mut next = self.clone();
        next.validate()?;
        if next.status != OperationStatus::Running {
            return Err("only running operations may apply a step".into());
        }
        let index = next.next_index(id)?;
        if next.steps[index].status != StepStatus::Started {
            return Err("step can only apply after started".into());
        }
        next.set_revision()?;
        next.steps[index].status = StepStatus::Applied;
        if next.steps.iter().all(|s| s.status == StepStatus::Applied) {
            next.status = OperationStatus::Applied;
        } else {
            next.status = OperationStatus::Pending;
        }
        next.validate()?;
        *self = next;
        Ok(())
    }
    pub fn reconcile_step(&mut self, id: &StepId, outcome: Reconciliation) -> Result<(), String> {
        let mut next = self.clone();
        next.validate()?;
        if next.status != OperationStatus::Running {
            return Err("terminal or pending operation cannot reconcile".into());
        }
        let index = next.next_index(id)?;
        if next.steps[index].status != StepStatus::Started {
            return Err("only started steps may reconcile".into());
        }
        next.set_revision()?;
        match outcome {
            Reconciliation::Applied => {
                next.steps[index].status = StepStatus::Applied;
                next.status = if next.steps.iter().all(|s| s.status == StepStatus::Applied) {
                    OperationStatus::Applied
                } else {
                    OperationStatus::Pending
                };
            }
            Reconciliation::Pending | Reconciliation::NeedsAttention => {
                next.steps[index].status = StepStatus::NeedsAttention;
                next.status = OperationStatus::NeedsAttention;
            }
        }
        next.validate()?;
        *self = next;
        Ok(())
    }
    fn next_index(&self, id: &StepId) -> Result<usize, String> {
        let index = self
            .steps
            .iter()
            .position(|s| &s.id == id)
            .ok_or_else(|| "unknown step".to_owned())?;
        if self.steps[..index]
            .iter()
            .any(|s| s.status != StepStatus::Applied)
            || self.steps[index].status == StepStatus::Applied
        {
            return Err("step is out of order".into());
        }
        Ok(index)
    }
    fn set_revision(&mut self) -> Result<(), String> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| String::from("journal revision overflow"))?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct JournalWire {
    schema_version: u8,
    revision: u64,
    operation_id: OperationId,
    plan: OperationPlan,
    status: OperationStatus,
    steps: Vec<JournalStepWire>,
}
#[derive(Serialize, Deserialize)]
struct JournalStepWire {
    id: StepId,
    status: StepStatus,
}
impl Serialize for Journal {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        JournalWire {
            schema_version: self.schema_version,
            revision: self.revision,
            operation_id: self.operation_id,
            plan: self.plan.clone(),
            status: self.status,
            steps: self
                .steps
                .iter()
                .map(|s| JournalStepWire {
                    id: s.id.clone(),
                    status: s.status,
                })
                .collect(),
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Journal {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = JournalWire::deserialize(deserializer)?;
        let value = Self {
            schema_version: wire.schema_version,
            revision: wire.revision,
            operation_id: wire.operation_id,
            plan: wire.plan,
            status: wire.status,
            steps: wire
                .steps
                .into_iter()
                .map(|s| JournalStep {
                    id: s.id,
                    status: s.status,
                })
                .collect(),
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconciliation {
    Pending,
    Applied,
    NeedsAttention,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    #[test]
    fn status_strings_are_stable() {
        assert_eq!(OperationStatus::NeedsAttention.as_str(), "needs_attention");
        assert_eq!(StepStatus::Started.as_str(), "started");
    }

    fn journal() -> Journal {
        Journal::new(crate::lifecycle::test_plan(2))
    }

    #[test]
    fn initial_validity_and_exact_revision_sequence() {
        let mut value = journal();
        assert_eq!(value.revision(), 0);
        assert_eq!(value.status(), OperationStatus::Pending);
        value.validate().unwrap();
        let first = value.steps()[0].id().clone();
        let second = value.steps()[1].id().clone();
        value.start_step(&first).unwrap();
        assert_eq!(value.revision(), 1);
        assert_eq!(value.status(), OperationStatus::Running);
        value.apply_step(&first).unwrap();
        assert_eq!(value.revision(), 2);
        assert_eq!(value.status(), OperationStatus::Pending);
        value.start_step(&second).unwrap();
        assert_eq!(value.revision(), 3);
        value.apply_step(&second).unwrap();
        assert_eq!(value.revision(), 4);
        assert_eq!(value.status(), OperationStatus::Applied);
        value.validate().unwrap();
    }

    #[test]
    fn ordering_unknown_and_started_matrix_is_rejected() {
        let mut value = journal();
        let first = value.steps()[0].id().clone();
        let second = value.steps()[1].id().clone();
        assert!(value.apply_step(&first).is_err());
        assert!(value.start_step(&second).is_err());
        assert!(value.start_step(&StepId::new("missing").unwrap()).is_err());
        value.start_step(&first).unwrap();
        assert!(value.start_step(&second).is_err());
        assert!(value.start_step(&first).is_err());
    }

    #[test]
    fn reconciliation_completion_attention_and_terminal_freeze() {
        let mut value = journal();
        let first = value.steps()[0].id().clone();
        value.start_step(&first).unwrap();
        value
            .reconcile_step(&first, Reconciliation::Pending)
            .unwrap();
        assert_eq!(value.status(), OperationStatus::NeedsAttention);
        assert_eq!(value.steps()[0].status(), StepStatus::NeedsAttention);
        let frozen = value.clone();
        assert!(
            value
                .reconcile_step(&first, Reconciliation::Applied)
                .is_err()
        );
        assert_eq!(value, frozen);
        let mut applied = journal();
        let first = applied.steps()[0].id().clone();
        let second = applied.steps()[1].id().clone();
        applied.start_step(&first).unwrap();
        applied
            .reconcile_step(&first, Reconciliation::Applied)
            .unwrap();
        applied.start_step(&second).unwrap();
        applied
            .reconcile_step(&second, Reconciliation::Applied)
            .unwrap();
        assert_eq!(applied.status(), OperationStatus::Applied);
        assert!(applied.apply_step(&second).is_err());
    }

    #[test]
    fn applied_prefix_can_fail_and_failed_state_is_frozen() {
        let mut value = journal();
        let first = value.steps()[0].id().clone();
        value.start_step(&first).unwrap();
        value.apply_step(&first).unwrap();
        assert_eq!(value.revision(), 2);
        value.fail_operation().unwrap();
        assert_eq!(value.status(), OperationStatus::Failed);
        assert_eq!(value.revision(), 3);
        let frozen = value.clone();
        assert!(value.fail_operation().is_err());
        assert_eq!(value, frozen);
    }

    #[test]
    fn failed_is_narrow_and_terminal() {
        let mut value = journal();
        value.fail_operation().unwrap();
        assert_eq!(value.status(), OperationStatus::Failed);
        let frozen = value.clone();
        assert!(value.fail_operation().is_err());
        assert_eq!(value, frozen);
    }

    #[test]
    fn validate_successor_accepts_each_legal_edge_and_rejects_jumps() {
        let initial = journal();
        let first = initial.steps()[0].id().clone();
        let second = initial.steps()[1].id().clone();
        let mut started = initial.clone();
        started.start_step(&first).unwrap();
        assert!(initial.validate_successor(&started).is_ok());
        let mut failed = initial.clone();
        failed.fail_operation().unwrap();
        assert!(initial.validate_successor(&failed).is_ok());
        let mut applied_prefix = started.clone();
        applied_prefix.apply_step(&first).unwrap();
        assert!(started.validate_successor(&applied_prefix).is_ok());
        let mut final_started = applied_prefix.clone();
        final_started.start_step(&second).unwrap();
        let mut final_applied = final_started.clone();
        final_applied.apply_step(&second).unwrap();
        assert!(final_started.validate_successor(&final_applied).is_ok());
        let mut final_reconciled = final_started.clone();
        final_reconciled
            .reconcile_step(&second, Reconciliation::Applied)
            .unwrap();
        assert!(final_started.validate_successor(&final_reconciled).is_ok());
        let mut attention = started.clone();
        attention
            .reconcile_step(&first, Reconciliation::Pending)
            .unwrap();
        assert!(started.validate_successor(&attention).is_ok());
        let mut forged = initial.clone();
        forged
            .steps
            .iter_mut()
            .for_each(|step| step.status = StepStatus::Applied);
        forged.status = OperationStatus::Applied;
        forged.revision = 1;
        assert!(initial.validate_successor(&forged).is_err());
        let mut forged_attention = initial.clone();
        forged_attention.steps[0].status = StepStatus::NeedsAttention;
        forged_attention.status = OperationStatus::NeedsAttention;
        forged_attention.revision = 1;
        assert!(initial.validate_successor(&forged_attention).is_err());
        let mut no_op = initial.clone();
        no_op.revision = 1;
        assert!(initial.validate_successor(&no_op).is_err());
        let mut terminal = failed.clone();
        terminal.revision += 1;
        assert!(failed.validate_successor(&terminal).is_err());
    }

    #[test]
    fn overflow_is_transactional() {
        let mut value = journal();
        value.revision = u64::MAX;
        let original = value.clone();
        let id = value.steps()[0].id().clone();
        assert!(value.start_step(&id).is_err());
        assert_eq!(value, original);
    }

    #[test]
    fn serde_rejects_malformed_state_matrix() {
        let value = journal();
        let mut wire = serde_json::to_value(&value).unwrap();
        for (field, bad) in [
            ("schema_version", Value::from(2)),
            (
                "operation_id",
                Value::from("00000000-0000-0000-0000-000000000000"),
            ),
        ] {
            let mut candidate = wire.clone();
            candidate[field] = bad;
            assert!(serde_json::from_value::<Journal>(candidate).is_err());
        }
        let steps = wire["steps"].as_array_mut().unwrap();
        let id = steps[0]["id"].clone();
        steps[1]["id"] = id;
        assert!(serde_json::from_value::<Journal>(wire.clone()).is_err());
        wire["steps"] = Value::Array(vec![]);
        assert!(serde_json::from_value::<Journal>(wire.clone()).is_err());
        let mut reordered = serde_json::to_value(&value).unwrap();
        let first = reordered["steps"][0]["id"].clone();
        let second = reordered["steps"][1]["id"].clone();
        reordered["steps"][0]["id"] = second;
        reordered["steps"][1]["id"] = first;
        assert!(serde_json::from_value::<Journal>(reordered).is_err());
        let mut plan_identity = serde_json::to_value(&value).unwrap();
        plan_identity["plan"]["operation_id"] = Value::from("00000000-0000-0000-0000-000000000000");
        assert!(serde_json::from_value::<Journal>(plan_identity).is_err());
        let mut multiple = serde_json::to_value(&value).unwrap();
        multiple["steps"][0]["status"] = Value::from("started");
        multiple["steps"][1]["status"] = Value::from("started");
        multiple["status"] = Value::from("running");
        assert!(serde_json::from_value::<Journal>(multiple).is_err());
        let mut impossible = serde_json::to_value(&value).unwrap();
        impossible["status"] = Value::from("applied");
        assert!(serde_json::from_value::<Journal>(impossible).is_err());
        let mut attention = serde_json::to_value(&value).unwrap();
        attention["steps"][0]["status"] = Value::from("needs_attention");
        attention["steps"][1]["status"] = Value::from("needs_attention");
        attention["status"] = Value::from("needs_attention");
        assert!(serde_json::from_value::<Journal>(attention).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn full_journal_roundtrip_preserves_non_utf8_paths() {
        let value = journal();
        let mut wire = serde_json::to_value(&value).unwrap();
        fn replace(value: &mut Value) {
            match value {
                Value::String(text) if text == "/r" => {
                    *value = serde_json::json!({"kind": "bytes", "bytes": [47, 114, 255]});
                }
                Value::Array(values) => values.iter_mut().for_each(replace),
                Value::Object(values) => values.values_mut().for_each(replace),
                _ => {}
            }
        }
        replace(&mut wire);
        let restored: Journal = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), wire);
    }
}
