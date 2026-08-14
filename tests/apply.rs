use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Repo {
    temp: TempDir,
    root: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("repo");
        git(temp.path(), ["init", "-b", "main", root.to_str().unwrap()]);
        git(&root, ["config", "user.name", "integration"]);
        git(
            &root,
            ["config", "user.email", "integration@example.invalid"],
        );
        fs::write(root.join("tracked"), b"initial\n").unwrap();
        git(&root, ["add", "tracked"]);
        git(&root, ["commit", "-m", "initial"]);
        Self { temp, root }
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ewtm"))
            .current_dir(cwd)
            .args(args)
            .env("HOME", self.temp.path().join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap()
    }

    fn state(&self) -> Vec<u8> {
        let mut state = git_output(&self.root, ["worktree", "list", "--porcelain", "-z"]);
        state.extend(git_output(&self.root, ["show-ref"]));
        append_tree_snapshot(&mut state, self.temp.path(), self.temp.path());
        state
    }
}

fn append_tree_snapshot(output: &mut Vec<u8>, root: &Path, path: &Path) {
    let relative = path.strip_prefix(root).unwrap();
    output.extend_from_slice(relative.to_string_lossy().as_bytes());
    let metadata = fs::symlink_metadata(path).unwrap();
    if metadata.file_type().is_symlink() {
        output.extend_from_slice(b"=link:");
        output.extend_from_slice(fs::read_link(path).unwrap().to_string_lossy().as_bytes());
        output.push(0);
        return;
    }
    if metadata.is_file() {
        output.extend_from_slice(b"=file:");
        output.extend_from_slice(&fs::read(path).unwrap());
        output.push(0);
        return;
    }
    output.extend_from_slice(b"=dir\0");
    let mut entries: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap())
        .collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        append_tree_snapshot(output, root, &entry.path());
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
    assert!(output.status.success());
    output.stdout
}

fn plan(repo: &Repo, destination: &Path) -> Vec<u8> {
    plan_with_source(
        repo,
        destination,
        &["--new", "applied-branch", "--base", "HEAD"],
    )
}

fn plan_with_source(repo: &Repo, destination: &Path, source: &[&str]) -> Vec<u8> {
    let mut args = vec!["create"];
    args.extend_from_slice(source);
    args.extend_from_slice(&[
        "--repo",
        repo.root.to_str().unwrap(),
        "--path",
        destination.to_str().unwrap(),
        "--plan",
    ]);
    let output = repo.command(&repo.temp.path().join("outside"), &args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    output.stdout
}

#[test]
fn apply_existing_local_create_and_replay() {
    let repo = Repo::new();
    git(&repo.root, ["branch", "existing"]);
    let outside = repo.temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let destination = repo.temp.path().join("existing-worktree");
    let bytes = plan_with_source(&repo, &destination, &["--existing-local", "existing"]);
    let path = repo.temp.path().join("existing-plan.json");
    fs::write(&path, &bytes).unwrap();
    let apply_args = [
        "apply",
        path.to_str().unwrap(),
        "--confirm-plan",
        &digest(&bytes),
        "--json",
    ];
    let first = repo.command(&outside, &apply_args);
    assert_eq!(
        first.status.code(),
        Some(0),
        "stdout={} stderr={} destination={} worktrees={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr),
        destination.display(),
        String::from_utf8_lossy(&git_output(&repo.root, ["worktree", "list", "--porcelain"],))
    );
    assert_eq!(json(&first)["data"]["outcome"], "applied");
    assert!(destination.is_dir());
    assert_eq!(
        String::from_utf8(git_output(
            &destination,
            ["rev-parse", "--abbrev-ref", "HEAD"],
        ))
        .unwrap(),
        "existing\n"
    );
    let second = repo.command(&outside, &apply_args);
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(json(&second)["data"]["outcome"], "already_applied");
}

#[test]
fn apply_remote_tracking_create_and_replay() {
    let repo = Repo::new();
    let remote = repo.temp.path().join("remote.git");
    git(
        repo.temp.path(),
        ["init", "--bare", remote.to_str().unwrap()],
    );
    git(
        &repo.root,
        ["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repo.root, ["push", "origin", "main"]);
    git(&repo.root, ["fetch", "origin"]);
    let outside = repo.temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let destination = repo.temp.path().join("remote-worktree");
    let bytes = plan_with_source(
        &repo,
        &destination,
        &["--remote", "origin/main", "--local-branch", "remote-branch"],
    );
    let path = repo.temp.path().join("remote-plan.json");
    fs::write(&path, &bytes).unwrap();
    let args = [
        "apply",
        path.to_str().unwrap(),
        "--confirm-plan",
        &digest(&bytes),
        "--json",
    ];
    let first = repo.command(&outside, &args);
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(json(&first)["data"]["outcome"], "applied");
    assert_eq!(
        String::from_utf8(git_output(
            &destination,
            ["rev-parse", "--abbrev-ref", "HEAD"],
        ))
        .unwrap(),
        "remote-branch\n"
    );
    assert!(
        String::from_utf8(git_output(
            &repo.root,
            ["config", "--get", "branch.remote-branch.remote"],
        ))
        .unwrap()
        .contains("origin")
    );
    let second = repo.command(&outside, &args);
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(json(&second)["data"]["outcome"], "already_applied");
}

#[test]
fn missing_grant_is_refused_before_mutation() {
    let repo = Repo::new();
    fs::write(repo.root.join(".gitignore"), ".env\n").unwrap();
    fs::write(repo.root.join(".env"), "SECRET=hidden\n").unwrap();
    fs::write(
        repo.root.join(".ewtm.toml"),
        "schema = 1\n[file_rules.env]\nkind = \"copy\"\nmatch_mode = \"glob\"\nsource = \"**/.env*\"\ndestination = \"config\"\nignored_only = true\nsensitive = true\nconfirm = true\n",
    )
    .unwrap();
    let outside = repo.temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let destination = repo.temp.path().join("missing-grant-worktree");
    let bytes = plan(&repo, &destination);
    let path = repo.temp.path().join("missing-grant-plan.json");
    fs::write(&path, &bytes).unwrap();
    let before = repo.state();
    let output = repo.command(
        &outside,
        &[
            "apply",
            path.to_str().unwrap(),
            "--confirm-plan",
            &digest(&bytes),
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["error"]["code"], "missing_consent");
    assert_eq!(before, repo.state());
    assert!(!destination.exists());
}

fn digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn json(output: &Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[cfg(unix)]
fn run_real_signal_apply(signal: rustix::process::Signal, expected_exit: i32) {
    let repo = Repo::new();
    let outside = repo.temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(
        repo.root.join(".ewtm.toml"),
        "schema = 1\n[tasks.block]\nphase = \"post_create\"\nargv = [\"/bin/sh\", \"-c\", \"echo $$ > pid; echo ready > ready; trap 'exit 0' INT TERM; while :; do sleep 1; done\"]\nrequired = true\n",
    )
    .unwrap();
    let destination = repo.temp.path().join("signal-worktree");
    let plan_output = repo.command(
        &outside,
        &[
            "create",
            "--new",
            "signal-branch",
            "--base",
            "HEAD",
            "--repo",
            repo.root.to_str().unwrap(),
            "--path",
            destination.to_str().unwrap(),
            "--task",
            "block",
            "--plan",
        ],
    );
    assert!(plan_output.status.success());
    let plan_path = repo.temp.path().join("signal-plan.json");
    fs::write(&plan_path, &plan_output.stdout).unwrap();
    let before = repo.state();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ewtm"))
        .current_dir(&outside)
        .args([
            "apply",
            plan_path.to_str().unwrap(),
            "--confirm-plan",
            &digest(&plan_output.stdout),
        ])
        .env("HOME", repo.temp.path().join("home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let ready = destination.join("ready");
    let deadline = Instant::now() + Duration::from_secs(8);
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ready.exists(), "task did not become ready");
    let pid: i32 = fs::read_to_string(destination.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let child_pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    rustix::process::kill_process(child_pid, signal).unwrap();

    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("apply did not exit after signal");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = child.wait_with_output().unwrap();
    assert_eq!(status.code(), Some(expected_exit));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("needs_attention: execution needs attention"),
        "{stderr}"
    );
    assert!(
        rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()).is_err()
    );
    assert!(
        rustix::process::test_kill_process_group(rustix::process::Pid::from_raw(pid).unwrap())
            .is_err()
    );
    let recovery = repo.command(
        &outside,
        &[
            "recover",
            "list",
            "--repo",
            repo.root.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(recovery.status.code(), Some(0));
    assert_eq!(json(&recovery)["data"][0]["status"], "needs_attention");
    assert_ne!(before, repo.state());
}

#[cfg(unix)]
#[test]
fn real_apply_sigint_returns_130_with_truthful_outcome() {
    run_real_signal_apply(rustix::process::Signal::INT, 130);
}

#[cfg(unix)]
#[test]
fn real_apply_sigterm_returns_143_with_truthful_outcome() {
    run_real_signal_apply(rustix::process::Signal::TERM, 143);
}

#[test]
fn apply_create_and_replay_are_idempotent() {
    let repo = Repo::new();
    let outside = repo.temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let destination = repo.temp.path().join("worktree");
    let bytes = plan(&repo, &destination);
    let plan_path = repo.temp.path().join("plan.json");
    fs::write(&plan_path, &bytes).unwrap();
    let before = repo.state();

    let first = repo.command(
        &outside,
        &[
            "apply",
            plan_path.to_str().unwrap(),
            "--confirm-plan",
            &digest(&bytes),
            "--json",
        ],
    );
    assert_eq!(first.status.code(), Some(0));
    let first_value = json(&first);
    assert_eq!(first_value["command"], "apply");
    assert_eq!(first_value["ok"], true);
    assert_eq!(first_value["data"]["outcome"], "applied");
    assert!(destination.is_dir());
    assert!(
        String::from_utf8(git_output(
            &repo.root,
            ["branch", "--list", "applied-branch"],
        ))
        .unwrap()
        .contains("applied-branch")
    );
    let recovery = repo.command(
        &outside,
        &[
            "recover",
            "list",
            "--repo",
            repo.root.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(recovery.status.code(), Some(0));
    let recovery_value = json(&recovery);
    assert_eq!(recovery_value["data"][0]["status"], "applied");
    assert!(!repo.state().is_empty());
    assert_ne!(before, repo.state());

    let after_first = repo.state();
    let second = repo.command(
        &outside,
        &[
            "apply",
            plan_path.to_str().unwrap(),
            "--confirm-plan",
            &digest(&bytes),
            "--json",
        ],
    );
    assert_eq!(
        second.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(json(&second)["data"]["outcome"], "already_applied");
    assert_eq!(after_first, repo.state());
}

#[test]
fn apply_refuses_invalid_inputs_before_mutation() {
    type RefusalCase = (&'static str, Box<dyn Fn(&Repo, &Path) -> (PathBuf, String)>);

    let mut cases: Vec<RefusalCase> = vec![
        (
            "malformed_digest",
            Box::new(|repo, _| {
                let bytes = plan(repo, &repo.temp.path().join("malformed-destination"));
                let path = repo.temp.path().join("plan");
                fs::write(&path, bytes).unwrap();
                (path, "bad".into())
            }),
        ),
        (
            "wrong_digest",
            Box::new(|repo, _| {
                let bytes = plan(repo, &repo.temp.path().join("wrong-digest-destination"));
                let path = repo.temp.path().join("plan");
                fs::write(&path, bytes).unwrap();
                (path, "0".repeat(64))
            }),
        ),
        (
            "malformed_json",
            Box::new(|repo, _| {
                let path = repo.temp.path().join("plan");
                fs::write(&path, b"not json").unwrap();
                (path, digest(b"not json"))
            }),
        ),
        (
            "duplicate_key",
            Box::new(|repo, _| {
                let bytes = br#"{"operation_id":1,"operation_id":2}"#;
                let path = repo.temp.path().join("plan");
                fs::write(&path, bytes).unwrap();
                (path, digest(bytes))
            }),
        ),
        (
            "noncanonical_defaulted_field",
            Box::new(|repo, _| {
                let original = plan(repo, &repo.temp.path().join("defaulted-destination"));
                let mut value: Value = serde_json::from_slice(&original).unwrap();
                value["intent"]["Create"]
                    .as_object_mut()
                    .unwrap()
                    .remove("task_contracts");
                let bytes = serde_json::to_vec_pretty(&value).unwrap();
                let path = repo.temp.path().join("plan");
                fs::write(&path, &bytes).unwrap();
                (path, digest(&bytes))
            }),
        ),
        (
            "missing",
            Box::new(|repo, _| (repo.temp.path().join("missing"), "0".repeat(64))),
        ),
        (
            "stdin_path",
            Box::new(|_repo, _| (PathBuf::from("-"), "0".repeat(64))),
        ),
        (
            "directory",
            Box::new(|repo, _| {
                let path = repo.temp.path().join("directory");
                fs::create_dir(&path).unwrap();
                (path, "0".repeat(64))
            }),
        ),
        (
            "oversize",
            Box::new(|repo, _| {
                let path = repo.temp.path().join("large");
                let file = fs::File::create(&path).unwrap();
                file.set_len(16 * 1024 * 1024 + 1).unwrap();
                (path, "0".repeat(64))
            }),
        ),
    ];
    #[cfg(unix)]
    {
        cases.push((
            "symlink",
            Box::new(|repo, _| {
                let target = repo.temp.path().join("target");
                let path = repo.temp.path().join("symlink");
                fs::write(&target, b"target").unwrap();
                std::os::unix::fs::symlink(&target, &path).unwrap();
                (path, "0".repeat(64))
            }),
        ));
    }
    cases.push((
        "drifted_redigested_plan",
        Box::new(|repo, _| {
            let destination = repo.temp.path().join("planned");
            let bytes = plan(repo, &destination);
            let mut value: Value = serde_json::from_slice(&bytes).unwrap();
            value["intent"]["Create"]["source"]["NewBranch"]["base"] = Value::String("main".into());
            let bytes = serde_json::to_vec_pretty(&value).unwrap();
            let path = repo.temp.path().join("drifted-plan");
            fs::write(&path, &bytes).unwrap();
            (path, digest(&bytes))
        }),
    ));
    for (name, make_case) in cases {
        let repo = Repo::new();
        let outside = repo.temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let (path, expected) = make_case(&repo, &outside);
        let before = repo.state();
        let output = repo.command(
            &outside,
            &[
                "apply",
                path.to_str().unwrap(),
                "--confirm-plan",
                &expected,
                "--json",
            ],
        );
        assert_eq!(output.status.code(), Some(1), "{name}");
        let value = json(&output);
        assert_eq!(value["command"], "apply", "{name}");
        assert!(!value["ok"].as_bool().unwrap(), "{name}");
        let expected = match name {
            "malformed_digest" => "plan_digest_invalid",
            "wrong_digest" => "plan_digest_mismatch",
            "malformed_json" | "duplicate_key" => "plan_json_invalid",
            "noncanonical_defaulted_field" => "plan_noncanonical",
            "drifted_redigested_plan" => "plan_not_executable",
            "missing" => "plan_file_open",
            "stdin_path" => "plan_file_not_regular",
            "symlink" | "directory" => "plan_file_not_regular",
            "oversize" => "plan_file_too_large",
            other => panic!("unexpected case {other}"),
        };
        assert_eq!(value["error"]["code"], expected, "{name}");
        assert_eq!(before, repo.state(), "{name}");
    }
}

#[cfg(unix)]
#[test]
fn text_apply_failure_is_stderr_only() {
    let repo = Repo::new();
    let output = repo.command(
        repo.temp.path(),
        &["apply", "-", "--confirm-plan", &"0".repeat(64)],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "plan_file_not_regular: plan file is not regular\n"
    );
}
