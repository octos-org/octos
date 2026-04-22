//! Harness ABI schema compatibility tests.
//!
//! Covers the four versioned harness types:
//! - `WorkspacePolicy`
//! - `HookPayload`
//! - `TaskResult`
//! - `ProgressEventEnvelope` (wire shape for `ProgressEvent`)
//!
//! These fixtures are the durable ABI promise for external app skills.
//! See `docs/OCTOS_HARNESS_ABI_VERSIONING.md` for the per-type stable and
//! experimental field list and the deprecation rules.

use std::path::{Path, PathBuf};

use octos_agent::abi_schema::{
    HOOK_PAYLOAD_SCHEMA_VERSION, PROGRESS_EVENT_SCHEMA_VERSION, WORKSPACE_POLICY_SCHEMA_VERSION,
    check_supported,
};
use octos_agent::hooks::{HookEvent, HookPayload};
use octos_agent::progress::{HARNESS_PROGRESS_EVENT_SCHEMA, ProgressEvent, ProgressEventEnvelope};
use octos_agent::workspace_policy::{
    WORKSPACE_POLICY_FILE, WorkspacePolicy, read_workspace_policy,
};
use octos_core::{TASK_RESULT_SCHEMA_VERSION, TaskResult};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn load_fixture(name: &str) -> String {
    let path = fixtures_dir().join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read fixture {}: {err}", path.display()))
}

fn copy_fixture_into_workspace(fixture: &str, project_root: &Path) {
    let contents = load_fixture(fixture);
    std::fs::write(project_root.join(WORKSPACE_POLICY_FILE), contents)
        .expect("write workspace policy fixture");
}

#[test]
fn should_load_workspace_policy_v1_slides_fixture() {
    let temp = tempfile::tempdir().unwrap();
    copy_fixture_into_workspace("workspace_policy_v1_slides.toml", temp.path());

    let policy = read_workspace_policy(temp.path())
        .expect("v1 slides fixture should parse")
        .expect("policy file should exist");

    assert_eq!(policy.schema_version, WORKSPACE_POLICY_SCHEMA_VERSION);
    assert_eq!(
        policy.workspace.kind,
        octos_agent::WorkspacePolicyKind::Slides
    );
    assert!(
        policy
            .validation
            .on_turn_end
            .iter()
            .any(|line| line == "file_exists:script.js"),
        "expected slides turn-end validation to include script.js",
    );
    assert_eq!(
        policy.artifacts.entries.get("primary").map(String::as_str),
        Some("output/deck.pptx")
    );
}

#[test]
fn should_load_workspace_policy_v1_session_fixture() {
    let temp = tempfile::tempdir().unwrap();
    copy_fixture_into_workspace("workspace_policy_v1_session.toml", temp.path());

    let policy = read_workspace_policy(temp.path())
        .expect("v1 session fixture should parse")
        .expect("policy file should exist");

    assert_eq!(policy.schema_version, WORKSPACE_POLICY_SCHEMA_VERSION);
    assert_eq!(
        policy.workspace.kind,
        octos_agent::WorkspacePolicyKind::Session
    );
    let tts = policy
        .spawn_tasks
        .get("fm_tts")
        .expect("fm_tts spawn task contract");
    assert_eq!(tts.artifact.as_deref(), Some("primary_audio"));
    assert!(
        tts.on_verify
            .iter()
            .any(|line| line == "file_size_min:$artifact:1024"),
        "expected fm_tts verify action for artifact size",
    );
}

#[test]
fn should_default_workspace_policy_to_v1_when_schema_version_missing() {
    let temp = tempfile::tempdir().unwrap();
    copy_fixture_into_workspace("workspace_policy_legacy_no_version.toml", temp.path());

    let policy = read_workspace_policy(temp.path())
        .expect("legacy policy should parse")
        .expect("policy file should exist");

    assert_eq!(
        policy.schema_version, WORKSPACE_POLICY_SCHEMA_VERSION,
        "pre-M4.6 policy files must load as v1",
    );
    assert_eq!(
        policy.workspace.kind,
        octos_agent::WorkspacePolicyKind::Sites
    );
}

#[test]
fn should_preserve_all_first_party_built_in_workspace_policies() {
    // Real policies produced by harness callers: slides, sites, session, and
    // the site-build-output variant. All four must round-trip TOML cleanly
    // and carry the current ABI version.
    let contracts: Vec<(&str, WorkspacePolicy)> = vec![
        (
            "slides",
            WorkspacePolicy::for_kind(octos_agent::WorkspaceProjectKind::Slides),
        ),
        (
            "sites",
            WorkspacePolicy::for_kind(octos_agent::WorkspaceProjectKind::Sites),
        ),
        ("session", WorkspacePolicy::for_session()),
        (
            "site-build-output",
            WorkspacePolicy::for_site_build_output("dist"),
        ),
    ];

    for (label, policy) in contracts {
        assert_eq!(
            policy.schema_version, WORKSPACE_POLICY_SCHEMA_VERSION,
            "{label} policy must carry current schema version"
        );

        let rendered = toml::to_string_pretty(&policy)
            .unwrap_or_else(|err| panic!("serialize {label} policy: {err}"));
        assert!(
            rendered.contains("schema_version = 1"),
            "{label} policy TOML should include schema_version line",
        );

        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(WORKSPACE_POLICY_FILE), &rendered).unwrap();
        let roundtrip = read_workspace_policy(temp.path())
            .unwrap_or_else(|err| panic!("reload {label} policy: {err}"))
            .expect("policy should be read back");
        assert_eq!(roundtrip, policy, "{label} policy should round-trip");
    }
}

#[test]
fn should_reject_future_workspace_policy_schema_version() {
    let temp = tempfile::tempdir().unwrap();
    let future = format!(
        r#"
schema_version = {}

[workspace]
kind = "slides"

[version_control]
provider = "git"
auto_init = true
trigger = "turn_end"
fail_on_error = true

[tracking]
ignore = []
"#,
        WORKSPACE_POLICY_SCHEMA_VERSION + 10
    );
    std::fs::write(temp.path().join(WORKSPACE_POLICY_FILE), future).unwrap();

    let err = read_workspace_policy(temp.path())
        .expect_err("future schema version must be rejected, not panic");
    let rendered = format!("{err:#}");
    assert!(rendered.contains("schema_version"));
    assert!(rendered.contains("upgrade octos"));
}

#[test]
fn should_load_hook_payload_v1_fixture() {
    let raw = load_fixture("hook_payload_v1_before_tool.json");
    let parsed: HookPayload = serde_json::from_str(&raw).expect("v1 hook payload parses");
    assert_eq!(parsed.schema_version, HOOK_PAYLOAD_SCHEMA_VERSION);
    assert_eq!(parsed.event, HookEvent::BeforeToolCall);
    assert_eq!(parsed.tool_name.as_deref(), Some("shell"));
    assert_eq!(parsed.session_id.as_deref(), Some("sess-1"));
}

#[test]
fn should_default_hook_payload_to_v1_when_schema_version_missing() {
    let raw = load_fixture("hook_payload_legacy_no_version.json");
    let parsed: HookPayload = serde_json::from_str(&raw).expect("legacy hook payload parses");
    assert_eq!(
        parsed.schema_version, HOOK_PAYLOAD_SCHEMA_VERSION,
        "pre-M4.6 hook payloads must deserialize as v1",
    );
    assert_eq!(parsed.event, HookEvent::AfterToolCall);
    assert_eq!(parsed.success, Some(true));
}

#[test]
fn should_reject_future_hook_payload_schema_version_via_check_supported() {
    // Consumers can inspect `schema_version` and apply the shared
    // [`check_supported`] guard before trusting the payload.
    let raw = serde_json::json!({
        "schema_version": HOOK_PAYLOAD_SCHEMA_VERSION + 5,
        "event": "after_tool_call",
        "tool_name": "shell",
        "tool_id": "tc-future",
        "success": true
    })
    .to_string();

    let parsed: HookPayload = serde_json::from_str(&raw).expect("payload parses");
    let err = check_supported(
        "HookPayload",
        parsed.schema_version,
        HOOK_PAYLOAD_SCHEMA_VERSION,
    )
    .expect_err("future version should be rejected");
    assert_eq!(err.kind, "HookPayload");
    assert_eq!(err.found, HOOK_PAYLOAD_SCHEMA_VERSION + 5);
}

#[test]
fn should_load_task_result_v1_fixture() {
    let raw = load_fixture("task_result_v1.json");
    let parsed: TaskResult = serde_json::from_str(&raw).expect("v1 task result parses");
    assert_eq!(parsed.schema_version, TASK_RESULT_SCHEMA_VERSION);
    assert!(parsed.success);
    assert_eq!(parsed.files_modified.len(), 1);
    assert_eq!(parsed.token_usage.input_tokens, 1234);
}

#[test]
fn should_default_task_result_to_v1_when_schema_version_missing() {
    let raw = load_fixture("task_result_legacy_no_version.json");
    let parsed: TaskResult = serde_json::from_str(&raw).expect("legacy task result parses");
    assert_eq!(
        parsed.schema_version, TASK_RESULT_SCHEMA_VERSION,
        "pre-M4.6 task results must deserialize as v1",
    );
    assert!(!parsed.success);
    assert!(parsed.output.contains("Token budget"));
}

#[test]
fn should_load_progress_event_envelope_v1_fixture() {
    let raw = load_fixture("progress_envelope_v1.json");
    let parsed: ProgressEventEnvelope =
        serde_json::from_str(&raw).expect("v1 progress envelope parses");
    assert_eq!(parsed.schema, HARNESS_PROGRESS_EVENT_SCHEMA);
    assert_eq!(parsed.schema_version, PROGRESS_EVENT_SCHEMA_VERSION);
    match parsed.event {
        ProgressEvent::Thinking { iteration } => assert_eq!(iteration, 3),
        other => panic!("unexpected event variant: {other:?}"),
    }
}

#[test]
fn should_default_progress_envelope_schema_when_fields_missing() {
    let raw = load_fixture("progress_envelope_legacy_no_version.json");
    let parsed: ProgressEventEnvelope =
        serde_json::from_str(&raw).expect("legacy progress envelope parses");
    assert_eq!(
        parsed.schema, HARNESS_PROGRESS_EVENT_SCHEMA,
        "missing schema name must default to the canonical identifier",
    );
    assert_eq!(
        parsed.schema_version, PROGRESS_EVENT_SCHEMA_VERSION,
        "missing schema_version must default to v1",
    );
    match parsed.event {
        ProgressEvent::TaskStarted { task_id } => assert_eq!(task_id, "task-legacy"),
        other => panic!("unexpected event variant: {other:?}"),
    }
}

#[test]
fn should_wrap_progress_event_in_current_envelope() {
    let envelope = ProgressEventEnvelope::wrap(ProgressEvent::Thinking { iteration: 1 });
    assert_eq!(envelope.schema, HARNESS_PROGRESS_EVENT_SCHEMA);
    assert_eq!(envelope.schema_version, PROGRESS_EVENT_SCHEMA_VERSION);
    let json = serde_json::to_string(&envelope).expect("serialize envelope");
    assert!(json.contains("\"schema\":\"octos.agent.progress.event.v1\""));
    assert!(json.contains("\"schema_version\":1"));
}
