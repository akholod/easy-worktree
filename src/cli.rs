use crate::{
    application::{CreatePlanRequest, CreateSourceRequest, RemovePlanRequest},
    lifecycle::{BranchName, ConsentId, CreateSource, RefName, RemoteBranch, RemoteName},
};
use clap::{ArgGroup, Args, Parser, Subcommand};
use std::collections::BTreeSet;

#[derive(Debug, Parser)]
#[command(name = "ewtm", version, about = "Easy Worktrees Manager")]
pub struct Command {
    #[command(subcommand)]
    pub action: Option<Action>,
}

#[derive(Debug, Subcommand)]
pub enum Action {
    Tui,
    Create(CreateArgs),
    Remove(RemoveArgs),
    Apply(ApplyArgs),
    List {
        #[arg(long)]
        json: bool,
        #[arg(long, value_name = "PATH")]
        path: Option<std::path::PathBuf>,
    },
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    Doctor {
        #[arg(long)]
        json: bool,
        #[arg(long, value_name = "PATH")]
        path: Option<std::path::PathBuf>,
    },
    Recover {
        #[command(subcommand)]
        action: RecoverAction,
    },
}

#[derive(Debug, Args)]
pub struct ApplyArgs {
    pub plan: std::path::PathBuf,
    #[arg(long = "confirm-plan")]
    pub confirm_plan: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum RecoverAction {
    List {
        #[arg(long)]
        json: bool,
        #[arg(long, value_name = "PATH")]
        repo: Option<std::path::PathBuf>,
    },
    Show {
        operation_id: String,
        #[arg(long)]
        json: bool,
        #[arg(long, value_name = "PATH")]
        repo: Option<std::path::PathBuf>,
    },
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("source").required(true)))]
pub struct CreateArgs {
    #[arg(long, group = "source")]
    pub new: Option<String>,
    #[arg(long, group = "source")]
    pub existing_local: Option<String>,
    #[arg(long, group = "source")]
    pub remote: Option<String>,
    #[arg(long)]
    pub local_branch: Option<String>,
    #[arg(long)]
    pub base: Option<String>,
    #[arg(long)]
    pub path: Option<std::path::PathBuf>,
    #[arg(long)]
    pub repo: Option<std::path::PathBuf>,
    #[arg(long = "task")]
    pub task: Vec<String>,
    #[arg(long = "skip-rule")]
    pub skip_rule: Vec<String>,
    #[arg(long = "accept-rule")]
    pub accept_rule: Vec<String>,
    #[arg(long)]
    pub plan: bool,
    #[arg(long)]
    pub json: bool,
}

impl CreateArgs {
    pub fn into_request(
        &self,
        repo: std::path::PathBuf,
        invocation_cwd: std::path::PathBuf,
    ) -> Result<CreatePlanRequest, String> {
        let source = match self.source()? {
            CreateSource::NewBranch { branch, base } => CreateSourceRequest::New {
                branch: branch.as_str().into(),
                base: base.map(|v| v.to_string()),
            },
            CreateSource::ExistingLocal { branch } => CreateSourceRequest::ExistingLocal {
                branch: branch.as_str().into(),
            },
            CreateSource::RemoteTracking {
                remote,
                remote_branch,
                local_branch,
            } => CreateSourceRequest::RemoteTracking {
                remote: remote.to_string(),
                remote_branch: remote_branch.to_string(),
                local_branch: local_branch.to_string(),
            },
        };
        let mut grants = BTreeSet::new();
        for rule in &self.accept_rule {
            grants.insert(ConsentId::new(format!("file-rule:{rule}"))?);
        }
        for task in &self.task {
            grants.insert(ConsentId::new(format!("task:{task}"))?);
        }
        Ok(CreatePlanRequest {
            repo,
            invocation_cwd,
            source,
            custom_path: self.path.clone(),
            selected_tasks: self.task.iter().cloned().collect(),
            skipped_rules: self.skip_rule.iter().cloned().collect(),
            granted_consents: grants,
        })
    }
    pub fn validate(&self) -> Result<(), String> {
        self.source().map(|_| ())
    }
    pub fn source(&self) -> Result<CreateSource, String> {
        match (&self.new, &self.existing_local, &self.remote, &self.local_branch) {
            (Some(branch), None, None, None) => Ok(CreateSource::NewBranch { branch: BranchName::new(branch.clone())?, base: self.base.as_deref().map(|v| RefName::new(v.to_owned())).transpose()? }),
            (None, Some(branch), None, None) if self.base.is_none() => Ok(CreateSource::ExistingLocal { branch: BranchName::new(branch.clone())? }),
            (None, None, Some(remote_branch), Some(local_branch)) if self.base.is_none() => {
                let (remote, branch) = remote_branch.split_once('/').ok_or("--remote requires REMOTE/BRANCH")?;
                Ok(CreateSource::RemoteTracking { remote: RemoteName::new(remote.to_owned())?, remote_branch: BranchName::new(branch.to_owned())?, local_branch: BranchName::new(local_branch.clone())? })
            }
            _ => Err("source options are invalid: --local-branch is only valid with --remote, and --base only with --new".into()),
        }
    }
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    pub target: std::path::PathBuf,
    #[arg(long)]
    pub repo: Option<std::path::PathBuf>,
    #[arg(long = "allow-dirty-removal")]
    pub allow_dirty_removal: bool,
    #[arg(long = "delete-local-branch")]
    pub delete_local_branch: bool,
    #[arg(long = "force-delete-local-branch")]
    pub force_delete_local_branch: bool,
    #[arg(long = "delete-remote-branch")]
    pub delete_remote_branch: Option<String>,
    #[arg(long)]
    pub plan: bool,
    #[arg(long)]
    pub json: bool,
}

impl RemoveArgs {
    pub fn into_request(
        &self,
        repo: std::path::PathBuf,
        invocation_cwd: std::path::PathBuf,
    ) -> Result<RemovePlanRequest, String> {
        self.validate()?;
        let remote = self.remote_target()?;
        let mut granted = BTreeSet::new();
        if self.allow_dirty_removal {
            granted.insert(ConsentId::new("remove:dirty")?);
        }
        if self.delete_local_branch {
            granted.insert(ConsentId::new("remove:local-branch")?);
        }
        if self.force_delete_local_branch {
            granted.insert(ConsentId::new("remove:force-local-branch")?);
        }
        if let Some(target) = &remote {
            granted.insert(ConsentId::new(format!(
                "remove:remote:{}/{}",
                target.remote, target.branch
            ))?);
        }
        Ok(RemovePlanRequest {
            repo,
            invocation_cwd,
            target: self.target.clone(),
            allow_dirty_removal: self.allow_dirty_removal,
            delete_local_branch: self.delete_local_branch,
            force_delete_local_branch: self.force_delete_local_branch,
            delete_remote_branch: remote,
            granted_consents: granted,
        })
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.force_delete_local_branch && !self.delete_local_branch {
            return Err("force-delete-local-branch requires delete-local-branch".into());
        }
        self.remote_target().map(|_| ())
    }
    fn remote_target(&self) -> Result<Option<RemoteBranch>, String> {
        let remote = self
            .delete_remote_branch
            .as_deref()
            .map(|value| -> Result<RemoteBranch, String> {
                let (remote, branch) = value
                    .split_once('/')
                    .ok_or("--delete-remote-branch requires REMOTE/BRANCH")?;
                Ok(RemoteBranch {
                    remote: RemoteName::new(remote.to_owned())?,
                    branch: BranchName::new(branch.to_owned())?,
                })
            })
            .transpose()?;
        Ok(remote)
    }
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    Show {
        #[arg(long)]
        json: bool,
        #[arg(long, value_name = "PATH")]
        path: Option<std::path::PathBuf>,
        #[command(flatten)]
        overrides: ConfigOverrideArgs,
    },
    Validate {
        #[arg(long)]
        json: bool,
        #[arg(long, value_name = "PATH")]
        path: Option<std::path::PathBuf>,
        #[command(flatten)]
        overrides: ConfigOverrideArgs,
    },
    Import {
        #[arg(long, value_name = "PATH")]
        file: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
        #[arg(long, value_name = "PATH")]
        path: Option<std::path::PathBuf>,
    },
    Edit {
        #[arg(long, value_parser = ["user", "project", "local"])]
        scope: String,
        #[arg(long, value_name = "PATH")]
        path: Option<std::path::PathBuf>,
    },
}

#[derive(Debug, Clone, Args, Default)]
pub struct ConfigOverrideArgs {
    #[arg(long)]
    pub slug_max_bytes: Option<usize>,
    #[arg(long)]
    pub worktree_root: Option<String>,
    #[arg(long)]
    pub directory_prefix: Option<String>,
}

impl From<ConfigOverrideArgs> for crate::config::ConfigOverrides {
    fn from(value: ConfigOverrideArgs) -> Self {
        Self {
            slug_max_bytes: value.slug_max_bytes,
            worktree_root: value.worktree_root,
            directory_prefix: value.directory_prefix,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, Command};
    use crate::application::RemovePlanRequest;
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_definition_is_valid() {
        Command::command().debug_assert();
    }

    #[test]
    fn apply_syntax_requires_path_and_confirmation() {
        let command = Command::try_parse_from([
            "ewtm",
            "apply",
            "plan.json",
            "--confirm-plan",
            &"a".repeat(64),
            "--json",
        ])
        .unwrap();
        let Action::Apply(args) = command.action.unwrap() else {
            panic!("apply");
        };
        assert_eq!(args.plan, std::path::PathBuf::from("plan.json"));
        assert!(args.json);
        assert!(Command::try_parse_from(["ewtm", "apply", "plan.json"]).is_err());
        assert!(
            Command::try_parse_from([
                "ewtm",
                "apply",
                "plan.json",
                "--json",
                "--confirm-plan",
                &"a".repeat(64),
            ])
            .is_ok()
        );
        assert!(
            Command::try_parse_from(["ewtm", "apply", "-", "--confirm-plan", &"a".repeat(64),])
                .is_ok()
        );
    }

    #[test]
    fn source_conversion_distinguishes_all_modes() {
        let command =
            Command::try_parse_from(["ewtm", "create", "--new", "feature", "--plan"]).unwrap();
        let Action::Create(args) = command.action.unwrap() else {
            panic!("create")
        };
        assert!(matches!(
            args.source().unwrap(),
            crate::lifecycle::CreateSource::NewBranch { .. }
        ));
        let command = Command::try_parse_from([
            "ewtm",
            "create",
            "--remote",
            "origin/topic",
            "--local-branch",
            "topic",
            "--plan",
        ])
        .unwrap();
        let Action::Create(args) = command.action.unwrap() else {
            panic!("create")
        };
        assert!(matches!(
            args.source().unwrap(),
            crate::lifecycle::CreateSource::RemoteTracking { .. }
        ));
    }

    #[test]
    fn local_branch_is_only_required_for_remote() {
        let command =
            Command::try_parse_from(["ewtm", "create", "--remote", "origin/topic", "--plan"])
                .unwrap();
        let Action::Create(args) = command.action.unwrap() else {
            panic!("create")
        };
        assert!(args.source().is_err());
        let command = Command::try_parse_from([
            "ewtm",
            "create",
            "--new",
            "topic",
            "--local-branch",
            "other",
            "--plan",
        ])
        .unwrap();
        let Action::Create(args) = command.action.unwrap() else {
            panic!("create")
        };
        assert!(args.source().is_err());
    }

    #[test]
    fn force_alias_is_not_in_cli() {
        assert!(Command::try_parse_from(["ewtm", "remove", "/tmp/w", "--force"]).is_err());
        let command =
            Command::try_parse_from(["ewtm", "remove", "/tmp/w", "--force-delete-local-branch"])
                .unwrap();
        let Action::Remove(args) = command.action.unwrap() else {
            panic!("remove")
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn remove_force_requires_delete_local_and_remote_is_typed() {
        let command = Command::try_parse_from([
            "ewtm",
            "remove",
            "/tmp/w",
            "--delete-remote-branch",
            "origin/topic",
            "--plan",
        ])
        .unwrap();
        let Action::Remove(args) = command.action.unwrap() else {
            panic!("remove")
        };
        assert!(args.validate().is_ok());
        let command = Command::try_parse_from([
            "ewtm",
            "remove",
            "/tmp/w",
            "--delete-remote-branch",
            "bad",
            "--plan",
        ])
        .unwrap();
        let Action::Remove(args) = command.action.unwrap() else {
            panic!("remove")
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn cli_grants_are_exact_and_remove_target_is_a_path() {
        let command = Command::try_parse_from([
            "ewtm",
            "remove",
            "relative/path",
            "--allow-dirty-removal",
            "--delete-local-branch",
            "--force-delete-local-branch",
            "--delete-remote-branch",
            "origin/topic",
            "--plan",
        ])
        .unwrap();
        let Action::Remove(args) = command.action.unwrap() else {
            panic!("remove")
        };
        assert_eq!(args.target, std::path::PathBuf::from("relative/path"));
        let RemovePlanRequest {
            granted_consents, ..
        } = args.into_request("/repo".into(), "/cwd".into()).unwrap();
        assert!(
            granted_consents.contains(&crate::lifecycle::ConsentId::new("remove:dirty").unwrap())
        );
        assert!(
            granted_consents
                .contains(&crate::lifecycle::ConsentId::new("remove:local-branch").unwrap())
        );
        assert!(
            granted_consents
                .contains(&crate::lifecycle::ConsentId::new("remove:force-local-branch").unwrap())
        );
        assert!(
            granted_consents
                .contains(&crate::lifecycle::ConsentId::new("remove:remote:origin/topic").unwrap())
        );
    }
}
