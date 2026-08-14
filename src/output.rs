use std::fmt::Write;

use crate::{
    application::{AppOutcome, DoctorReport, ResponseData},
    config::ProvenanceValue,
    domain::{CheckoutStatus, ListData, WorktreeClass},
};

#[derive(serde::Serialize)]
pub struct Envelope<T> {
    pub schema_version: u8,
    pub command: &'static str,
    pub ok: bool,
    pub data: Option<T>,
    pub warnings: Vec<crate::application::DiagnosticDto>,
    pub error: Option<crate::application::DiagnosticDto>,
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum JsonData {
    List(ListData),
    Config(crate::config::LoadedConfig),
    Validated {
        valid: bool,
    },
    Imported(crate::worktreerc::ImportResult),
    Edited {
        path: crate::domain::PathDto,
    },
    Doctor {
        checks: Vec<JsonCheck>,
    },
    OperationPlan(crate::lifecycle::OperationPlan),
    JournalList(Vec<crate::journal::Journal>),
    Journal(crate::journal::Journal),
    Execution {
        operation_id: String,
        outcome: &'static str,
    },
}

#[derive(serde::Serialize)]
struct JsonCheck {
    name: &'static str,
    ok: bool,
}

pub fn render_json(outcome: &AppOutcome) -> Result<String, serde_json::Error> {
    let data = outcome.result.as_ref().ok().map(json_data);
    let error = outcome
        .result
        .as_ref()
        .err()
        .map(|failure| failure.diagnostic.clone());
    serde_json::to_string(&Envelope {
        schema_version: 1,
        command: outcome.command,
        ok: outcome.is_success(),
        data,
        warnings: outcome.warnings.clone(),
        error,
    })
}

pub fn render_text(outcome: &AppOutcome) -> Result<String, String> {
    match &outcome.result {
        Ok(ResponseData::List(data)) => render_list(data),
        Ok(ResponseData::ConfigShow(loaded)) => render_config(loaded),
        Ok(ResponseData::ConfigValidate(result)) => Ok(if result.valid {
            "configuration is valid\n".into()
        } else {
            "configuration is invalid\n".into()
        }),
        Ok(ResponseData::ConfigImport(result)) => {
            let mut text =
                toml::to_string_pretty(&result.config).map_err(|error| error.to_string())?;
            writeln!(text, "# source: {}", result.source.display())
                .map_err(|error| error.to_string())?;
            for diagnostic in &result.diagnostics {
                writeln!(
                    text,
                    "# diagnostic {}:{}:{}: {}",
                    result.source.display(),
                    diagnostic.line,
                    diagnostic.column,
                    diagnostic.message
                )
                .map_err(|error| error.to_string())?;
            }
            Ok(text)
        }
        Ok(ResponseData::ConfigEdit(result)) => Ok(format!("editing {}\n", result.path.display())),
        Ok(ResponseData::Doctor(report)) => render_doctor(report),
        Ok(ResponseData::OperationPlan(plan)) => serde_json::to_string_pretty(plan)
            .map_err(|e| e.to_string())
            .map(|s| format!("{s}\n")),
        Ok(ResponseData::JournalList(items)) => {
            let mut text = String::new();
            for item in items {
                writeln!(
                    text,
                    "{}\t{}\trevision {}",
                    item.operation_id(),
                    item.status().as_str(),
                    item.revision()
                )
                .map_err(|e| e.to_string())?;
            }
            Ok(text)
        }
        Ok(ResponseData::Journal(item)) => Ok(format!(
            "operation {}\nstatus: {}\nrevision: {}\n",
            item.operation_id(),
            item.status().as_str(),
            item.revision()
        )),
        Ok(ResponseData::Execution(result)) => Ok(format!(
            "{}\t{}\n",
            result.operation_id,
            execution_name(result.outcome)
        )),
        Err(failure) => Err(format!(
            "{}: {}",
            failure.diagnostic.code, failure.diagnostic.message
        )),
    }
}

fn json_data(data: &ResponseData) -> JsonData {
    match data {
        ResponseData::List(value) => JsonData::List(value.clone()),
        ResponseData::ConfigShow(value) => JsonData::Config(value.clone()),
        ResponseData::ConfigValidate(value) => JsonData::Validated { valid: value.valid },
        ResponseData::ConfigImport(value) => JsonData::Imported(value.clone()),
        ResponseData::ConfigEdit(value) => JsonData::Edited {
            path: value.path.clone().into(),
        },
        ResponseData::Doctor(value) => JsonData::Doctor {
            checks: value
                .checks
                .iter()
                .map(|check| JsonCheck {
                    name: check.name,
                    ok: check.ok,
                })
                .collect(),
        },
        ResponseData::OperationPlan(value) => JsonData::OperationPlan(value.clone()),
        ResponseData::JournalList(value) => JsonData::JournalList(value.clone()),
        ResponseData::Journal(value) => JsonData::Journal(value.clone()),
        ResponseData::Execution(value) => JsonData::Execution {
            operation_id: value.operation_id.to_string(),
            outcome: execution_name(value.outcome),
        },
    }
}

fn execution_name(outcome: crate::application::ExecutionOutcomeKind) -> &'static str {
    match outcome {
        crate::application::ExecutionOutcomeKind::Applied => "applied",
        crate::application::ExecutionOutcomeKind::AlreadyApplied => "already_applied",
        crate::application::ExecutionOutcomeKind::PreflightRefused => "preflight_refused",
        crate::application::ExecutionOutcomeKind::Paused => "paused",
        crate::application::ExecutionOutcomeKind::NeedsAttention => "needs_attention",
        crate::application::ExecutionOutcomeKind::ExistingOperation => "existing_operation",
    }
}

fn render_list(data: &ListData) -> Result<String, String> {
    let mut output = String::new();
    for item in &data.worktrees {
        let class = match item.classification {
            WorktreeClass::Primary => "primary",
            WorktreeClass::Linked => "linked",
            WorktreeClass::Bare => "bare",
            WorktreeClass::Unknown => "unknown",
        };
        let status = match item.status {
            CheckoutStatus::Clean => "clean",
            CheckoutStatus::Dirty => "dirty",
            CheckoutStatus::Unknown => "unknown",
        };
        writeln!(
            output,
            "{class}\t{}\t{}\t{status}",
            item.path.to_string_lossy(),
            item.branch.as_deref().unwrap_or("(detached)")
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(output)
}

fn render_config(loaded: &crate::config::LoadedConfig) -> Result<String, String> {
    let mut output = toml::to_string_pretty(&loaded.config).map_err(|error| error.to_string())?;
    output.push_str("\n# provenance\n");
    for (key, source) in &loaded.provenance.scalars {
        writeln!(output, "# {key} = {}", provenance(source)).map_err(|error| error.to_string())?;
    }
    for (key, source) in &loaded.provenance.file_rules {
        writeln!(output, "# file_rules.{key} = {}", provenance(source))
            .map_err(|error| error.to_string())?;
    }
    for (key, source) in &loaded.provenance.tasks {
        writeln!(output, "# tasks.{key} = {}", provenance(source))
            .map_err(|error| error.to_string())?;
    }
    for (key, source) in &loaded.provenance.hooks {
        writeln!(output, "# hooks.{key} = {}", provenance(source))
            .map_err(|error| error.to_string())?;
    }
    Ok(output)
}

fn provenance(value: &ProvenanceValue) -> String {
    match value {
        ProvenanceValue::Defaults => "defaults".into(),
        ProvenanceValue::Cli => "cli".into(),
        ProvenanceValue::User { path } => format!("user:{}", path.display()),
        ProvenanceValue::Project { path } => format!("project:{}", path.display()),
        ProvenanceValue::Local { path } => format!("local:{}", path.display()),
    }
}

fn render_doctor(report: &DoctorReport) -> Result<String, String> {
    let mut output = String::new();
    for check in &report.checks {
        writeln!(
            output,
            "{}: {}",
            check.name,
            if check.ok { "ok" } else { "failed" }
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::render_text;
    use crate::{
        application::{AppOutcome, ResponseData},
        domain::{ListData, RepositorySummary},
    };

    #[test]
    fn list_text_is_tabular_not_json() {
        let outcome = AppOutcome::ok(
            "list",
            ResponseData::List(ListData {
                repository: RepositorySummary {
                    common_dir: ".git".into(),
                    bare: false,
                },
                worktrees: Vec::new(),
            }),
            Vec::new(),
        );
        let text = render_text(&outcome).unwrap();
        assert!(!text.starts_with('{'));
    }

    #[test]
    fn execution_output_uses_stable_names() {
        let id = crate::lifecycle::OperationId::new(uuid::Uuid::new_v4());
        let outcome = AppOutcome::ok(
            "apply",
            ResponseData::Execution(crate::application::ExecutionResponse {
                operation_id: id,
                outcome: crate::application::ExecutionOutcomeKind::AlreadyApplied,
            }),
            Vec::new(),
        );
        let json = super::render_json(&outcome).unwrap();
        assert!(json.contains("\"outcome\":\"already_applied\""));
        assert!(
            super::render_text(&outcome)
                .unwrap()
                .ends_with("already_applied\n")
        );
    }
}
