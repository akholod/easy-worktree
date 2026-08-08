use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    repo: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        git(temp.path(), ["init", "-b", "main", repo.to_str().unwrap()]);
        git(&repo, ["config", "user.name", "recovery"]);
        git(&repo, ["config", "user.email", "recovery@example.invalid"]);
        std::fs::write(repo.join("tracked"), b"initial").unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "initial"]);
        Self { temp, repo }
    }
    fn command(&self, args: &[&str]) -> Output {
        self.command_from(None, args)
    }
    fn command_from(&self, cwd: Option<&Path>, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ewtm"));
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
            .args(args)
            .env("HOME", self.temp.path().join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap()
    }
    fn journal_dir(&self) -> PathBuf {
        self.repo.join(".git/ewtm/journal")
    }
    fn seed_journal(&self) -> (String, Vec<u8>) {
        let destination = self.temp.path().join("planned-destination");
        let output = self.command(&[
            "create",
            "--new",
            "recovery-seed",
            "--base",
            "HEAD",
            "--repo",
            self.repo.to_str().unwrap(),
            "--path",
            destination.to_str().unwrap(),
            "--plan",
            "--json",
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let plan = json(&output)["data"].clone();
        let id = plan["operation_id"].as_str().unwrap().to_owned();
        let steps = plan["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| {
                serde_json::json!({
                    "id": step["id"], "status": "pending"
                })
            })
            .collect::<Vec<_>>();
        let journal = serde_json::json!({
            "schema_version": 1, "revision": 0, "operation_id": id,
            "plan": plan, "status": "pending", "steps": steps
        });
        let bytes = serde_json::to_vec_pretty(&journal).unwrap();
        std::fs::create_dir_all(self.journal_dir()).unwrap();
        std::fs::write(self.journal_dir().join(format!("{id}.json")), &bytes).unwrap();
        (id, bytes)
    }
}
fn git<'a>(cwd: &Path, args: impl IntoIterator<Item = &'a str>) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "recovery")
        .env("GIT_AUTHOR_EMAIL", "recovery@example.invalid")
        .env("GIT_COMMITTER_NAME", "recovery")
        .env("GIT_COMMITTER_EMAIL", "recovery@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn json(output: &Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn journal_snapshot(fixture: &Fixture) -> Vec<(PathBuf, Vec<u8>)> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(fixture.journal_dir())
        .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
        .unwrap_or_default();
    paths.sort();
    paths
        .into_iter()
        .map(|path| (path.clone(), std::fs::read(path).unwrap()))
        .collect()
}

#[test]
fn empty_recovery_is_read_only_and_json_clean() {
    let fixture = Fixture::new();
    let before = git_output(&fixture.repo, ["worktree", "list", "--porcelain", "-z"]);
    let output = fixture.command(&[
        "recover",
        "list",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json(&output)["data"], serde_json::json!([]));
    assert_eq!(
        before,
        git_output(&fixture.repo, ["worktree", "list", "--porcelain", "-z"])
    );
    assert!(!fixture.repo.join(".git/ewtm").exists());
}

#[test]
fn invalid_missing_and_corrupt_recovery_are_typed_and_fail_closed() {
    let fixture = Fixture::new();
    let invalid = fixture.command(&[
        "recover",
        "show",
        "not-an-id",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(invalid.status.code(), Some(1));
    assert_eq!(json(&invalid)["error"]["code"], "invalid_operation_id");
    let missing = fixture.command(&[
        "recover",
        "show",
        "00000000-0000-0000-0000-000000000000",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(json(&missing)["error"]["code"], "journal_not_found");
    std::fs::create_dir_all(fixture.journal_dir()).unwrap();
    std::fs::write(fixture.journal_dir().join("bad.json"), b"{").unwrap();
    let corrupt = fixture.command(&[
        "recover",
        "list",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(corrupt.status.code(), Some(1));
    assert_eq!(json(&corrupt)["error"]["code"], "journal_corrupt");
}

#[test]
fn valid_recovery_list_and_show_are_read_only_in_json_text_and_default_repo_mode() {
    let fixture = Fixture::new();
    let (id, bytes) = fixture.seed_journal();
    let refs_before = git_output(&fixture.repo, ["show-ref"]);
    let worktrees_before = git_output(&fixture.repo, ["worktree", "list", "--porcelain", "-z"]);
    let journal_before = journal_snapshot(&fixture);
    let list = fixture.command(&[
        "recover",
        "list",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--json",
    ]);
    assert!(list.status.success());
    assert_eq!(json(&list)["data"][0]["operation_id"], id);
    assert_eq!(json(&list)["data"][0]["status"], "pending");
    assert_eq!(json(&list)["data"][0]["revision"], 0);
    let show = fixture.command(&[
        "recover",
        "show",
        &id,
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--json",
    ]);
    assert!(show.status.success());
    assert_eq!(json(&show)["data"]["operation_id"], id);
    let text = fixture.command(&[
        "recover",
        "show",
        &id,
        "--repo",
        fixture.repo.to_str().unwrap(),
    ]);
    assert!(text.status.success());
    assert!(text.stderr.is_empty());
    let text = String::from_utf8(text.stdout).unwrap();
    assert_eq!(
        text,
        format!("operation {id}\nstatus: pending\nrevision: 0\n")
    );
    let default = fixture.command_from(Some(&fixture.repo), &["recover", "list", "--json"]);
    assert!(default.status.success());
    assert_eq!(json(&default)["data"][0]["operation_id"], id);
    let list_text = fixture.command(&["recover", "list", "--repo", fixture.repo.to_str().unwrap()]);
    assert!(list_text.status.success());
    assert!(list_text.stderr.is_empty());
    assert_eq!(
        String::from_utf8(list_text.stdout).unwrap(),
        format!("{id}\tpending\trevision 0\n")
    );
    assert_eq!(
        bytes,
        std::fs::read(fixture.journal_dir().join(format!("{id}.json"))).unwrap()
    );
    assert_eq!(refs_before, git_output(&fixture.repo, ["show-ref"]));
    assert_eq!(
        worktrees_before,
        git_output(&fixture.repo, ["worktree", "list", "--porcelain", "-z"])
    );
    assert_eq!(journal_before, journal_snapshot(&fixture));
}

#[test]
fn non_repository_and_filename_identity_corruption_fail_closed() {
    let fixture = Fixture::new();
    let outside = fixture.temp.path().join("not-a-repository");
    std::fs::create_dir(&outside).unwrap();
    let output = fixture.command(&[
        "recover",
        "list",
        "--repo",
        outside.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["error"]["code"], "repository_error");
    let (id, bytes) = fixture.seed_journal();
    let other = "00000000-0000-0000-0000-000000000001";
    std::fs::write(fixture.journal_dir().join(format!("{other}.json")), &bytes).unwrap();
    let output = fixture.command(&[
        "recover",
        "list",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["error"]["code"], "journal_corrupt");
    let output = fixture.command(&[
        "recover",
        "show",
        other,
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["error"]["code"], "journal_corrupt");
    assert!(fixture.journal_dir().join(format!("{id}.json")).exists());
}

#[test]
fn truncated_valid_uuid_journal_fails_list_and_show_as_corrupt() {
    let fixture = Fixture::new();
    let (id, _) = fixture.seed_journal();
    std::fs::write(fixture.journal_dir().join(format!("{id}.json")), b"{\n").unwrap();
    let list = fixture.command(&[
        "recover",
        "list",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(list.status.code(), Some(1));
    assert_eq!(json(&list)["error"]["code"], "journal_corrupt");
    let show = fixture.command(&[
        "recover",
        "show",
        &id,
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(show.status.code(), Some(1));
    assert_eq!(json(&show)["error"]["code"], "journal_corrupt");
}

#[test]
fn plan_only_create_and_remove_do_not_create_recovery_state_or_mutate_git() {
    let fixture = Fixture::new();
    let destination = fixture.temp.path().join("planned");
    let create_before = git_output(&fixture.repo, ["worktree", "list", "--porcelain", "-z"]);
    let create = fixture.command(&[
        "create",
        "--new",
        "no-execution",
        "--base",
        "HEAD",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--path",
        destination.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert!(create.status.success());
    assert_eq!(
        create_before,
        git_output(&fixture.repo, ["worktree", "list", "--porcelain", "-z"])
    );
    assert!(!fixture.repo.join(".git/ewtm").exists());

    let linked = fixture.temp.path().join("linked");
    git(
        &fixture.repo,
        [
            "worktree",
            "add",
            "-b",
            "planned-remove",
            linked.to_str().unwrap(),
        ],
    );
    let refs_before = git_output(&fixture.repo, ["show-ref"]);
    let worktrees_before = git_output(&fixture.repo, ["worktree", "list", "--porcelain", "-z"]);
    let remove = fixture.command(&[
        "remove",
        linked.to_str().unwrap(),
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--plan",
        "--json",
    ]);
    assert!(remove.status.success());
    assert_eq!(refs_before, git_output(&fixture.repo, ["show-ref"]));
    assert_eq!(
        worktrees_before,
        git_output(&fixture.repo, ["worktree", "list", "--porcelain", "-z"])
    );
    assert!(!fixture.repo.join(".git/ewtm").exists());
}

fn git_output<'a>(cwd: &Path, args: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success());
    output.stdout
}
