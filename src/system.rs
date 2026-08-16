use crate::{
    application::{
        ConfigFilePort, ConfigLocationPort, CreateFactsNaming, CreatePlanRequest,
        CreatePlanningFacts, EditorPort, EnvironmentPort, LifecyclePlanningPort,
        ManifestPlanningPort, ManifestRuleSpec, PlanFileError, PlanFilePort, PlanningError,
        ProcessPort, RemovePlanRequest,
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

impl PlanFilePort for System {
    fn read_plan(&self, path: &Path) -> Result<Vec<u8>, PlanFileError> {
        if path == Path::new("-") || path.as_os_str().is_empty() {
            return Err(PlanFileError::NotRegular);
        }
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            let flags = rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC;
            fs::OpenOptions::new()
                .read(true)
                .custom_flags(flags.bits() as i32)
                .open(path)
                .map_err(|error| match error.kind() {
                    std::io::ErrorKind::NotFound => PlanFileError::Io,
                    std::io::ErrorKind::IsADirectory | std::io::ErrorKind::InvalidInput => {
                        PlanFileError::NotRegular
                    }
                    _ if (cfg!(target_os = "linux") && error.raw_os_error() == Some(40))
                        || (cfg!(target_os = "macos") && error.raw_os_error() == Some(62)) =>
                    {
                        PlanFileError::NotRegular
                    }
                    _ => PlanFileError::Io,
                })?
        };
        #[cfg(unix)]
        {
            let metadata = file.metadata().map_err(|_| PlanFileError::Io)?;
            if !metadata.is_file() {
                return Err(PlanFileError::NotRegular);
            }
            read_held_plan(file)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(PlanFileError::Io)
        }
    }
}

#[cfg(unix)]
fn read_held_plan(file: fs::File) -> Result<Vec<u8>, PlanFileError> {
    use std::io::Read;

    let mut bytes = Vec::new();
    file.take(crate::plan_authority::MAX_PLAN_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| PlanFileError::Io)?;
    if bytes.len() > crate::plan_authority::MAX_PLAN_BYTES {
        return Err(PlanFileError::TooLarge);
    }
    Ok(bytes)
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
        naming: CreateFactsNaming,
    ) -> Result<crate::application::CreatePlanningFacts, PlanningError> {
        crate::infrastructure::GitCli.create_facts(
            request,
            default_base,
            remote,
            worktree_root,
            directory_prefix,
            naming,
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
    use super::{PlanFilePort, System, resolve_home};
    use std::{fs, path::Path, process::Command};
    use tempfile::tempdir;

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

    #[test]
    fn plan_reader_accepts_exact_limit_and_rejects_limit_plus_one() {
        let dir = tempdir().unwrap();
        let ordinary = dir.path().join("plan.json");
        let exact = vec![b'x'; crate::plan_authority::MAX_PLAN_BYTES];
        fs::write(&ordinary, &exact).unwrap();
        assert_eq!(System.read_plan(&ordinary).unwrap(), exact);
        fs::write(
            &ordinary,
            vec![b'x'; crate::plan_authority::MAX_PLAN_BYTES + 1],
        )
        .unwrap();
        assert_eq!(
            System.read_plan(&ordinary),
            Err(crate::application::PlanFileError::TooLarge)
        );
    }

    #[test]
    fn plan_reader_rejects_missing_empty_and_nonregular_paths() {
        let dir = tempdir().unwrap();
        let empty = dir.path().join("empty");
        fs::write(&empty, []).unwrap();
        assert_eq!(System.read_plan(&empty).unwrap(), Vec::<u8>::new());
        assert_eq!(
            System.read_plan(Path::new("-")),
            Err(crate::application::PlanFileError::NotRegular)
        );
        assert_eq!(
            System.read_plan(Path::new("")),
            Err(crate::application::PlanFileError::NotRegular)
        );
        assert_eq!(
            System.read_plan(&dir.path().join("missing")),
            Err(crate::application::PlanFileError::Io)
        );
        assert_eq!(
            System.read_plan(dir.path()),
            Err(crate::application::PlanFileError::NotRegular)
        );
        #[cfg(unix)]
        {
            let ordinary = dir.path().join("ordinary");
            fs::write(&ordinary, b"data").unwrap();
            std::os::unix::fs::symlink(&ordinary, dir.path().join("link")).unwrap();
            assert_eq!(
                System.read_plan(&dir.path().join("link")),
                Err(crate::application::PlanFileError::NotRegular)
            );

            let fifo = dir.path().join("fifo");
            assert!(
                Command::new("mkfifo")
                    .arg(&fifo)
                    .status()
                    .unwrap()
                    .success()
            );
            assert_eq!(
                System.read_plan(&fifo),
                Err(crate::application::PlanFileError::NotRegular)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn held_plan_reader_survives_path_replacement() {
        use std::os::unix::fs::OpenOptionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("plan");
        let replacement = dir.path().join("replacement");
        fs::write(&path, b"original").unwrap();
        fs::write(&replacement, b"replacement").unwrap();
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC).bits() as i32,
            )
            .open(&path)
            .unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&replacement, &path).unwrap();
        assert_eq!(super::read_held_plan(file).unwrap(), b"original");
    }
}
