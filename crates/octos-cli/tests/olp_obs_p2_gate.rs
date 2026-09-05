//! #48d — feature-carrying gate tests (contract v3.1).
//!
//! The api-module scenarios live behind `--features api`, but agent-spec
//! selectors cannot carry cargo features. These gates bind the feature
//! into the selector: each runs the ONE target test by exact name in a
//! subprocess and asserts success with `1 passed`. They duplicate no
//! tested logic — they only carry the feature flag.
//!
//! cwd is the workspace root (cargo sets it for integration tests run
//! via `cargo test -p octos-cli --test olp_obs_p2_gate`); `CARGO` and
//! `CARGO_TARGET_DIR` are inherited from the environment so CI cache
//! and toolchain overrides keep working.

use std::process::Command;

fn run_gated_test(target: &str) {
    // `--exact` matches the FULL test path (module::fn), so every target is
    // prefixed with its api-module location.
    let full = format!("api::ui_protocol_transport::tests::{target}");
    let target = full.as_str();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(&cargo)
        .args([
            "test",
            "-p",
            "octos-cli",
            "--features",
            "api",
            "--lib",
            target,
            "--",
            "--exact",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("CARGO") // subprocess must invoke cargo itself
        // #48d-r: inherit CARGO_TARGET_DIR when set (shares the outer
        // lock-holding build dir instead of contending on a second one);
        // when unset, cargo falls back to ./target — either way nested
        // invocations queue on the SAME lock rather than deadlocking.
        .output()
        .unwrap_or_else(|e| panic!("spawn {cargo} for {target}: {e}"));
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "gated test {target} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("1 passed") || stderr.contains("1 passed"),
        "gated test {target} must report exactly '1 passed', got:\n{stdout}{stderr}"
    );
}

#[test]
fn olp_obs_p2_gate_obs_fallback_switch_ui_forwarder_writes_own_session() {
    run_gated_test(
        "obs_fallback_switch_ui_48b::obs_fallback_switch_ui_forwarder_writes_own_session",
    );
}

#[test]
fn olp_obs_p2_gate_obs_fallback_switch_ui_forwarder_ignores_other_session() {
    run_gated_test(
        "obs_fallback_switch_ui_48b::obs_fallback_switch_ui_forwarder_ignores_other_session",
    );
}

#[test]
fn olp_obs_p2_gate_obs_fallback_switch_ui_forwarder_ignores_none_originator() {
    run_gated_test(
        "obs_fallback_switch_ui_48b::obs_fallback_switch_ui_forwarder_ignores_none_originator",
    );
}

#[test]
fn olp_obs_p2_gate_obs_malformed_exhausted_event_on_errored_terminal() {
    run_gated_test(
        "obs_malformed_exhausted_48b::obs_malformed_exhausted_event_on_errored_terminal",
    );
}

#[test]
fn olp_obs_p2_gate_obs_malformed_exhausted_terminal_test_uses_real_agent_error() {
    run_gated_test(
        "obs_malformed_exhausted_48b::obs_malformed_exhausted_terminal_test_uses_real_agent_error",
    );
}

#[test]
fn olp_obs_p2_gate_obs_no_malformed_exhausted_when_marker_not_prefix() {
    run_gated_test(
        "obs_malformed_exhausted_48b::obs_no_malformed_exhausted_when_marker_not_prefix",
    );
}

#[test]
fn olp_obs_p2_gate_obs_events_doc_lists_new_kinds() {
    run_gated_test("obs_events_doc_lists_new_kinds");
}
