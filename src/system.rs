use crate::{
    application::{
        ConfigFilePort, ConfigLocationPort, CreatePlanRequest, CreatePlanningFacts, EditorPort,
        EnvironmentPort, LifecyclePlanningPort, ManifestPlanningPort, ManifestRuleSpec,
        PlanningError, ProcessPort, RemovePlanRequest,
    },
    config::{ConfigLocations, LayerContents, LayerSource},
    worktreerc,
};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

pub struct System;

impl ConfigLocationPort for System {
    fn locations(&self, repo: &Path) -> Result<ConfigLocations, String> {
        let (project, common) =
            crate::infrastructure::repository_roots(repo).map_err(|e| e.to_string())?;
        let home = resolve_home(env::var_os("HOME"))?;
        Ok(ConfigLocations {
            user: Some(home.join(".config/ewtm/config.toml")),
            project: project.join(".ewtm.toml"),
            local: common.join("ewtm/config.toml"),
        })
    }
}

pub fn resolve_home(value: Option<std::ffi::OsString>) -> Result<PathBuf, String> {
    let home =
        value.ok_or_else(|| "HOME is not set; refusing current-directory fallback".to_owned())?;
    let home = PathBuf::from(home);
    if home.is_absolute() {
        Ok(home)
    } else {
        Err("HOME must be absolute".into())
    }
}

impl ConfigFilePort for System {
    fn read_layers(&self, locations: &ConfigLocations) -> Result<Vec<LayerContents>, String> {
        let mut result = Vec::new();
        for (path, source) in [
            (locations.user.as_ref(), LayerSource::User),
            (Some(&locations.project), LayerSource::Project),
            (Some(&locations.local), LayerSource::Local),
        ] {
            let Some(path) = path else { continue };
            let contents = match fs::read_to_string(path) {
                Ok(value) => Some(value),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("{}: {e}", path.display())),
            };
            result.push(LayerContents {
                path: path.clone(),
                contents,
                source,
            });
        }
        Ok(result)
    }
    fn read_import(&self, path: &Path) -> Result<String, String> {
        fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

impl EnvironmentPort for System {
    fn editor(&self) -> Result<String, String> {
        env::var("VISUAL")
            .or_else(|_| env::var("EDITOR"))
            .map_err(|_| "VISUAL or EDITOR is not set".into())
    }
    fn git_available(&self) -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

impl EditorPort for System {
    fn prepare(&self, target: &Path) -> Result<(), String> {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if !target.exists() {
            fs::write(target, "schema = 1\n").map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    fn execute(&self, editor: &str, target: &Path) -> Result<(), String> {
        if editor.trim().is_empty() || editor.chars().any(char::is_whitespace) {
            return Err("editor must be one executable without whitespace".into());
        }
        let status = Command::new(editor)
            .arg(target)
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("editor exited with {status}"))
        }
    }
}

impl ProcessPort for System {
    fn import(
        &self,
        source: &str,
        path: &Path,
    ) -> Result<worktreerc::ImportResult, Vec<worktreerc::Diagnostic>> {
        worktreerc::import_source(source, path)
    }
}

impl LifecyclePlanningPort for System {
    fn create_facts(
        &self,
        request: &CreatePlanRequest,
        default_base: Option<&str>,
        remote: &str,
        worktree_root: Option<&str>,
        directory_prefix: Option<&str>,
    ) -> Result<crate::application::CreatePlanningFacts, PlanningError> {
        crate::infrastructure::GitCli.create_facts(
            request,
            default_base,
            remote,
            worktree_root,
            directory_prefix,
        )
    }
    fn remove_facts(
        &self,
        request: &RemovePlanRequest,
    ) -> Result<crate::application::RemovePlanningFacts, PlanningError> {
        crate::infrastructure::GitCli.remove_facts(request)
    }
}

impl ManifestPlanningPort for System {
    fn plan_manifests(
        &self,
        request: &CreatePlanRequest,
        facts: &CreatePlanningFacts,
        rules: Vec<ManifestRuleSpec>,
    ) -> Result<Vec<crate::planner::FileActionManifest>, PlanningError> {
        crate::infrastructure::GitCli.plan_manifests(request, facts, rules)
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_home;

    #[test]
    fn home_resolution_rejects_missing_and_relative_values() {
        assert!(resolve_home(None).is_err());
        assert!(resolve_home(Some("relative".into())).is_err());
    }

    #[test]
    fn home_resolution_accepts_absolute_value() {
        assert_eq!(
            resolve_home(Some("/tmp/home".into())).unwrap().to_str(),
            Some("/tmp/home")
        );
    }
}
