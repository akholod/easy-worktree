pub mod application;
pub mod cli;
pub mod config;
pub mod domain;
pub mod execution;
pub mod infrastructure;
pub mod journal;
pub mod journal_store;
pub mod lifecycle;
mod output;
pub mod planner;
mod system;
mod tui;
mod worktreerc;

use application::{Application, Request};
use clap::Parser;
use std::{path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let command = cli::Command::parse();
    let Some(action) = command.action else {
        return tui::run().map_or(ExitCode::from(1), |_| ExitCode::SUCCESS);
    };
    let invocation_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (request, json) = match action {
        cli::Action::Tui => return tui::run().map_or(ExitCode::from(1), |_| ExitCode::SUCCESS),
        cli::Action::Create(args) => {
            if !args.plan {
                return unavailable(
                    "create",
                    false,
                    args.json,
                    "execution_not_available",
                    "execution is not available in M2A2G; pass --plan".into(),
                );
            }
            let repo = args.repo.clone().unwrap_or_else(|| PathBuf::from("."));
            let request = match args.into_request(repo, invocation_cwd.clone()) {
                Ok(value) => value,
                Err(message) => {
                    return unavailable(
                        "create",
                        true,
                        args.json,
                        "invalid_create_options",
                        message,
                    );
                }
            };
            (Request::CreatePlan(request), args.json)
        }
        cli::Action::Remove(args) => {
            if !args.plan {
                return unavailable(
                    "remove",
                    false,
                    args.json,
                    "execution_not_available",
                    "execution is not available in M2A2G; pass --plan".into(),
                );
            }
            let repo = args.repo.clone().unwrap_or_else(|| PathBuf::from("."));
            let request = match args.into_request(repo, invocation_cwd.clone()) {
                Ok(value) => value,
                Err(message) => {
                    return unavailable(
                        "remove",
                        true,
                        args.json,
                        "invalid_remove_options",
                        message,
                    );
                }
            };
            (Request::RemovePlan(request), args.json)
        }
        cli::Action::List { json, path } => (
            Request::List {
                path: path.unwrap_or_else(|| PathBuf::from(".")),
            },
            json,
        ),
        cli::Action::Doctor { json, path } => (
            Request::Doctor {
                path: path.unwrap_or_else(|| PathBuf::from(".")),
            },
            json,
        ),
        cli::Action::Recover { action } => match action {
            cli::RecoverAction::List { json, repo } => (
                Request::RecoverList {
                    repo: repo.unwrap_or_else(|| PathBuf::from(".")),
                },
                json,
            ),
            cli::RecoverAction::Show {
                operation_id,
                json,
                repo,
            } => {
                let id = match operation_id.parse() {
                    Ok(id) => id,
                    Err(_) => {
                        return unavailable(
                            "recover_show",
                            true,
                            json,
                            "invalid_operation_id",
                            "invalid operation id".into(),
                        );
                    }
                };
                (
                    Request::RecoverShow {
                        repo: repo.unwrap_or_else(|| PathBuf::from(".")),
                        operation_id: id,
                    },
                    json,
                )
            }
        },
        cli::Action::Config { action } => match action {
            cli::ConfigAction::Show {
                json,
                path,
                overrides,
            } => (
                Request::ConfigShow {
                    path: path.unwrap_or_else(|| PathBuf::from(".")),
                    overrides: overrides.into(),
                },
                json,
            ),
            cli::ConfigAction::Validate {
                json,
                path,
                overrides,
            } => (
                Request::ConfigValidate {
                    path: path.unwrap_or_else(|| PathBuf::from(".")),
                    overrides: overrides.into(),
                },
                json,
            ),
            cli::ConfigAction::Import { json, file, path } => (
                Request::ConfigImport {
                    repo: path.unwrap_or_else(|| PathBuf::from(".")),
                    file,
                },
                json,
            ),
            cli::ConfigAction::Edit { scope, path } => (
                Request::ConfigEdit {
                    repo: path.unwrap_or_else(|| PathBuf::from(".")),
                    scope,
                },
                false,
            ),
        },
    };
    let repository = infrastructure::GitCli;
    let system = system::System;
    let outcome = Application {
        repository: &repository,
        system: &system,
    }
    .execute(request);
    if json {
        match output::render_json(&outcome) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("failed to serialize response: {error}");
                return ExitCode::from(1);
            }
        }
    } else {
        match output::render_text(&outcome) {
            Ok(text) => print!("{text}"),
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(1);
            }
        }
    }
    if outcome.is_success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn unavailable(
    command: &'static str,
    plan: bool,
    json: bool,
    requested_code: &'static str,
    requested_message: String,
) -> ExitCode {
    let (code, message) = if requested_code == "planning_backend_unavailable" && !plan {
        (
            "execution_not_available",
            "execution is not available in M2A2G; pass --plan".into(),
        )
    } else {
        (requested_code, requested_message)
    };
    let outcome = application::AppOutcome::fail(
        command,
        application::DiagnosticDto {
            code: code.into(),
            message,
            path: None,
            line: None,
            column: None,
        },
    );
    if json {
        if let Ok(text) = output::render_json(&outcome) {
            println!("{text}");
        }
    } else if let Ok(text) = output::render_text(&outcome) {
        eprintln!("{text}");
    }
    ExitCode::from(1)
}
