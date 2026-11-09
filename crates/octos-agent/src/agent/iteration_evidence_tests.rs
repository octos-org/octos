use super::*;
use serde_json::json;

fn setup() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".octos")).unwrap();
    fs::write(
        dir.path().join(".octos/continuation.json"),
        serde_json::to_vec(&json!({
            "schema_version":1, "task_id":"test", "owner_agent":"test", "evidence_file":"evidence.json",
            "stagnant_attempts":3, "deliverables":["artifact.bin"], "required_checks":[{"id":"layout","epsilon":0.01}]
        }))
        .unwrap(),
    )
    .unwrap();
    dir
}
fn report(root: &Path, attempt: u64, remaining: f64, status: &str) {
    let artifact = format!("candidate {attempt}");
    fs::write(root.join("artifact.bin"), &artifact).unwrap();
    fs::write(
        root.join("evidence.json"),
        serde_json::to_vec(&json!({
            "schema_version":1, "task_id":"test", "attempt":attempt, "phase":"checked",
            "artifacts":[{"path":"artifact.bin","sha256":digest(artifact.as_bytes())}],
            "checks":[{"id":"layout","status":status,"remaining":remaining}]
        }))
        .unwrap(),
    )
    .unwrap();
}
fn poll(root: &Path, final_answer: bool) -> Decision {
    Observer::new(root, "test").poll(final_answer).unwrap()
}

#[test]
fn large_evidence_within_byte_limit_is_verified_without_a_second_count_limit() {
    let d = setup();
    report(d.path(), 1, 0., "pass");
    let path = d.path().join("evidence.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for i in 0..1500 {
        let name = format!("part-{i}.bin");
        fs::write(d.path().join(&name), b"checked content").unwrap();
        value["artifacts"].as_array_mut().unwrap().push(json!({
            "path":name,"sha256":digest(b"checked content")
        }));
    }
    let bytes = serde_json::to_vec(&value).unwrap();
    assert!(bytes.len() as u64 <= MAX_RECORD_BYTES);
    fs::write(path, bytes).unwrap();
    assert_eq!(poll(d.path(), true), Decision::Continue);
    // Every binding remains enforced, including one after the old cutoff.
    fs::write(d.path().join("part-1499.bin"), b"changed after review").unwrap();
    assert!(matches!(poll(d.path(), true), Decision::Notice(message)
        if message.contains("stale artifact binding: part-1499.bin")));
}

#[test]
fn large_evidence_does_not_disable_the_existing_record_byte_limit() {
    let d = setup();
    report(d.path(), 1, 0., "pass");
    let path = d.path().join("evidence.json");
    let mut bytes = fs::read(&path).unwrap();
    bytes.resize(MAX_RECORD_BYTES as usize + 1, b' ');
    fs::write(path, bytes).unwrap();
    assert!(matches!(poll(d.path(), true), Decision::Notice(_)));
}

#[test]
fn completed_scope_can_expand_and_new_repairs_have_a_comparable_baseline() {
    let d = setup();
    let scoped = |n, remaining, status: &str, scope: serde_json::Value| {
        report(d.path(), n, remaining, status);
        let p = d.path().join("evidence.json");
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        v["scope"] = scope;
        v["scope_change_reason"] = json!("Continue the authorized next batch");
        fs::write(p, serde_json::to_vec(&v).unwrap()).unwrap();
    };
    scoped(1, 0., "pass", json!(["first"]));
    assert_eq!(poll(d.path(), true), Decision::Continue);
    scoped(2, 8., "unknown", json!(["first", "second"]));
    assert_eq!(poll(d.path(), false), Decision::Continue);
    let state: State = serde_json::from_slice(&fs::read(d.path().join(STATE)).unwrap()).unwrap();
    assert_eq!(state.attempt, 2);
    assert_eq!(state.flat_attempts, 0);
    assert_eq!(state.best["layout"].remaining, Some(8.));
    assert_eq!(
        fs::read_dir(d.path().join(".octos/continuation-scope-history"))
            .unwrap()
            .count(),
        1
    );
    scoped(3, 6., "unknown", json!(["first", "second"]));
    assert_eq!(poll(d.path(), false), Decision::Continue);
    let state: State = serde_json::from_slice(&fs::read(d.path().join(STATE)).unwrap()).unwrap();
    assert_eq!(
        state.flat_attempts, 0,
        "new work improvement must not compare against the smaller batch's zero"
    );
    scoped(4, 0., "pass", json!(["first"]));
    assert!(
        matches!(poll(d.path(), true), Decision::Notice(_)),
        "cannot remove the unfinished page"
    );
    scoped(4, 9., "unknown", json!(["first", "second", "third"]));
    assert!(
        matches!(poll(d.path(), true), Decision::Notice(_)),
        "cannot expand repeatedly to reset a failed streak"
    );
    scoped(4, 0., "pass", json!(["first", "second"]));
    assert_eq!(poll(d.path(), true), Decision::Continue);
}

#[test]
fn delivery_retarget_preserves_the_stagnation_history_and_rejects_other_policy_edits() {
    let d = setup();
    for n in 1..=3 {
        report(d.path(), n, 9., "fail");
        poll(d.path(), false);
    }
    let old = fs::read(d.path().join(POLICY)).unwrap();
    let mut new: serde_json::Value = serde_json::from_slice(&old).unwrap();
    new["deliverables"] = json!(["final.bin"]);
    let bytes = serde_json::to_vec(&new).unwrap();
    let revision = d.path().join(format!(
        ".octos/continuation-delivery-revisions/{}.json",
        digest(&bytes)
    ));
    fs::create_dir_all(revision.parent().unwrap()).unwrap();
    fs::write(&revision, serde_json::to_vec(&json!({
        "schema_version":1,"previous_policy":String::from_utf8(old.clone()).unwrap(),
        "new_policy_digest":digest(&bytes),"reason":"Move the final preview without resetting work"
    })).unwrap()).unwrap();
    fs::write(d.path().join(POLICY), &bytes).unwrap();
    report(d.path(), 4, 9., "fail");
    assert!(
        matches!(poll(d.path(), false), Decision::Notice(_)),
        "retarget must not reset the streak"
    );
    let state: State = serde_json::from_slice(&fs::read(d.path().join(STATE)).unwrap()).unwrap();
    assert_eq!(state.attempt, 4);
    assert_eq!(state.flat_attempts, 3);
    assert_eq!(state.policy_digest, digest(&bytes));
    // A forged delivery revision cannot also change goal identity or thresholds.
    let previous = bytes;
    new["stagnant_attempts"] = json!(999);
    let bytes = serde_json::to_vec(&new).unwrap();
    let revision = d.path().join(format!(
        ".octos/continuation-delivery-revisions/{}.json",
        digest(&bytes)
    ));
    fs::write(revision, serde_json::to_vec(&json!({"schema_version":1,"previous_policy":String::from_utf8(previous).unwrap(),"new_policy_digest":digest(&bytes),"reason":"Invalid threshold edit"})).unwrap()).unwrap();
    fs::write(d.path().join(POLICY), bytes).unwrap();
    assert!(Observer::new(d.path(), "test").poll(false).is_err());
}
#[test]
fn should_ignore_workspace_without_policy() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(poll(dir.path(), true), Decision::Continue);
}
#[test]
fn should_warn_then_yield_for_flat_verified_churn() {
    let d = setup();
    for n in 1..=3 {
        report(d.path(), n, 9.0, "fail");
        assert_eq!(poll(d.path(), false), Decision::Continue);
    }
    report(d.path(), 4, 9.0, "fail");
    assert!(matches!(poll(d.path(), false), Decision::Notice(_)));
    // A fresh observer proves the warning/streak survive process or turn restart.
    report(d.path(), 5, 9.0, "fail");
    assert!(matches!(poll(d.path(), false), Decision::Yield(_)));
}
#[test]
fn should_allow_improvement_beyond_window() {
    let d = setup();
    for n in 1..=20 {
        report(d.path(), n, 30.0 - n as f64, "fail");
        assert_eq!(poll(d.path(), false), Decision::Continue);
    }
    report(d.path(), 21, 0.0, "pass");
    assert_eq!(poll(d.path(), true), Decision::Continue);
}
#[test]
fn should_not_count_polling_or_pending_jobs() {
    let d = setup();
    report(d.path(), 1, 9.0, "fail");
    poll(d.path(), false);
    for _ in 0..30 {
        assert_eq!(poll(d.path(), false), Decision::Continue);
    }
    fs::write(d.path().join("evidence.json"),serde_json::to_vec(&json!({"schema_version":1,"task_id":"test","attempt":2,"phase":"pending","artifacts":[],"checks":[]})).unwrap()).unwrap();
    for _ in 0..30 {
        assert_eq!(poll(d.path(), false), Decision::Continue);
    }
    report(d.path(), 2, 9.0, "fail");
    assert_eq!(poll(d.path(), false), Decision::Continue);
}
#[test]
fn should_not_reward_oscillation() {
    let d = setup();
    for (n, v) in [(1, 10.), (2, 9.), (3, 10.), (4, 9.)] {
        report(d.path(), n, v, "fail");
        assert_eq!(poll(d.path(), false), Decision::Continue);
    }
    report(d.path(), 5, 10., "fail");
    assert!(matches!(poll(d.path(), false), Decision::Notice(_)));
}
#[test]
fn should_deny_unchanged_premature_completion_without_endless_veto() {
    let d = setup();
    report(d.path(), 1, 9., "fail");
    assert!(matches!(poll(d.path(), true), Decision::Notice(_)));
    assert!(matches!(poll(d.path(), true), Decision::Yield(_)));
}
#[test]
fn should_not_accept_stale_artifact_or_missing_check() {
    let d = setup();
    report(d.path(), 1, 0., "pass");
    fs::write(d.path().join("artifact.bin"), "changed after checking").unwrap();
    assert!(matches!(poll(d.path(), true), Decision::Notice(_)));
    report(d.path(), 2, 0., "pass");
    let path = d.path().join("evidence.json");
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    v["checks"] = json!([]);
    fs::write(path, serde_json::to_vec(&v).unwrap()).unwrap();
    assert!(matches!(poll(d.path(), true), Decision::Notice(_)));
}
#[test]
fn should_refuse_path_escape_and_policy_mutation() {
    let d = setup();
    report(d.path(), 1, 9., "fail");
    poll(d.path(), false);
    let path = d.path().join(".octos/continuation.json");
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    v["required_checks"] = json!([]);
    fs::write(path, serde_json::to_vec(&v).unwrap()).unwrap();
    assert!(Observer::new(d.path(), "test").poll(false).is_err());
    assert!(confined(d.path(), "../outside").is_err());
}

#[test]
fn should_count_expanding_inspection_coverage_as_progress_without_accepting_unknown() {
    let d = setup();
    for n in 1..=9 {
        report(d.path(), n, 20.0 - n as f64, "unknown");
        assert_eq!(poll(d.path(), false), Decision::Continue);
    }
    assert!(matches!(poll(d.path(), true), Decision::Notice(_)));
}
#[test]
fn should_not_forget_a_regression_when_another_check_improves() {
    let d = setup();
    let path = d.path().join(".octos/continuation.json");
    let mut policy: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    policy["required_checks"] = json!([{"id":"layout"},{"id":"copy"}]);
    fs::write(path, serde_json::to_vec(&policy).unwrap()).unwrap();
    for n in 1..=5 {
        report(d.path(), n, 10.0 - n as f64, "fail");
        let path = d.path().join("evidence.json");
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        v["checks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"copy","status":"fail","remaining":n}));
        fs::write(path, serde_json::to_vec(&v).unwrap()).unwrap();
        let decision = poll(d.path(), false);
        if n == 4 {
            assert!(matches!(decision, Decision::Notice(_)));
        }
        if n == 5 {
            assert!(matches!(decision, Decision::Yield(_)));
        }
    }
}

#[test]
fn should_not_apply_coordinator_requirements_to_a_page_worker() {
    let d = setup();
    report(d.path(), 1, 9., "fail");
    assert_eq!(
        Observer::new(d.path(), "different-worker")
            .poll(true)
            .unwrap(),
        Decision::Continue
    );
    assert!(!d.path().join(STATE).exists());
    assert!(matches!(poll(d.path(), true), Decision::Notice(_)));
}

#[test]
fn should_recognize_a_real_improvement_after_repairing_an_introduced_regression() {
    let d = setup();
    let path = d.path().join(".octos/continuation.json");
    let mut policy: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    policy["required_checks"] = json!([{"id":"layout"},{"id":"copy"}]);
    fs::write(path, serde_json::to_vec(&policy).unwrap()).unwrap();
    for (n, layout, copy) in [(1, 10., 1.), (2, 9., 2.), (3, 9., 1.)] {
        report(d.path(), n, layout, "fail");
        let path = d.path().join("evidence.json");
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        v["checks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"copy","status":"fail","remaining":copy}));
        fs::write(path, serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(poll(d.path(), false), Decision::Continue);
    }
    let state: State = serde_json::from_slice(&fs::read(d.path().join(STATE)).unwrap()).unwrap();
    assert_eq!(state.flat_attempts, 0);
}

#[test]
fn should_not_accept_an_old_candidate_as_evidence_for_an_unbound_delivery() {
    let d = setup();
    let path = d.path().join(".octos/continuation.json");
    let mut policy: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    policy["deliverables"] = json!(["actual-delivery.bin"]);
    fs::write(path, serde_json::to_vec(&policy).unwrap()).unwrap();
    report(d.path(), 1, 0., "pass");
    fs::write(
        d.path().join("actual-delivery.bin"),
        "not the inspected artifact",
    )
    .unwrap();
    assert!(matches!(poll(d.path(), true), Decision::Notice(_)));
}

#[test]
fn initial_pending_snapshot_is_waiting_not_corrupt_or_a_completed_attempt() {
    let d = setup();
    fs::write(d.path().join("evidence.json"),serde_json::to_vec(&json!({"schema_version":1,"task_id":"test","attempt":0,"phase":"pending","artifacts":[],"checks":[]})).unwrap()).unwrap();
    assert_eq!(poll(d.path(), false), Decision::Continue);
    let Decision::Notice(note) = poll(d.path(), true) else {
        panic!("pending final answer must remain incomplete")
    };
    assert!(note.contains("pending"));
    assert!(!note.contains("unavailable"));
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(d.path().join(STATE)).unwrap()).unwrap();
    assert_eq!(state["attempt"], 0);
    assert_eq!(state["flat_attempts"], 0);
    report(d.path(), 1, 0.0, "pass");
    assert_eq!(poll(d.path(), true), Decision::Continue);
}
