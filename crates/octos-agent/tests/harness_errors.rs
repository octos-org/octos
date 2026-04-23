//! Acceptance tests for M6.1 harness error taxonomy (issue #488).
//!
//! These tests pin the public contract:
//! - `LlmError` classifies into `HarnessError` variants deterministically.
//! - `HarnessError` converts into `HarnessEventPayload::Error` with a primary
//!   `RecoveryHint` per variant.
//! - Emitted events conform to `octos.harness.event.v1` and round-trip through
//!   `HarnessEvent::from_json_line`.
//! - The schema version is registered via `abi_schema`.

use std::fs;
use std::path::PathBuf;

use octos_agent::abi_schema::{HARNESS_ERROR_SCHEMA_VERSION, check_supported};
use octos_agent::harness_errors::{HarnessError, RecoveryHint};
use octos_agent::harness_events::{
    HARNESS_EVENT_SCHEMA_V1, HarnessEvent, HarnessEventPayload, write_event_to_sink,
};
use octos_llm::{LlmError, LlmErrorKind};

// ─────────────────────────────────────────────────────────────────────────
// Classification
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn should_classify_rate_limit_from_llm_error_429() {
    let err = LlmError::from_status(429, "Too Many Requests");
    let classified = HarnessError::from(err);
    assert!(
        matches!(classified, HarnessError::RateLimited { .. }),
        "expected RateLimited, got {classified:?}"
    );
    assert_eq!(classified.recovery_hint(), RecoveryHint::BackoffRetry);
    assert_eq!(classified.variant_name(), "rate_limited");
}

#[test]
fn should_classify_context_overflow_from_llm_error_oversized() {
    let err = LlmError::from_status(400, "context_length_exceeded: 200k");
    let classified = HarnessError::from(err);
    assert!(
        matches!(classified, HarnessError::ContextOverflow { .. }),
        "expected ContextOverflow, got {classified:?}"
    );
    assert_eq!(classified.recovery_hint(), RecoveryHint::CompactContext);
    assert_eq!(classified.variant_name(), "context_overflow");
}

#[test]
fn should_classify_authentication_failure_as_fail_fast() {
    let err = LlmError::from_status(401, "Unauthorized");
    let classified = HarnessError::from(err);
    assert!(matches!(classified, HarnessError::Authentication { .. }));
    assert_eq!(classified.recovery_hint(), RecoveryHint::FailFast);
}

#[test]
fn should_classify_provider_server_error_as_switch_provider() {
    let err = LlmError::from_status(503, "Service Unavailable");
    let classified = HarnessError::from(err);
    assert!(matches!(
        classified,
        HarnessError::ProviderUnavailable { .. }
    ));
    assert_eq!(classified.recovery_hint(), RecoveryHint::SwitchProvider);
}

#[test]
fn should_classify_llm_timeout_as_backoff_retry() {
    let err = LlmError::timeout("request timed out after 120s");
    let classified = HarnessError::from(err);
    assert!(matches!(classified, HarnessError::Timeout { .. }));
    assert_eq!(classified.recovery_hint(), RecoveryHint::BackoffRetry);
}

#[test]
fn should_classify_network_as_backoff_retry() {
    let err = LlmError::network("connection reset");
    let classified = HarnessError::from(err);
    assert!(matches!(classified, HarnessError::Network { .. }));
    assert_eq!(classified.recovery_hint(), RecoveryHint::BackoffRetry);
}

#[test]
fn should_classify_content_filtered_as_fail_fast() {
    let err = LlmError::new(LlmErrorKind::ContentFiltered, "filtered by safety");
    let classified = HarnessError::from(err);
    assert!(matches!(classified, HarnessError::ContentFiltered { .. }));
    assert_eq!(classified.recovery_hint(), RecoveryHint::FailFast);
}

#[test]
fn should_classify_invalid_request_as_fail_fast() {
    let err = LlmError::new(
        LlmErrorKind::InvalidRequest {
            detail: "bad param temperature".into(),
        },
        "400",
    );
    let classified = HarnessError::from(err);
    assert!(matches!(classified, HarnessError::InvalidRequest { .. }));
    assert_eq!(classified.recovery_hint(), RecoveryHint::FailFast);
}

// ─────────────────────────────────────────────────────────────────────────
// Deterministic classification (contract invariant #2)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn should_classify_same_input_to_same_variant_deterministically() {
    // Run classification 100 times; same input must always map to the same
    // variant and hint — no HashMap iteration order, no randomness.
    for _ in 0..100 {
        let a = HarnessError::from(LlmError::from_status(429, "Too Many Requests"));
        let b = HarnessError::from(LlmError::from_status(429, "Too Many Requests"));
        assert_eq!(a.variant_name(), b.variant_name());
        assert_eq!(a.recovery_hint(), b.recovery_hint());
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Event emission
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn should_emit_harness_event_on_error_classification() {
    let classified = HarnessError::from(LlmError::from_status(429, "Too Many Requests"));
    let event = classified.to_event("session-xyz", "task-42", Some("coding"), Some("verify"));

    // Schema is octos.harness.event.v1 — the existing harness event schema.
    assert_eq!(event.schema, HARNESS_EVENT_SCHEMA_V1);

    // Payload serializes as kind: "error".
    let HarnessEventPayload::Error { data } = &event.payload else {
        panic!("expected Error payload, got {:?}", event.payload);
    };
    assert_eq!(data.session_id, "session-xyz");
    assert_eq!(data.task_id, "task-42");
    assert_eq!(data.workflow.as_deref(), Some("coding"));
    assert_eq!(data.phase.as_deref(), Some("verify"));
    assert_eq!(data.variant, "rate_limited");
    assert_eq!(data.recovery, "backoff_retry");

    // The event passes validation and renders as valid JSON.
    event.validate().expect("error event should validate");
    let rendered = serde_json::to_string(&event).expect("serialize");
    assert!(rendered.contains(r#""schema":"octos.harness.event.v1""#));
    assert!(rendered.contains(r#""kind":"error""#));
    assert!(rendered.contains(r#""variant":"rate_limited""#));
    assert!(rendered.contains(r#""recovery":"backoff_retry""#));
}

#[test]
fn should_round_trip_harness_error_through_sink() {
    let temp = tempfile::NamedTempFile::new().expect("create sink file");
    let sink_path: PathBuf = temp.path().to_path_buf();

    let classified = HarnessError::from(LlmError::from_status(400, "context_length_exceeded"));
    let event = classified.to_event(
        "session-ctx",
        "task-ctx",
        Some("deep_research"),
        Some("plan"),
    );

    write_event_to_sink(sink_path.display().to_string(), &event)
        .expect("write error event to sink");

    let raw = fs::read_to_string(&sink_path).expect("read sink");
    let line = raw.lines().next().expect("sink line present");
    let parsed = HarnessEvent::from_json_line(line).expect("parse harness event line");

    assert_eq!(parsed.schema, HARNESS_EVENT_SCHEMA_V1);
    let HarnessEventPayload::Error { data } = parsed.payload else {
        panic!("expected Error payload after round-trip");
    };
    assert_eq!(data.variant, "context_overflow");
    assert_eq!(data.recovery, "compact_context");
    assert_eq!(data.session_id, "session-ctx");
    assert_eq!(data.task_id, "task-ctx");
    assert_eq!(data.workflow.as_deref(), Some("deep_research"));
    assert_eq!(data.phase.as_deref(), Some("plan"));
    assert_eq!(data.schema_version, HARNESS_ERROR_SCHEMA_VERSION);
}

// ─────────────────────────────────────────────────────────────────────────
// Schema version registration (M4.6)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn should_register_error_schema_version_in_abi() {
    assert_eq!(HARNESS_ERROR_SCHEMA_VERSION, 1);
    assert!(check_supported("HarnessError", 1, HARNESS_ERROR_SCHEMA_VERSION).is_ok());
    assert!(check_supported("HarnessError", 2, HARNESS_ERROR_SCHEMA_VERSION).is_err());
}

// ─────────────────────────────────────────────────────────────────────────
// Metrics counter
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn should_count_errors_per_variant_in_metrics() {
    // The counter name is octos_loop_error_total and must be labeled with
    // {variant, recovery}. This test uses the label-emitting helper so the
    // actual Prometheus text rendering is validated in the CLI metrics tests.
    let variant = HarnessError::from(LlmError::from_status(429, "Too Many Requests"));
    let (label_variant, label_recovery) = variant.metric_labels();
    assert_eq!(label_variant, "rate_limited");
    assert_eq!(label_recovery, "backoff_retry");

    let overflow = HarnessError::from(LlmError::from_status(400, "context_length_exceeded"));
    let (overflow_variant, overflow_recovery) = overflow.metric_labels();
    assert_eq!(overflow_variant, "context_overflow");
    assert_eq!(overflow_recovery, "compact_context");

    // Variant names are stable identifiers (snake_case) suitable for
    // Prometheus labels — no spaces, no uppercase, no operator-supplied text.
    for name in [
        "rate_limited",
        "context_overflow",
        "authentication",
        "invalid_request",
        "content_filtered",
        "provider_unavailable",
        "network",
        "timeout",
        "tool_execution",
        "plugin_spawn",
        "plugin_timeout",
        "plugin_protocol",
        "delegate_depth_exceeded",
        "internal",
    ] {
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "metric label variant '{name}' must be snake_case ASCII"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────
// DelegateDepthExceeded reserved for M6.7
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn should_expose_delegate_depth_exceeded_variant_for_m6_7() {
    let err = HarnessError::DelegateDepthExceeded {
        depth: 5,
        limit: 4,
        message: "spawn chain exceeded max depth".into(),
    };
    assert_eq!(err.variant_name(), "delegate_depth_exceeded");
    // Hitting the max delegation depth is a structural budget — fail fast and
    // surface to the operator; do not auto-retry.
    assert_eq!(err.recovery_hint(), RecoveryHint::FailFast);
    let event = err.to_event("s", "t", None, None);
    event
        .validate()
        .expect("delegate depth event should validate");
}
