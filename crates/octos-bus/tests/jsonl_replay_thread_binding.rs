//! Replay harness for thread_id binding correctness on JSONL session fixtures.
//!
//! The invariant under test:
//!   `assistant.thread_id == originating_user.client_message_id`
//! for every assistant + tool record in a session JSONL.
//!
//! Originating user is determined as:
//!   1. The user record whose `client_message_id` matches the
//!      `response_to_client_message_id` field on the assistant/tool record,
//!      when that field is present and non-empty.
//!   2. Otherwise, the most recent user record before this record.
//!
//! Today's M8.10 fix cycle (#629 -> #635 -> #637 -> #649) repeatedly missed
//! variations of the same bug class. The production JSONL had the answer all
//! along: record #6+7 in session `web-1777402538752` were tagged with the
//! wrong thread_id, but no test parsed the JSONL. This harness replays JSONL
//! fixtures so the next regression is caught at `cargo test` time.
//!
//! See `tests/fixtures/jsonl/README.md` for how to import a real production
//! session JSONL as a new regression fixture.
//!
//! Tracking: #649 (immediate bug), #654 (generative property tests umbrella).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::Value;

#[derive(Debug)]
#[allow(dead_code)] // fields are read via Debug formatting for test reports
struct ThreadBindingViolation {
    record_index: usize,
    role: String,
    timestamp: String,
    expected_thread_id: String,
    actual_thread_id: String,
    response_to_client_message_id: Option<String>,
    content_preview: String,
}

/// Parse a JSONL fixture and return every record that violates the
/// thread_id binding invariant.
fn check_jsonl(path: &Path) -> Vec<ThreadBindingViolation> {
    let body = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("fixture {} should exist: {}", path.display(), e));
    let records: Vec<Value> = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| {
                panic!("invalid JSON in {}: {} -- line: {}", path.display(), e, l)
            })
        })
        .collect();

    // First pass: collect every user-cmid we know about so we can validate
    // that response_to_client_message_id targets a real user record.
    let known_user_cmids: HashSet<String> = records
        .iter()
        .filter(|r| r["role"].as_str() == Some("user"))
        .filter_map(|r| r["client_message_id"].as_str().map(|s| s.to_string()))
        .collect();

    let mut violations = Vec::new();
    let mut current_user_cmid: Option<String> = None;

    for (i, r) in records.iter().enumerate() {
        let role = r["role"].as_str().unwrap_or("");
        let actual_tid = r["thread_id"].as_str().unwrap_or("").to_string();
        let timestamp = r["timestamp"].as_str().unwrap_or("").to_string();
        let content_preview: String = r["content"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        let rtcmid = r["response_to_client_message_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        match role {
            "user" => {
                if let Some(cmid) = r["client_message_id"].as_str() {
                    current_user_cmid = Some(cmid.to_string());
                    // User records: thread_id should equal their own cmid.
                    if actual_tid != cmid {
                        violations.push(ThreadBindingViolation {
                            record_index: i,
                            role: role.to_string(),
                            timestamp,
                            expected_thread_id: cmid.to_string(),
                            actual_thread_id: actual_tid,
                            response_to_client_message_id: rtcmid,
                            content_preview,
                        });
                    }
                }
            }
            "assistant" | "tool" => {
                // Prefer explicit response_to_client_message_id when it
                // points at a known user. Otherwise fall back to the most
                // recent user cmid in the stream.
                let expected = rtcmid
                    .clone()
                    .filter(|c| known_user_cmids.contains(c))
                    .or_else(|| current_user_cmid.clone());

                if let Some(expected) = expected {
                    if actual_tid != expected {
                        violations.push(ThreadBindingViolation {
                            record_index: i,
                            role: role.to_string(),
                            timestamp,
                            expected_thread_id: expected,
                            actual_thread_id: actual_tid,
                            response_to_client_message_id: rtcmid,
                            content_preview,
                        });
                    }
                }
            }
            // System / other roles are not subject to the invariant.
            _ => {}
        }
    }

    violations
}

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/jsonl")
        .join(name)
}

#[test]
fn issue_649_fixture_violates_invariant_until_fixed() {
    let path = fixture_path("issue-649-three-user-overflow.jsonl");
    let violations = check_jsonl(&path);

    assert!(
        !violations.is_empty(),
        "Issue #649 fixture should expose at least one thread_id binding violation"
    );

    eprintln!(
        "Issue #649 fixture surfaced {} violation(s):",
        violations.len()
    );
    for v in &violations {
        eprintln!("  {:?}", v);
    }

    // Three known production violations the fixture mirrors:
    //   record 1: assistant for slow-research has empty thread_id
    //   record 6: deep-research tool result tagged cmid-3 instead of cmid-2
    //   record 7: stock answer assistant tagged cmid-3 instead of cmid-2
    assert!(
        violations.len() >= 3,
        "Expected at least 3 violations from issue-649 fixture, got {}: {:?}",
        violations.len(),
        violations
    );

    let by_index: std::collections::HashMap<usize, &ThreadBindingViolation> =
        violations.iter().map(|v| (v.record_index, v)).collect();

    let v1 = by_index
        .get(&1)
        .expect("record 1 (assistant for slow-research) should be flagged");
    assert_eq!(v1.expected_thread_id, "cmid-1");
    assert_eq!(v1.actual_thread_id, "");

    let v6 = by_index
        .get(&6)
        .expect("record 6 (deep-research tool) should be flagged");
    assert_eq!(v6.expected_thread_id, "cmid-2");
    assert_eq!(v6.actual_thread_id, "cmid-3");

    let v7 = by_index
        .get(&7)
        .expect("record 7 (stock answer) should be flagged");
    assert_eq!(v7.expected_thread_id, "cmid-2");
    assert_eq!(v7.actual_thread_id, "cmid-3");
}

#[test]
fn correct_three_user_overflow_passes() {
    let path = fixture_path("correct-three-user-overflow.jsonl");
    let violations = check_jsonl(&path);
    assert!(
        violations.is_empty(),
        "Correct fixture should have zero thread_id binding violations, got: {:?}",
        violations
    );
}
