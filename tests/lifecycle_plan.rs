use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

struct RepoFixture {
    _temp: TempDir,
    repo: PathBuf,
}

impl RepoFixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        git(temp.path(), ["init", "-b", "main", repo.to_str().unwrap()]);
        git(&repo, ["config", "user.name", "integration"]);
        git(
            &repo,
            ["config", "user.email", "integration@example.invalid"],
        );
        std::fs::write(repo.join("tracked"), b"initial").unwrap();
        git(&repo, ["add", "tracked"]);
        git(&repo, ["commit", "-m", "initial"]);
        Self { _temp: temp, repo }
    }

    fn command(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_ewtm"))
            .args(args)
            .env("HOME", self._temp.path().join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap()
    }

    fn snapshot(&self) -> (Vec<u8>, Vec<u8>) {
        (
            git_output(&self.repo, ["worktree", "list", "--porcelain", "-z"]),
            git_output(&self.repo, ["show-ref"]),
        )
    }
}

fn git<'a>(cwd: &Path, args: impl IntoIterator<Item = &'a str>) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "integration")
        .env("GIT_AUTHOR_EMAIL", "integration@example.invalid")
        .env("GIT_COMMITTER_NAME", "integration")
        .env("GIT_COMMITTER_EMAIL", "integration@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output<'a>(cwd: &Path, args: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn json(output: &std::process::Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn create_plan_json_is_successful_and_non_mutating() {
    let fixture = RepoFixture::new();
    let destination = fixture._temp.path().join("destination");
    let before = fixture.snapshot();
    let output = fixture.command(&[
        "create",
        "--new",
        "feature",
        "--base",
        "HEAD",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--path",
        destination.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let value = json(&output);
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "create");
    assert_eq!(value["data"]["kind"], "create");
    assert!(!destination.exists());
    assert_eq!(before, fixture.snapshot());
}

#[test]
fn create_without_plan_is_execution_refusal() {
    let fixture = RepoFixture::new();
    let before = fixture.snapshot();
    let output = fixture.command(&[
        "create",
        "--new",
        "feature",
        "--base",
        "HEAD",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let value = json(&output);
    assert_eq!(value["error"]["code"], "execution_not_available");
    assert_eq!(before, fixture.snapshot());
}

#[test]
fn existing_local_plan_uses_existing_branch_without_mutation() {
    let fixture = RepoFixture::new();
    git(&fixture.repo, ["branch", "existing"]);
    let before = fixture.snapshot();
    let output = fixture.command(&[
        "create",
        "--existing-local",
        "existing",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--path",
        fixture
            ._temp
            .path()
            .join("existing-destination")
            .to_str()
            .unwrap(),
        "--plan",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json(&output)["data"]["kind"], "create");
    assert_eq!(before, fixture.snapshot());
}

#[test]
fn destination_present_and_dangling_are_refused() {
    let fixture = RepoFixture::new();
    let present = fixture._temp.path().join("present");
    std::fs::write(&present, b"existing").unwrap();
    let output = fixture.command(&[
        "create",
        "--new",
        "present-branch",
        "--base",
        "HEAD",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--path",
        present.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["error"]["code"], "planning_refused");
    #[cfg(unix)]
    {
        let dangling = fixture._temp.path().join("dangling");
        std::os::unix::fs::symlink("missing", &dangling).unwrap();
        let output = fixture.command(&[
            "create",
            "--new",
            "dangling-branch",
            "--base",
            "HEAD",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--path",
            dangling.to_str().unwrap(),
            "--plan",
            "--json",
        ]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(json(&output)["error"]["code"], "planning_refused");
    }
}

#[test]
fn branch_collision_and_checked_out_branch_are_refused() {
    let fixture = RepoFixture::new();
    git(&fixture.repo, ["branch", "collision"]);
    let collision = fixture.command(&[
        "create",
        "--new",
        "collision",
        "--base",
        "HEAD",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert_eq!(collision.status.code(), Some(1));
    assert_eq!(json(&collision)["error"]["code"], "branch_collision");
    let checked_out = fixture.command(&[
        "create",
        "--existing-local",
        "main",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert_eq!(checked_out.status.code(), Some(1));
    assert_eq!(json(&checked_out)["error"]["code"], "branch_checked_out");
}

#[test]
fn remove_by_primary_path_is_refused_without_mutation() {
    let fixture = RepoFixture::new();
    let before = fixture.snapshot();
    let output = fixture.command(&[
        "remove",
        fixture.repo.to_str().unwrap(),
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["error"]["code"], "planning_refused");
    assert_eq!(before, fixture.snapshot());
}

#[test]
fn remove_linked_worktree_by_path_and_unique_branch_is_non_mutating() {
    let fixture = RepoFixture::new();
    let linked = fixture._temp.path().join("linked");
    git(
        &fixture.repo,
        ["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
    );
    let before = fixture.snapshot();
    let by_path = fixture.command(&[
        "remove",
        linked.to_str().unwrap(),
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert_eq!(by_path.status.code(), Some(0));
    assert_eq!(json(&by_path)["data"]["kind"], "remove");
    let by_branch = fixture.command(&[
        "remove",
        "feature",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert_eq!(by_branch.status.code(), Some(0));
    assert_eq!(json(&by_branch)["data"]["kind"], "remove");
    assert!(linked.exists());
    assert_eq!(before, fixture.snapshot());
}

#[test]
fn remove_dirty_worktree_requires_explicit_flag() {
    let fixture = RepoFixture::new();
    let linked = fixture._temp.path().join("dirty");
    git(
        &fixture.repo,
        ["worktree", "add", "-b", "dirty", linked.to_str().unwrap()],
    );
    std::fs::write(linked.join("untracked"), b"dirty").unwrap();
    let refused = fixture.command(&[
        "remove",
        linked.to_str().unwrap(),
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(json(&refused)["error"]["code"], "planning_refused");
    let allowed = fixture.command(&[
        "remove",
        linked.to_str().unwrap(),
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--allow-dirty-removal",
        "--plan",
        "--json",
    ]);
    assert_eq!(allowed.status.code(), Some(0));
    assert_eq!(json(&allowed)["data"]["kind"], "remove");
}

#[cfg(unix)]
#[test]
fn process_paths_resolve_through_repository_and_worktree_aliases() {
    let fixture = RepoFixture::new();
    let repo_alias = fixture._temp.path().join("repo-alias");
    std::os::unix::fs::symlink(&fixture.repo, &repo_alias).unwrap();
    let destination = fixture._temp.path().join("alias-destination");
    let before_create = fixture.snapshot();
    let create = fixture.command(&[
        "create",
        "--new",
        "alias-feature",
        "--base",
        "HEAD",
        "--repo",
        repo_alias.to_str().unwrap(),
        "--path",
        destination.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert!(
        create.status.success(),
        "create failed: stdout={} stderr={}",
        String::from_utf8_lossy(&create.stdout),
        String::from_utf8_lossy(&create.stderr)
    );
    assert_eq!(json(&create)["data"]["kind"], "create");
    assert!(!destination.exists());
    assert_eq!(before_create, fixture.snapshot());

    let linked = fixture._temp.path().join("linked-real");
    git(
        &fixture.repo,
        [
            "worktree",
            "add",
            "-b",
            "alias-linked",
            linked.to_str().unwrap(),
        ],
    );
    let linked_alias = fixture._temp.path().join("linked-alias");
    std::os::unix::fs::symlink(&linked, &linked_alias).unwrap();
    let before_remove = fixture.snapshot();
    let remove = fixture.command(&[
        "remove",
        linked_alias.to_str().unwrap(),
        "--repo",
        repo_alias.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert!(
        remove.status.success(),
        "remove failed: stdout={} stderr={}",
        String::from_utf8_lossy(&remove.stdout),
        String::from_utf8_lossy(&remove.stderr)
    );
    assert_eq!(json(&remove)["data"]["kind"], "remove");
    assert!(linked.exists());
    assert_eq!(before_remove, fixture.snapshot());
}

#[test]
fn enabled_env_rule_is_manifested_without_writing_destination() {
    let fixture = RepoFixture::new();
    std::fs::write(fixture.repo.join(".gitignore"), "**/.env*\n").unwrap();
    std::fs::create_dir_all(fixture.repo.join("app")).unwrap();
    std::fs::write(fixture.repo.join("app/.env.local"), "SECRET=hidden\n").unwrap();
    std::fs::write(fixture.repo.join("app/.env.example"), "example\n").unwrap();
    std::fs::write(fixture.repo.join(".ewtm.toml"), "schema = 1\n[file_rules.env]\nkind = \"copy\"\nmatch_mode = \"glob\"\nsource = \"**/.env*\"\ndestination = \"config\"\nsource_root = \"current_worktree\"\nignored_only = true\nexcludes = [\".env.example\"]\nsensitive = true\nconfirm = true\n").unwrap();
    let destination = fixture._temp.path().join("env-destination");
    let output = fixture.command(&[
        "create",
        "--new",
        "env-plan",
        "--base",
        "HEAD",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--path",
        destination.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let value = json(&output);
    assert_eq!(
        value["data"]["steps"][1]["action"]["CopyFileV3"]["rule"],
        "env"
    );
    assert_eq!(value["data"]["required_consents"][0]["id"], "file-rule:env");
    assert!(!destination.exists());
}

#[test]
fn selected_post_create_task_is_planned_but_never_executed() {
    let fixture = RepoFixture::new();
    std::fs::write(fixture.repo.join(".ewtm.toml"), "schema = 1\n[tasks.check]\nphase = \"post_create\"\nargv = [\"echo\", \"task-would-run\"]\nrequired = true\nenvironment_allowlist = [\"CI\"]\n").unwrap();
    let before = fixture.snapshot();
    let output = fixture.command(&[
        "create",
        "--new",
        "task-plan",
        "--base",
        "HEAD",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--task",
        "check",
        "--plan",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let value = json(&output);
    assert_eq!(
        value["data"]["steps"][1]["action"]["RunTask"]["name"],
        "check"
    );
    assert_eq!(
        value["data"]["steps"][1]["action"]["RunTask"]["required"],
        true
    );
    assert_eq!(
        value["data"]["steps"][1]["action"]["RunTask"]["environment_allowlist"][0],
        "CI"
    );
    assert_eq!(value["data"]["required_consents"][0]["id"], "task:check");
    assert_eq!(before, fixture.snapshot());
}

#[test]
fn skip_rule_omits_manifest_and_accept_rule_grants_consent() {
    let fixture = RepoFixture::new();
    std::fs::write(fixture.repo.join(".gitignore"), ".env\n").unwrap();
    std::fs::write(fixture.repo.join(".env"), "hidden\n").unwrap();
    std::fs::write(fixture.repo.join(".ewtm.toml"), "schema = 1\n[file_rules.env]\nkind = \"copy\"\nmatch_mode = \"glob\"\nsource = \"**/.env*\"\ndestination = \"config\"\nignored_only = true\nsensitive = true\nconfirm = true\n").unwrap();
    let skipped = fixture.command(&[
        "create",
        "--new",
        "skip",
        "--base",
        "HEAD",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--skip-rule",
        "env",
        "--plan",
        "--json",
    ]);
    assert_eq!(skipped.status.code(), Some(0));
    assert_eq!(json(&skipped)["data"]["steps"].as_array().unwrap().len(), 1);
    let accepted = fixture.command(&[
        "create",
        "--new",
        "accept",
        "--base",
        "HEAD",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--accept-rule",
        "env",
        "--plan",
        "--json",
    ]);
    assert_eq!(accepted.status.code(), Some(0));
    let value = json(&accepted);
    assert_eq!(value["data"]["required_consents"][0]["id"], "file-rule:env");
    assert_eq!(value["data"]["granted_consents"][0], "file-rule:env");
}

#[test]
fn unknown_disabled_and_wrong_phase_tasks_are_refused() {
    let fixture = RepoFixture::new();
    std::fs::write(fixture.repo.join(".ewtm.toml"), "schema = 1\n[tasks.disabled]\nphase = \"post_create\"\nargv = [\"echo\"]\nenabled = false\n[tasks.manual]\nphase = \"manual\"\nargv = [\"echo\"]\n").unwrap();
    for name in ["unknown", "disabled", "manual"] {
        let output = fixture.command(&[
            "create",
            "--new",
            name,
            "--base",
            "HEAD",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--task",
            name,
            "--plan",
            "--json",
        ]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            json(&output)["error"]["code"],
            if name == "unknown" {
                "unknown_task"
            } else {
                "planning_refused"
            }
        );
    }
}

#[cfg(unix)]
#[test]
fn unicode_and_space_paths_are_planned_without_lossy_ordering() {
    let fixture = RepoFixture::new();
    std::fs::create_dir_all(fixture.repo.join("spaced Ω/tree")).unwrap();
    std::fs::write(fixture.repo.join("spaced Ω/tree/b"), b"b").unwrap();
    std::fs::write(fixture.repo.join("spaced Ω/tree/a"), b"a").unwrap();
    std::fs::write(fixture.repo.join(".ewtm.toml"), "schema = 1\n[file_rules.tree]\nkind = \"copy_tree\"\nsource = \"spaced Ω/tree\"\ndestination = \"out\"\n").unwrap();
    let output = fixture.command(&[
        "create",
        "--new",
        "unicode",
        "--base",
        "HEAD",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let value = json(&output);
    let steps = value["data"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 3);
    assert!(
        steps[1]["action"]["CopyFileV3"]["source"]
            .to_string()
            .contains("Ω")
    );
}

#[test]
fn task_cwd_defaults_to_future_root_and_is_persisted() {
    let fixture = RepoFixture::new();
    std::fs::write(fixture.repo.join(".ewtm.toml"), "schema = 1\n[tasks.check]\nphase = \"post_create\"\nargv = [\"echo\", \"check\"]\nrequired = true\n").unwrap();
    let output = fixture.command(&[
        "create",
        "--new",
        "cwd",
        "--base",
        "HEAD",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--task",
        "check",
        "--plan",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let value = json(&output);
    let action = &value["data"]["steps"][1]["action"]["RunTask"];
    assert_eq!(
        action["cwd"],
        value["data"]["intent"]["Create"]["destination"]
    );
}
