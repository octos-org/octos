//! Integration tests for the domain-hook pattern (RP03).
//!
//! These tests exercise the public `HookPayloadEnricher` extension point and
//! prove the end-to-end integration with a real shell-script hook. They are
//! deliberately test-heavy because the value of RP03 is the shell pattern:
//! integrators attach robot sensor data in Rust, then filter in POSIX shell.
//!
//! Invariants covered:
//! - `HookPayload.domain_data` serializes only when `Some`.
//! - Enriched domain_data whose JSON exceeds `MAX_PAYLOAD_FIELD_BYTES` (1024)
//!   becomes a `{"truncated": true}` marker object.
//! - A before-hook can read `domain_data.force_n` from stdin JSON and exit 1
//!   to deny a `BeforeSpawnVerify` event.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use octos_agent::{
    HookConfig, HookEvent, HookExecutor, HookPayload, HookPayloadEnricher, HookResult,
};

/// Test enricher that attaches a fixed `force_n` reading.
struct StaticForceEnricher {
    force_n: f64,
}

impl HookPayloadEnricher for StaticForceEnricher {
    fn enrich(&self, _event: &HookEvent, payload: &mut HookPayload) {
        payload.domain_data = Some(serde_json::json!({
            "force_n": self.force_n,
            "source": "static-test-enricher",
        }));
    }
}

#[tokio::test]
async fn should_include_domain_data_when_enricher_registered() {
    // A deny-hook that writes the stdin JSON to a side channel file and
    // exits 0 so we can assert that domain_data was serialized in the payload.
    let dir = tempfile::tempdir().unwrap();
    let captured = dir.path().join("captured.json");
    let script_path = dir.path().join("capture.sh");
    let script = format!(
        "#!/bin/sh\ncat > {captured_quoted}\nexit 0\n",
        captured_quoted = shell_quote(captured.to_str().unwrap())
    );
    write_exec(&script_path, &script);

    let executor = HookExecutor::new(vec![HookConfig {
        event: HookEvent::BeforeSpawnVerify,
        command: vec![script_path.to_string_lossy().to_string()],
        timeout_ms: 5000,
        tool_filter: vec![],
    }])
    .with_enricher(Arc::new(StaticForceEnricher { force_n: 12.5 }));

    let payload = HookPayload::before_spawn_verify(
        "task-1",
        "move-arm",
        "parent",
        "child",
        Some("robot"),
        Some("verify"),
        Some("candidate"),
        vec![],
        None,
    );
    let result = executor.run(HookEvent::BeforeSpawnVerify, &payload).await;
    assert!(matches!(result, HookResult::Allow));

    let captured_json = std::fs::read_to_string(&captured).unwrap();
    let captured: serde_json::Value = serde_json::from_str(&captured_json).unwrap();
    let domain_data = captured
        .get("domain_data")
        .expect("domain_data field present");
    assert_eq!(
        domain_data.get("force_n").and_then(|v| v.as_f64()),
        Some(12.5)
    );
    assert_eq!(
        domain_data.get("source").and_then(|v| v.as_str()),
        Some("static-test-enricher")
    );
}

#[tokio::test]
async fn should_omit_domain_data_when_no_enricher() {
    // Without any enricher registered, the payload field must be absent
    // (serde skip_serializing_if = Option::is_none). Prior hook tests must
    // still pass and must not observe new payload shape.
    let dir = tempfile::tempdir().unwrap();
    let captured = dir.path().join("captured.json");
    let script_path = dir.path().join("capture.sh");
    let script = format!(
        "#!/bin/sh\ncat > {captured_quoted}\nexit 0\n",
        captured_quoted = shell_quote(captured.to_str().unwrap())
    );
    write_exec(&script_path, &script);

    let executor = HookExecutor::new(vec![HookConfig {
        event: HookEvent::BeforeSpawnVerify,
        command: vec![script_path.to_string_lossy().to_string()],
        timeout_ms: 5000,
        tool_filter: vec![],
    }]);

    let payload = HookPayload::before_spawn_verify(
        "task-1",
        "move-arm",
        "parent",
        "child",
        Some("robot"),
        Some("verify"),
        Some("candidate"),
        vec![],
        None,
    );
    let result = executor.run(HookEvent::BeforeSpawnVerify, &payload).await;
    assert!(matches!(result, HookResult::Allow));

    let captured_json = std::fs::read_to_string(&captured).unwrap();
    // No enricher -> no domain_data key at all
    assert!(
        !captured_json.contains("\"domain_data\""),
        "payload should not contain domain_data without enricher: {captured_json}"
    );
}

/// Enricher that attaches a giant blob larger than MAX_PAYLOAD_FIELD_BYTES.
struct OversizeEnricher;

impl HookPayloadEnricher for OversizeEnricher {
    fn enrich(&self, _event: &HookEvent, payload: &mut HookPayload) {
        // 4 KiB of ASCII exceeds the 1024 byte field cap.
        let blob: String = "A".repeat(4096);
        payload.domain_data = Some(serde_json::json!({
            "blob": blob,
        }));
    }
}

#[tokio::test]
async fn should_truncate_domain_data_at_max_payload_field_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let captured = dir.path().join("captured.json");
    let script_path = dir.path().join("capture.sh");
    let script = format!(
        "#!/bin/sh\ncat > {captured_quoted}\nexit 0\n",
        captured_quoted = shell_quote(captured.to_str().unwrap())
    );
    write_exec(&script_path, &script);

    let executor = HookExecutor::new(vec![HookConfig {
        event: HookEvent::BeforeSpawnVerify,
        command: vec![script_path.to_string_lossy().to_string()],
        timeout_ms: 5000,
        tool_filter: vec![],
    }])
    .with_enricher(Arc::new(OversizeEnricher));

    let payload = HookPayload::before_spawn_verify(
        "task-1",
        "move-arm",
        "parent",
        "child",
        Some("robot"),
        Some("verify"),
        Some("candidate"),
        vec![],
        None,
    );
    let _ = executor.run(HookEvent::BeforeSpawnVerify, &payload).await;

    let captured_json = std::fs::read_to_string(&captured).unwrap();
    let captured: serde_json::Value = serde_json::from_str(&captured_json).unwrap();
    let domain_data = captured
        .get("domain_data")
        .expect("domain_data still present after truncation");
    assert_eq!(
        domain_data.get("truncated").and_then(|v| v.as_bool()),
        Some(true),
        "oversize domain_data must be replaced with a {{\"truncated\": true}} marker, got: {domain_data}"
    );
    // Ensure the original giant blob is gone.
    assert!(
        !captured_json.contains("AAAAAAAAAAAA"),
        "truncated payload must not leak the oversize blob"
    );
    // Whole serialized payload stays small.
    assert!(
        captured_json.len() < 2048,
        "truncated payload length must be bounded, got {}",
        captured_json.len()
    );
}

/// Enricher that attaches a dynamically computed force reading.
struct DynamicForceEnricher {
    reading: std::sync::Mutex<f64>,
}

impl HookPayloadEnricher for DynamicForceEnricher {
    fn enrich(&self, _event: &HookEvent, payload: &mut HookPayload) {
        let n = *self.reading.lock().unwrap();
        payload.domain_data = Some(serde_json::json!({
            "force_n": n,
            "limit_n": 40.0,
        }));
    }
}

#[tokio::test]
async fn should_deny_before_spawn_verify_when_domain_data_violates() {
    // Real shell before-hook that parses domain_data.force_n and exits 1 when
    // it exceeds 40 N. No jq dependency - use sed-based field extraction.
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("force_guard.sh");
    let script = r#"#!/bin/sh
# Read entire stdin and extract domain_data.force_n by pattern matching.
payload="$(cat)"
# Extract the numeric value after `"force_n":` using sed.
force=$(printf '%s' "$payload" \
    | sed -n 's/.*"force_n":[[:space:]]*\([0-9][0-9]*\(\.[0-9]*\)*\).*/\1/p' \
    | head -n1)
if [ -z "$force" ]; then
    echo "missing force_n" >&2
    exit 0
fi
# POSIX shell has no float arithmetic; use awk to compare.
violates=$(awk -v f="$force" 'BEGIN { print (f > 40) ? 1 : 0 }')
if [ "$violates" = "1" ]; then
    printf "force %s N exceeds 40 N limit" "$force"
    exit 1
fi
exit 0
"#;
    write_exec(&script_path, script);

    let executor = HookExecutor::new(vec![HookConfig {
        event: HookEvent::BeforeSpawnVerify,
        command: vec![script_path.to_string_lossy().to_string()],
        timeout_ms: 5000,
        tool_filter: vec![],
    }])
    .with_enricher(Arc::new(DynamicForceEnricher {
        reading: std::sync::Mutex::new(55.2),
    }));

    let payload = HookPayload::before_spawn_verify(
        "task-1",
        "move-arm",
        "parent",
        "child",
        Some("robot"),
        Some("verify"),
        Some("candidate"),
        vec![],
        None,
    );
    let result = executor.run(HookEvent::BeforeSpawnVerify, &payload).await;
    match result {
        HookResult::Deny(reason) => {
            assert!(
                reason.contains("exceeds 40 N"),
                "expected deny reason to mention force limit, got: {reason}"
            );
        }
        other => panic!("expected HookResult::Deny, got {other:?}"),
    }
}

#[tokio::test]
async fn should_allow_before_spawn_verify_when_domain_data_within_limits() {
    // Same script, but force value is safe -> hook exits 0 -> Allow.
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("force_guard.sh");
    let script = r#"#!/bin/sh
payload="$(cat)"
force=$(printf '%s' "$payload" \
    | sed -n 's/.*"force_n":[[:space:]]*\([0-9][0-9]*\(\.[0-9]*\)*\).*/\1/p' \
    | head -n1)
if [ -z "$force" ]; then
    exit 0
fi
violates=$(awk -v f="$force" 'BEGIN { print (f > 40) ? 1 : 0 }')
if [ "$violates" = "1" ]; then
    printf "force %s N exceeds 40 N limit" "$force"
    exit 1
fi
exit 0
"#;
    write_exec(&script_path, script);

    let executor = HookExecutor::new(vec![HookConfig {
        event: HookEvent::BeforeSpawnVerify,
        command: vec![script_path.to_string_lossy().to_string()],
        timeout_ms: 5000,
        tool_filter: vec![],
    }])
    .with_enricher(Arc::new(DynamicForceEnricher {
        reading: std::sync::Mutex::new(12.0),
    }));

    let payload = HookPayload::before_spawn_verify(
        "task-1",
        "move-arm",
        "parent",
        "child",
        Some("robot"),
        Some("verify"),
        Some("candidate"),
        vec![],
        None,
    );
    let result = executor.run(HookEvent::BeforeSpawnVerify, &payload).await;
    assert!(
        matches!(result, HookResult::Allow),
        "safe force reading must be allowed, got {result:?}"
    );
}

#[tokio::test]
async fn robot_domain_hook_example_runs_end_to_end() {
    // Acceptance test for the `robot_domain_hook` example. We don't execute
    // the example binary (that would require `cargo run --example`); instead,
    // we replicate its end-to-end shape in-process:
    //   enricher attaches force reading -> before-hook denies motion.
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("robot_guard.sh");
    let script = r#"#!/bin/sh
payload="$(cat)"
force=$(printf '%s' "$payload" \
    | sed -n 's/.*"force_n":[[:space:]]*\([0-9][0-9]*\(\.[0-9]*\)*\).*/\1/p' \
    | head -n1)
estop=$(printf '%s' "$payload" \
    | sed -n 's/.*"estop":[[:space:]]*\(true\|false\).*/\1/p' \
    | head -n1)
if [ "$estop" = "true" ]; then
    printf "e-stop engaged"
    exit 1
fi
if [ -n "$force" ]; then
    violates=$(awk -v f="$force" 'BEGIN { print (f > 40) ? 1 : 0 }')
    if [ "$violates" = "1" ]; then
        printf "force %s N exceeds 40 N limit" "$force"
        exit 1
    fi
fi
exit 0
"#;
    write_exec(&script_path, script);

    struct RobotEnricher;
    impl HookPayloadEnricher for RobotEnricher {
        fn enrich(&self, _event: &HookEvent, payload: &mut HookPayload) {
            payload.domain_data = Some(serde_json::json!({
                "force_n": 62.3,
                "estop": false,
                "workspace_in_bounds": true,
            }));
        }
    }

    let executor = HookExecutor::new(vec![HookConfig {
        event: HookEvent::BeforeSpawnVerify,
        command: vec![script_path.to_string_lossy().to_string()],
        timeout_ms: 5000,
        tool_filter: vec![],
    }])
    .with_enricher(Arc::new(RobotEnricher));

    let payload = HookPayload::before_spawn_verify(
        "task-demo",
        "move-end-effector",
        "parent-sess",
        "child-sess",
        Some("robot"),
        Some("pre_motion"),
        Some("motion candidate ready"),
        vec![],
        None,
    );
    let result = executor.run(HookEvent::BeforeSpawnVerify, &payload).await;
    match result {
        HookResult::Deny(reason) => {
            assert!(
                reason.contains("exceeds 40 N"),
                "expected deny due to force, got: {reason}"
            );
        }
        other => panic!("robot_domain_hook_example_runs_end_to_end must deny, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn write_exec(path: &std::path::Path, contents: &str) {
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

/// Minimal single-quote escaping for a shell path. Paths under `tempfile::tempdir`
/// never contain single quotes on supported platforms, but we stay defensive.
fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}
