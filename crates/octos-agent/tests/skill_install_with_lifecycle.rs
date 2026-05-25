//! Integration tests for hardware_lifecycle + tool_discovery wiring.
//!
//! Acceptance bullets:
//! A) preflight → init → ready_check run in order (critical failure aborts)
//! B) OCTOS_SKILL_DIR env available to lifecycle steps
//! C) tool_discovery=Http registers HttpTools via GET /tools
//! D) tool_discovery=Static preserves binary-protocol path (backward compat)
//! E) uninstall (deactivate) runs shutdown phase
//! F) HTTP discovery failure aborts install — no tools partially registered

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use octos_agent::plugins::{activate_skill, run_shutdown_phase};
use octos_agent::tools::ToolRegistry;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ─── helpers ────────────────────────────────────────────────────────────────

/// Write a minimal manifest.json to `dir`.
fn write_manifest(dir: &Path, manifest: serde_json::Value) {
    std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
}

/// Write an executable shell script to `dir/<name>`.
fn write_script(dir: &Path, name: &str, content: &str) {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{content}")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// ─── A + B: lifecycle phases run in order; OCTOS_SKILL_DIR is injected ──────

/// Each lifecycle phase (preflight → init → ready_check) runs in order and
/// the `OCTOS_SKILL_DIR` environment variable is available to each step.
///
/// The manifest has no tools (empty array) so PluginLoader's binary search
/// is skipped — this isolates the lifecycle logic.
#[tokio::test]
async fn install_runs_preflight_init_ready_check_in_order_with_skill_dir_env() {
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path().join("hw-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();

    // Each phase touches a sentinel file. We use append so we can verify order.
    let log = skill_dir.join("phases.log");
    let log_str = log.to_string_lossy().to_string();
    // Also capture OCTOS_SKILL_DIR via the init step.
    let env_file = skill_dir.join("skill_dir.txt");
    let env_str = env_file.to_string_lossy().to_string();

    let manifest = json!({
        "name": "hw-skill",
        "version": "0.1.0",
        "tools": [],
        "hardware_lifecycle": {
            "preflight": [
                {"label": "pre", "command": format!("echo preflight >> '{log_str}'")}
            ],
            "init": [
                {"label": "init-env", "command": format!("echo init >> '{log_str}'; printf '%s' \"$OCTOS_SKILL_DIR\" > '{env_str}'")}
            ],
            "ready_check": [
                {"label": "ready", "command": format!("echo ready_check >> '{log_str}'")}
            ]
        }
    });
    write_manifest(&skill_dir, manifest);

    let mut registry = ToolRegistry::new();
    let result = activate_skill(&mut registry, &skill_dir, &[]).await;
    assert!(result.is_ok(), "activate_skill failed: {:?}", result.err());

    // Verify phases ran in order.
    let log_content = std::fs::read_to_string(&log).expect("phases.log should exist");
    let phases: Vec<&str> = log_content.trim().lines().collect();
    assert_eq!(
        phases,
        ["preflight", "init", "ready_check"],
        "wrong order: {log_content}"
    );

    // Verify OCTOS_SKILL_DIR was injected.
    let captured_dir = std::fs::read_to_string(&env_file).expect("skill_dir.txt should exist");
    assert_eq!(
        captured_dir,
        skill_dir.to_string_lossy().as_ref(),
        "OCTOS_SKILL_DIR mismatch"
    );

    // No tools registered (manifest has empty tools array).
    assert_eq!(registry.len(), 0);
}

// ─── A reinforcement: critical preflight failure aborts install ──────────────

#[tokio::test]
async fn install_aborts_when_critical_preflight_step_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path().join("fail-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();

    let manifest = json!({
        "name": "fail-skill",
        "version": "0.1.0",
        "tools": [],
        "hardware_lifecycle": {
            "preflight": [
                {"label": "critical-fail", "command": "exit 1", "critical": true}
            ],
            "init": [
                {"label": "never-run", "command": "echo should_not_run"}
            ]
        }
    });
    write_manifest(&skill_dir, manifest);

    let mut registry = ToolRegistry::new();
    let result = activate_skill(&mut registry, &skill_dir, &[]).await;
    assert!(
        result.is_err(),
        "should have failed but got: {:?}",
        result.ok()
    );

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("preflight"),
        "error should mention 'preflight', got: {err_msg}"
    );

    // No tools should be registered.
    assert_eq!(registry.len(), 0);
}

// ─── C: HTTP tool discovery ──────────────────────────────────────────────────

/// Skills with `tool_discovery=Http` register discovered tools via GET /tools,
/// and a round-trip call (POST /tools/<name>) succeeds.
#[tokio::test]
async fn install_with_http_discovery_registers_http_tools() {
    let server = MockServer::start().await;

    // Mock the discovery endpoint.
    Mock::given(method("GET"))
        .and(path("/tools"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "name": "robot.ping",
                "description": "Ping the robot",
                "input_schema": {"type": "object"}
            }
        ])))
        .mount(&server)
        .await;

    // Mock the execution endpoint (called after registration, from the test).
    Mock::given(method("POST"))
        .and(path("/tools/robot.ping"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "code": "0",
            "msg": "",
            "data": {"pong": true}
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path().join("http-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();

    let base_url = server.uri();
    let manifest = json!({
        "name": "http-skill",
        "version": "0.1.0",
        "tool_discovery": {"type": "http", "base_url": base_url}
    });
    write_manifest(&skill_dir, manifest);

    let mut registry = ToolRegistry::new();
    let result = activate_skill(&mut registry, &skill_dir, &[]).await;
    assert!(result.is_ok(), "activate_skill failed: {:?}", result.err());

    let activated = result.unwrap();
    assert_eq!(activated.tool_names, vec!["robot.ping"]);
    assert!(
        registry.get("robot.ping").is_some(),
        "tool should be registered"
    );

    // Execute the tool via the registry to verify round-trip.
    let tool = registry.get("robot.ping").unwrap();
    let exec_result = tool.execute(&json!({})).await.unwrap();
    assert!(
        exec_result.success,
        "execute failed: {}",
        exec_result.output
    );
    assert!(
        exec_result.output.contains("pong"),
        "unexpected output: {}",
        exec_result.output
    );
}

// ─── D: backward compat – Static tool discovery unchanged ───────────────────

/// Skills without `hardware_lifecycle` or `tool_discovery` continue to work
/// exactly as before via the binary-protocol path.
#[cfg(unix)]
#[tokio::test]
async fn install_with_static_tool_discovery_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path().join("static-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();

    // Simple manifest — no lifecycle, no tool_discovery (defaults to Static).
    let manifest = json!({
        "name": "static-skill",
        "version": "0.1.0",
        "tools": [
            {"name": "greet", "description": "Say hello"}
        ]
    });
    write_manifest(&skill_dir, manifest);

    // Provide an executable that the PluginLoader will find.
    write_script(
        &skill_dir,
        "static-skill",
        r#"echo '{"output": "hello!", "success": true}'"#,
    );

    let mut registry = ToolRegistry::new();
    let result = activate_skill(&mut registry, &skill_dir, &[]).await;
    assert!(result.is_ok(), "activate_skill failed: {:?}", result.err());

    // Tool should be registered via the existing binary-protocol path.
    assert!(
        registry.get("greet").is_some(),
        "tool 'greet' should be registered"
    );
    assert_eq!(registry.len(), 1);
}

// ─── E: uninstall runs shutdown phase ────────────────────────────────────────

/// `run_shutdown_phase` executes the shutdown lifecycle steps before removal.
#[tokio::test]
async fn uninstall_runs_shutdown_phase() {
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path().join("shutdown-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();

    // Shutdown step touches a sentinel file.
    let sentinel = skill_dir.join("shutdown_ran.txt");
    let sentinel_str = sentinel.to_string_lossy().to_string();

    let manifest = json!({
        "name": "shutdown-skill",
        "version": "0.1.0",
        "tools": [],
        "hardware_lifecycle": {
            "shutdown": [
                {"label": "write-sentinel", "command": format!("touch '{sentinel_str}'")}
            ]
        }
    });
    write_manifest(&skill_dir, manifest);

    // Run shutdown phase (as part of uninstall / deactivate).
    run_shutdown_phase(&skill_dir).await;

    assert!(
        sentinel.exists(),
        "shutdown sentinel file should exist after run_shutdown_phase"
    );
}

// ─── F: HTTP discovery failure aborts install ─────────────────────────────────

/// If `GET /tools` returns 5xx, `activate_skill` must fail and register no tools.
#[tokio::test]
async fn install_aborts_when_http_discovery_fails() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/tools"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal server error"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path().join("failing-http-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();

    let base_url = server.uri();
    let manifest = json!({
        "name": "failing-http-skill",
        "version": "0.1.0",
        "tool_discovery": {"type": "http", "base_url": base_url}
    });
    write_manifest(&skill_dir, manifest);

    let mut registry = ToolRegistry::new();
    let result = activate_skill(&mut registry, &skill_dir, &[]).await;
    assert!(result.is_err(), "should have failed but got ok");

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("500") || err_msg.contains("discovery"),
        "error should mention failure, got: {err_msg}"
    );

    // No tools should be partially registered.
    assert_eq!(registry.len(), 0);
}
