use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

fn run_skill(octos_home: &Path, input: Value) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_account_manager"))
        .env("OCTOS_HOME", octos_home)
        .env("OCTOS_PROFILE_ID", "parent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn account-manager");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.to_string().as_bytes())
        .expect("write input");
    child.wait_with_output().expect("wait for account-manager")
}

fn assert_sandbox_config_error(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected a structured configuration error, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).expect("stdout JSON");
    assert_eq!(response["success"], false);
    let message = response["output"].as_str().expect("error output");
    assert!(message.contains("sandbox"), "unexpected error: {message}");
    assert!(message.contains("object"), "unexpected error: {message}");
}

#[test]
fn should_return_config_error_when_create_inherits_non_object_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let profiles = dir.path().join("profiles");
    std::fs::create_dir(&profiles).unwrap();
    std::fs::write(
        profiles.join("parent.json"),
        br#"{"config":{"sandbox":17}}"#,
    )
    .unwrap();

    let output = run_skill(
        dir.path(),
        json!({
            "action": "create",
            "sub_account_id": "child",
            "public_subdomain": "child.example",
            "name": "Child",
            "sandbox": true
        }),
    );

    assert_sandbox_config_error(&output);
    assert!(!profiles.join("parent--child.json").exists());
}

#[test]
fn should_return_config_error_and_preserve_profile_when_update_has_non_object_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let profiles = dir.path().join("profiles");
    std::fs::create_dir(&profiles).unwrap();
    let profile_path = profiles.join("parent--child.json");
    let original = br#"{
  "id": "parent--child",
  "name": "Child",
  "parent_id": "parent",
  "config": {"sandbox": "legacy"},
  "created_at": "2026-01-01T00:00:00Z",
  "updated_at": "2026-01-01T00:00:00Z"
}"#;
    std::fs::write(&profile_path, original).unwrap();

    let output = run_skill(
        dir.path(),
        json!({
            "action": "update",
            "sub_account_id": "parent--child",
            "sandbox_mode": "bwrap"
        }),
    );

    assert_sandbox_config_error(&output);
    assert_eq!(std::fs::read(&profile_path).unwrap(), original);
}

#[test]
fn should_update_existing_object_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let profiles = dir.path().join("profiles");
    std::fs::create_dir(&profiles).unwrap();
    let profile_path = profiles.join("parent--child.json");
    std::fs::write(
        &profile_path,
        r#"{
  "id": "parent--child",
  "name": "Child",
  "parent_id": "parent",
  "config": {
    "sandbox": {
      "enabled": false,
      "mode": "auto",
      "allow_network": true,
      "custom": "keep"
    }
  },
  "created_at": "2026-01-01T00:00:00Z",
  "updated_at": "2026-01-01T00:00:00Z"
}"#,
    )
    .unwrap();

    let output = run_skill(
        dir.path(),
        json!({
            "action": "update",
            "sub_account_id": "parent--child",
            "sandbox": true,
            "sandbox_mode": "bwrap",
            "sandbox_network": false
        }),
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).expect("stdout JSON");
    assert_eq!(response["success"], true);
    let profile: Value = serde_json::from_slice(&std::fs::read(&profile_path).unwrap()).unwrap();
    assert_eq!(profile["config"]["sandbox"]["enabled"], true);
    assert_eq!(profile["config"]["sandbox"]["mode"], "bwrap");
    assert_eq!(profile["config"]["sandbox"]["allow_network"], false);
    assert_eq!(profile["config"]["sandbox"]["custom"], "keep");
}
