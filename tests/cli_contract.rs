use std::process::Command;

use serde_json::Value;

fn run(args: &[&str]) -> (Option<i32>, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_ewtm"))
        .args(args)
        .output()
        .expect("binary should execute");
    let value: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    (output.status.code(), value)
}

fn assert_envelope(value: &Value, command: &str) {
    let object = value.as_object().expect("envelope should be an object");
    let keys: std::collections::BTreeSet<String> = object.keys().cloned().collect();
    let expected: std::collections::BTreeSet<String> = [
        "command",
        "data",
        "error",
        "ok",
        "schema_version",
        "warnings",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(keys, expected);
    assert_eq!(object["schema_version"], 1);
    assert_eq!(object["command"], command);
    assert!(object["ok"].is_boolean());
    assert!(object["warnings"].is_array());
    assert!(object["error"].is_null() || object["error"].is_object());
}

#[test]
fn list_json_has_stable_success_envelope() {
    let (exit_code, value) = run(&["list", "--json"]);
    assert_eq!(exit_code, Some(0));
    assert_envelope(&value, "list");
    assert!(value["ok"].as_bool().unwrap_or(false));
}

#[test]
fn missing_import_json_has_stable_failure_envelope() {
    let (exit_code, value) = run(&[
        "config",
        "import",
        "--json",
        "--file",
        "/definitely/missing/ewtm-worktreerc",
    ]);
    assert_eq!(exit_code, Some(1));
    assert_envelope(&value, "config_import");
    assert!(!value["ok"].as_bool().unwrap_or(true));
    assert_eq!(value["error"]["code"], "import_io");
}
