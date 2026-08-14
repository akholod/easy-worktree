#![cfg(unix)]

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
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
        git(temp.path(), &["init", "-b", "main", root.to_str().unwrap()]);
        git(&root, &["config", "user.name", "integration"]);
        git(
            &root,
            &["config", "user.email", "integration@example.invalid"],
        );
        fs::write(root.join("tracked"), b"initial\n").unwrap();
        fs::create_dir_all(temp.path().join("outside")).unwrap();
        git(&root, &["add", "tracked"]);
        git(&root, &["commit", "-m", "initial"]);
        Self { temp, root }
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
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
}

fn git(cwd: &Path, args: &[&str]) {
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

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn domain_digest(domain: &[u8], bytes: &[u8]) -> String {
    let mut input = domain.to_vec();
    input.extend_from_slice(bytes);
    digest(&input)
}

fn compact_json_preserving_order(raw: &[u8]) -> Vec<u8> {
    let mut compact = Vec::with_capacity(raw.len());
    let mut quoted = false;
    let mut escaped = false;
    for byte in raw {
        if quoted {
            compact.push(*byte);
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
        } else if *byte == b'"' {
            quoted = true;
            compact.push(*byte);
        } else if !byte.is_ascii_whitespace() {
            compact.push(*byte);
        }
    }
    compact
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

fn append_snapshot(out: &mut Vec<u8>, root: &Path, path: &Path) {
    let relative = path.strip_prefix(root).unwrap();
    out.extend_from_slice(b"path:");
    out.extend_from_slice(relative.as_os_str().as_encoded_bytes());
    let metadata = fs::symlink_metadata(path).unwrap();
    #[cfg(unix)]
    out.extend_from_slice(
        format!(
            " mode:{:o}",
            std::os::unix::fs::PermissionsExt::mode(&metadata.permissions())
        )
        .as_bytes(),
    );
    if metadata.file_type().is_symlink() {
        out.extend_from_slice(b" type:symlink target:");
        out.extend_from_slice(fs::read_link(path).unwrap().as_os_str().as_encoded_bytes());
        out.push(0);
    } else if metadata.is_file() {
        out.extend_from_slice(b" type:file bytes:");
        out.extend_from_slice(&fs::read(path).unwrap());
        out.push(0);
    } else if metadata.is_dir() {
        out.extend_from_slice(b" type:directory\0");
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            append_snapshot(out, root, &entry);
        }
    } else {
        out.extend_from_slice(b" type:special\0");
    }
}

fn snapshot(repo: &Repo) -> Vec<u8> {
    let mut out = Vec::new();
    append_snapshot(&mut out, repo.temp.path(), repo.temp.path());
    out.extend_from_slice(b"git:refs\0");
    out.extend_from_slice(&git_output(&repo.root, &["show-ref"]));
    out.extend_from_slice(b"git:worktrees\0");
    out.extend_from_slice(&git_output(
        &repo.root,
        &["worktree", "list", "--porcelain", "-z"],
    ));
    for marker in [
        ".git/ewtm/compensation",
        ".git/ewtm/staging",
        ".git/ewtm/journal",
    ] {
        let path = repo.root.join(marker);
        out.extend_from_slice(b"marker:");
        out.extend_from_slice(marker.as_bytes());
        out.push(if path.exists() { 1 } else { 0 });
    }
    out
}

fn git_output(cwd: &Path, args: &[&str]) -> Vec<u8> {
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

fn propose(repo: &Repo, args: &[&str]) -> Output {
    let before = snapshot(repo);
    let output = repo.run(&repo.temp.path().join("outside"), args);
    let after = snapshot(repo);
    assert_eq!(before, after, "proposal mutated repository");
    output
}

fn create_and_apply(repo: &Repo, destination: &Path) -> String {
    let outside = repo.temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let plan = repo.run(
        &outside,
        &[
            "create",
            "--repo",
            repo.root.to_str().unwrap(),
            "--new",
            "proposal-branch",
            "--base",
            "HEAD",
            "--path",
            destination.to_str().unwrap(),
            "--plan",
        ],
    );
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let plan_bytes = plan.stdout;
    let plan_path = repo.temp.path().join("plan.json");
    fs::write(&plan_path, &plan_bytes).unwrap();
    let applied = repo.run(
        &outside,
        &[
            "apply",
            plan_path.to_str().unwrap(),
            "--confirm-plan",
            &digest(&plan_bytes),
            "--json",
        ],
    );
    assert!(
        applied.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    json(&applied)["data"]["operation_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn create_and_apply_existing(repo: &Repo, destination: &Path) -> String {
    git(&repo.root, &["branch", "existing-proposal"]);
    let outside = repo.temp.path().join("outside-existing");
    fs::create_dir_all(&outside).unwrap();
    let plan = repo.run(
        &outside,
        &[
            "create",
            "--repo",
            repo.root.to_str().unwrap(),
            "--existing-local",
            "existing-proposal",
            "--path",
            destination.to_str().unwrap(),
            "--plan",
        ],
    );
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let plan_path = repo.temp.path().join("existing-plan.json");
    fs::write(&plan_path, &plan.stdout).unwrap();
    let applied = repo.run(
        &outside,
        &[
            "apply",
            plan_path.to_str().unwrap(),
            "--confirm-plan",
            &digest(&plan.stdout),
            "--json",
        ],
    );
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    json(&applied)["data"]["operation_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn applied_new_branch_proposes_read_only_reverse_plan_in_text_and_json() {
    let repo = Repo::new();
    let destination = repo.temp.path().join("proposal-worktree");
    let operation_id = create_and_apply(&repo, &destination);
    let proposal = propose(
        &repo,
        &[
            "recover",
            "propose-compensation",
            &operation_id,
            "--repo",
            repo.root.to_str().unwrap(),
            "--allow-worktree",
            "--allow-local-branch",
            "--json",
        ],
    );
    assert!(
        proposal.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&proposal.stdout),
        String::from_utf8_lossy(&proposal.stderr)
    );
    let wire = json(&proposal);
    assert_eq!(wire["schema_version"], 1);
    assert_eq!(wire["command"], "recover_propose_compensation");
    assert_eq!(wire["ok"], true);
    assert_eq!(wire["data"]["executable"], false);
    let proposal_id = wire["data"]["proposal_id"].as_str().unwrap();
    let parsed_id = uuid::Uuid::parse_str(proposal_id).unwrap();
    assert_eq!(parsed_id.get_version_num(), 4);
    assert_eq!(proposal_id, parsed_id.hyphenated().to_string());
    let journal_path = repo
        .root
        .join(".git/ewtm/journal")
        .join(format!("{operation_id}.json"));
    let journal_bytes = fs::read(&journal_path).unwrap();
    let journal_json: Value = serde_json::from_slice(&journal_bytes).unwrap();
    assert_eq!(wire["data"]["source"]["operation_id"], operation_id);
    assert_eq!(wire["data"]["source"]["plan_schema_version"], 3);
    assert_eq!(
        wire["data"]["source"]["journal_schema_version"],
        journal_json["schema_version"]
    );
    assert_eq!(
        wire["data"]["source"]["journal_revision"],
        journal_json["revision"]
    );
    assert_eq!(
        wire["data"]["source"]["forward_journal_digest"],
        domain_digest(b"ewtm:forward-journal:v1\0", &journal_bytes)
    );
    let compact_plan =
        compact_json_preserving_order(&fs::read(repo.temp.path().join("plan.json")).unwrap());
    assert_eq!(
        wire["data"]["source"]["forward_plan_digest"],
        domain_digest(b"ewtm:forward-plan:v1\0", &compact_plan)
    );
    assert_eq!(
        wire["data"]["steps"][0]["action"]["kind"],
        "remove_created_worktree"
    );
    assert_eq!(
        wire["data"]["steps"][1]["action"]["kind"],
        "delete_created_local_branch"
    );
    assert_eq!(
        wire["data"]["allowed_categories"],
        serde_json::json!(["worktree", "local_branch"])
    );
    let journal = journal_bytes;
    let second = propose(
        &repo,
        &[
            "recover",
            "propose-compensation",
            &operation_id,
            "--repo",
            repo.root.to_str().unwrap(),
            "--allow-worktree",
            "--allow-local-branch",
            "--json",
        ],
    );
    assert!(second.status.success());
    let second_wire = json(&second);
    assert_ne!(
        wire["data"]["proposal_id"],
        second_wire["data"]["proposal_id"]
    );
    let mut first_without_id = wire["data"].clone();
    let mut second_without_id = second_wire["data"].clone();
    first_without_id["proposal_id"] = Value::Null;
    second_without_id["proposal_id"] = Value::Null;
    assert_eq!(first_without_id, second_without_id);
    assert_eq!(journal, fs::read(&journal_path).unwrap());

    let text = propose(
        &repo,
        &[
            "recover",
            "propose-compensation",
            &operation_id,
            "--repo",
            repo.root.to_str().unwrap(),
            "--allow-worktree",
            "--allow-local-branch",
        ],
    );
    assert!(text.status.success());
    assert!(text.stdout.starts_with(b"{\n"));
    assert!(text.stderr.is_empty());
    let text_wire: Value = serde_json::from_slice(&text.stdout).unwrap();
    assert_eq!(text_wire["steps"], wire["data"]["steps"]);
    let mut text_without_id = text_wire.clone();
    text_without_id["proposal_id"] = Value::Null;
    let mut json_without_id = wire["data"].clone();
    json_without_id["proposal_id"] = Value::Null;
    assert_eq!(text_without_id, json_without_id);
}

#[test]
fn proposal_refusals_have_stable_text_output_and_no_stdout() {
    let repo = Repo::new();
    let existing = create_and_apply(&repo, &repo.temp.path().join("existing-worktree"));
    let id = "00000000-0000-4000-8000-000000000001";
    assert_ne!(existing, id);
    let output = propose(
        &repo,
        &[
            "recover",
            "propose-compensation",
            id,
            "--repo",
            repo.root.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "forward_operation_not_found: forward operation was not found\n"
    );
}

#[test]
fn invalid_operation_id_is_stable_and_does_not_access_repository() {
    let repo = Repo::new();
    let missing = repo.temp.path().join("missing-repository");
    let output = repo.run(
        &repo.temp.path().join("outside"),
        &[
            "recover",
            "propose-compensation",
            "not-an-operation-id",
            "--repo",
            missing.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["error"]["code"], "invalid_operation_id");
    assert_eq!(json(&output)["error"]["message"], "invalid operation id");
    assert!(!missing.exists());
}

#[test]
fn applied_proposal_rejects_missing_and_unrelated_allowances_without_stdout() {
    let repo = Repo::new();
    let destination = repo.temp.path().join("refusal-worktree");
    let operation_id = create_and_apply(&repo, &destination);
    let missing = propose(
        &repo,
        &[
            "recover",
            "propose-compensation",
            &operation_id,
            "--repo",
            repo.root.to_str().unwrap(),
            "--allow-worktree",
        ],
    );
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    assert_eq!(
        String::from_utf8(missing.stderr).unwrap(),
        "compensation_missing_allow: required compensation allowance is missing\n"
    );
    let unrelated = propose(
        &repo,
        &[
            "recover",
            "propose-compensation",
            &operation_id,
            "--repo",
            repo.root.to_str().unwrap(),
            "--allow-worktree",
            "--allow-local-branch",
            "--allow-file-artifact",
        ],
    );
    assert_eq!(unrelated.status.code(), Some(1));
    assert!(unrelated.stdout.is_empty());
    assert_eq!(
        String::from_utf8(unrelated.stderr).unwrap(),
        "compensation_unrelated_allow: compensation allowance is unrelated\n"
    );
}

#[test]
fn existing_local_proposal_has_no_derived_branch_action() {
    let repo = Repo::new();
    let destination = repo.temp.path().join("existing-worktree");
    let operation_id = create_and_apply_existing(&repo, &destination);
    let output = propose(
        &repo,
        &[
            "recover",
            "propose-compensation",
            &operation_id,
            "--repo",
            repo.root.to_str().unwrap(),
            "--allow-worktree",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = json(&output)["data"].clone();
    assert_eq!(data["allowed_categories"], serde_json::json!(["worktree"]));
    assert_eq!(data["steps"].as_array().unwrap().len(), 1);
    assert_eq!(
        data["steps"][0]["action"]["kind"],
        "remove_created_worktree"
    );
    assert_eq!(
        data["steps"][0]["action"]["descriptor"]["branch_was_created"],
        false
    );
    let again = propose(
        &repo,
        &[
            "recover",
            "propose-compensation",
            &operation_id,
            "--repo",
            repo.root.to_str().unwrap(),
            "--allow-worktree",
            "--json",
        ],
    );
    assert!(again.status.success());
    let again_data = json(&again)["data"].clone();
    assert_ne!(data["proposal_id"], again_data["proposal_id"]);
    let mut first = data;
    first["proposal_id"] = Value::Null;
    let mut second = again_data;
    second["proposal_id"] = Value::Null;
    assert_eq!(first, second);
}

#[test]
fn regular_v3_artifact_proposal_precedes_worktree_and_is_read_only() {
    let repo = Repo::new();
    fs::write(repo.root.join(".gitignore"), b".env\n").unwrap();
    fs::write(repo.root.join(".env"), b"secret-value\n").unwrap();
    fs::create_dir(repo.root.join("config")).unwrap();
    git(&repo.root, &["add", ".gitignore", "config"]);
    git(&repo.root, &["commit", "-m", "config"]);
    fs::write(repo.root.join(".ewtm.toml"), "schema = 1\n[file_rules.env]\nkind = \"copy\"\nmatch_mode = \"glob\"\nsource = \"**/.env*\"\ndestination = \".\"\nignored_only = true\nsensitive = false\nconfirm = false\n").unwrap();
    let destination = repo.temp.path().join("artifact-worktree");
    let outside = repo.temp.path().join("artifact-outside");
    fs::create_dir(&outside).unwrap();
    let plan = repo.run(
        &outside,
        &[
            "create",
            "--repo",
            repo.root.to_str().unwrap(),
            "--new",
            "artifact-branch",
            "--base",
            "HEAD",
            "--path",
            destination.to_str().unwrap(),
            "--plan",
        ],
    );
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let plan_path = repo.temp.path().join("artifact-plan.json");
    fs::write(&plan_path, &plan.stdout).unwrap();
    let applied = repo.run(
        &outside,
        &[
            "apply",
            plan_path.to_str().unwrap(),
            "--confirm-plan",
            &digest(&plan.stdout),
            "--json",
        ],
    );
    assert!(
        applied.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        fs::read(destination.join(".env")).unwrap(),
        b"secret-value\n"
    );
    let operation_id = json(&applied)["data"]["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let output = propose(
        &repo,
        &[
            "recover",
            "propose-compensation",
            &operation_id,
            "--repo",
            repo.root.to_str().unwrap(),
            "--allow-file-artifact",
            "--allow-worktree",
            "--allow-local-branch",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = json(&output)["data"].clone();
    assert_eq!(
        data["allowed_categories"],
        serde_json::json!(["file_artifact", "worktree", "local_branch"])
    );
    let steps = data["steps"].as_array().unwrap();
    assert_eq!(steps[0]["action"]["kind"], "remove_created_artifact_v3");
    assert_eq!(steps[1]["action"]["kind"], "remove_created_worktree");
    assert_eq!(steps[2]["action"]["kind"], "delete_created_local_branch");
    assert_eq!(
        steps[0]["action"]["descriptor"]["path"],
        destination.join("./.env").to_str().unwrap()
    );
    assert!(
        steps[0]["action"]["descriptor"]["staging"]["path"]
            .as_str()
            .unwrap()
            .starts_with(destination.to_str().unwrap())
    );
    assert_eq!(
        steps[0]["action"]["descriptor"]["staging"]["ownership_token"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
}

#[test]
fn regular_artifact_drift_table_is_state_changed_without_writes() {
    for case in 0..5 {
        let repo = Repo::new();
        fs::write(repo.root.join(".gitignore"), b".env\n").unwrap();
        fs::write(repo.root.join(".env"), b"secret-value\n").unwrap();
        fs::write(repo.root.join(".ewtm.toml"), "schema = 1\n[file_rules.env]\nkind = \"copy\"\nmatch_mode = \"glob\"\nsource = \"**/.env*\"\ndestination = \".\"\nignored_only = true\nsensitive = false\nconfirm = false\n").unwrap();
        let destination = repo.temp.path().join("artifact-drift-worktree");
        let outside = repo.temp.path().join("artifact-drift-outside");
        fs::create_dir(&outside).unwrap();
        let plan = repo.run(
            &outside,
            &[
                "create",
                "--repo",
                repo.root.to_str().unwrap(),
                "--new",
                "artifact-drift",
                "--base",
                "HEAD",
                "--path",
                destination.to_str().unwrap(),
                "--plan",
            ],
        );
        assert!(plan.status.success());
        let plan_path = repo.temp.path().join("artifact-drift-plan.json");
        fs::write(&plan_path, &plan.stdout).unwrap();
        let applied = repo.run(
            &outside,
            &[
                "apply",
                plan_path.to_str().unwrap(),
                "--confirm-plan",
                &digest(&plan.stdout),
                "--json",
            ],
        );
        assert!(
            applied.status.success(),
            "{}",
            String::from_utf8_lossy(&applied.stdout)
        );
        let operation_id = json(&applied)["data"]["operation_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let flags = [
            "--allow-file-artifact",
            "--allow-worktree",
            "--allow-local-branch",
        ];
        let initial = propose(
            &repo,
            &[
                "recover",
                "propose-compensation",
                &operation_id,
                "--repo",
                repo.root.to_str().unwrap(),
                flags[0],
                flags[1],
                flags[2],
                "--json",
            ],
        );
        assert!(initial.status.success());
        let initial_data = json(&initial)["data"].clone();
        let artifact = destination.join("./.env");
        match case {
            0 => {
                fs::write(&artifact, b"changed-value\n").unwrap();
            }
            1 => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut permissions = fs::metadata(&artifact).unwrap().permissions();
                    permissions.set_mode(0o600);
                    fs::set_permissions(&artifact, permissions).unwrap();
                }
            }
            2 => {
                fs::remove_file(&artifact).unwrap();
                std::os::unix::fs::symlink("elsewhere", &artifact).unwrap();
            }
            3 => {
                fs::remove_file(&artifact).unwrap();
                fs::create_dir(&artifact).unwrap();
            }
            _ => {
                let staging = initial_data["steps"][0]["action"]["descriptor"]["staging"]["path"]
                    .as_str()
                    .unwrap();
                fs::write(staging, b"unexpected").unwrap();
            }
        }
        let output = propose(
            &repo,
            &[
                "recover",
                "propose-compensation",
                &operation_id,
                "--repo",
                repo.root.to_str().unwrap(),
                flags[0],
                flags[1],
                flags[2],
                "--json",
            ],
        );
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(json(&output)["error"]["code"], "compensation_state_changed");
    }
}

fn assert_state_changed_after_drift<F>(mutate: F)
where
    F: FnOnce(&Repo, &Path),
{
    let repo = Repo::new();
    let destination = repo.temp.path().join("drift-worktree");
    let operation_id = create_and_apply(&repo, &destination);
    mutate(&repo, &destination);
    let root = repo.root.to_str().unwrap().to_owned();
    let output = propose(
        &repo,
        &[
            "recover",
            "propose-compensation",
            &operation_id,
            "--repo",
            &root,
            "--allow-worktree",
            "--allow-local-branch",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["error"]["code"], "compensation_state_changed");
}

#[test]
fn worktree_and_local_ref_drift_is_state_changed_and_read_only() {
    assert_state_changed_after_drift(|repo, worktree| {
        git(worktree, &["commit", "--allow-empty", "-m", "drift"]);
        let _ = repo;
    });
    assert_state_changed_after_drift(|repo, worktree| {
        git(
            &repo.root,
            &["worktree", "remove", "--force", worktree.to_str().unwrap()],
        );
    });
}

#[test]
fn primary_head_drift_is_repository_identity_mismatch_and_read_only() {
    let repo = Repo::new();
    let destination = repo.temp.path().join("identity-drift-worktree");
    let operation_id = create_and_apply(&repo, &destination);
    git(
        &repo.root,
        &["commit", "--allow-empty", "-m", "primary-drift"],
    );
    let output = propose(
        &repo,
        &[
            "recover",
            "propose-compensation",
            &operation_id,
            "--repo",
            repo.root.to_str().unwrap(),
            "--allow-worktree",
            "--allow-local-branch",
            "--json",
        ],
    );
    assert_eq!(
        json(&output)["error"]["code"],
        "repository_identity_mismatch"
    );
}

#[test]
fn applied_task_plan_is_unsupported_for_compensation() {
    let repo = Repo::new();
    fs::write(repo.root.join(".ewtm.toml"), "schema = 1\n[tasks.check]\nphase = \"post_create\"\nargv = [\"true\"]\nenabled = true\nrequired = true\n").unwrap();
    let destination = repo.temp.path().join("task-worktree");
    let outside = repo.temp.path().join("task-outside");
    fs::create_dir(&outside).unwrap();
    let plan = repo.run(
        &outside,
        &[
            "create",
            "--repo",
            repo.root.to_str().unwrap(),
            "--new",
            "task-branch",
            "--base",
            "HEAD",
            "--path",
            destination.to_str().unwrap(),
            "--task",
            "check",
            "--plan",
        ],
    );
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let path = repo.temp.path().join("task-plan.json");
    fs::write(&path, &plan.stdout).unwrap();
    let applied = repo.run(
        &outside,
        &[
            "apply",
            path.to_str().unwrap(),
            "--confirm-plan",
            &digest(&plan.stdout),
            "--json",
        ],
    );
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stdout)
    );
    let id = json(&applied)["data"]["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let output = propose(
        &repo,
        &[
            "recover",
            "propose-compensation",
            &id,
            "--repo",
            repo.root.to_str().unwrap(),
            "--allow-worktree",
            "--allow-local-branch",
            "--json",
        ],
    );
    assert_eq!(
        json(&output)["error"]["code"],
        "compensation_unsupported_forward_step"
    );
}

#[test]
fn corrupt_duplicate_and_unknown_journal_bytes_are_preserved() {
    for suffix in ["duplicate", "unknown"] {
        let repo = Repo::new();
        let destination = repo.temp.path().join(format!("corrupt-{suffix}"));
        let id = create_and_apply(&repo, &destination);
        let path = repo
            .root
            .join(".git/ewtm/journal")
            .join(format!("{id}.json"));
        let original = fs::read(&path).unwrap();
        let corrupt = if suffix == "duplicate" {
            let text = String::from_utf8(original.clone()).unwrap();
            text.replacen(
                "\"revision\": 2,",
                "\"revision\": 2,\n  \"revision\": 2,",
                1,
            )
            .into_bytes()
        } else {
            let mut value: Value = serde_json::from_slice(&original).unwrap();
            value["unknown_field"] = Value::Bool(true);
            serde_json::to_vec(&value).unwrap()
        };
        fs::write(&path, &corrupt).unwrap();
        let output = propose(
            &repo,
            &[
                "recover",
                "propose-compensation",
                &id,
                "--repo",
                repo.root.to_str().unwrap(),
                "--json",
            ],
        );
        assert_eq!(json(&output)["error"]["code"], "journal_corrupt");
        assert_eq!(fs::read(&path).unwrap(), corrupt);
    }
}

#[test]
fn repository_lock_contention_is_typed_and_read_only() {
    if std::env::var_os("EWTM_COMPENSATION_LOCK_CHILD").is_some() {
        use fs4::FileExt;
        let root = PathBuf::from(std::env::var_os("EWTM_COMPENSATION_LOCK_ROOT").unwrap());
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join(".git/ewtm/repository.lock"))
            .unwrap();
        FileExt::try_lock(&file).unwrap();
        fs::write(
            std::env::var_os("EWTM_COMPENSATION_LOCK_READY").unwrap(),
            b"ready",
        )
        .unwrap();
        let mut byte = [0u8; 1];
        std::io::Read::read(&mut std::io::stdin(), &mut byte).unwrap();
        return;
    }
    let repo = Repo::new();
    let destination = repo.temp.path().join("locked-worktree");
    let id = create_and_apply(&repo, &destination);
    let ready = repo.temp.path().join("lock-ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("repository_lock_contention_is_typed_and_read_only")
        .arg("--nocapture")
        .env("EWTM_COMPENSATION_LOCK_CHILD", "1")
        .env("EWTM_COMPENSATION_LOCK_ROOT", &repo.root)
        .env("EWTM_COMPENSATION_LOCK_READY", &ready)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if ready.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(ready.exists());
    let output = propose(
        &repo,
        &[
            "recover",
            "propose-compensation",
            &id,
            "--repo",
            repo.root.to_str().unwrap(),
            "--allow-worktree",
            "--allow-local-branch",
            "--json",
        ],
    );
    assert_eq!(json(&output)["error"]["code"], "repository_busy");
    child.stdin.take().unwrap().write_all(b"x").unwrap();
    assert!(child.wait().unwrap().success());
}
