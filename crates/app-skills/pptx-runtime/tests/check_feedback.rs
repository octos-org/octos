use octos_pptx::check_feedback::summarize;
use serde_json::{Value, json};

fn native(fail: u64, gaps: u64) -> Value {
    json!({"diagnosis":{"counts":{"measured_constraint_violations":fail,"measurement_gaps":gaps,"visual_questions":0}}})
}

#[test]
fn unresolved_measurement_is_not_an_instruction_to_move_text() {
    let checks = json!([{"id":"package","status":"pass"},{"id":"readability","status":"unknown","remaining":12}]);
    let before = checks.clone();
    let r = summarize(
        &checks,
        &native(0, 12),
        &json!({"defect_objects":0,"unknown_objects":2}),
        &json!({"defects":0,"unknown":0}),
    );
    assert_eq!(r["next_action"], "resolve_evidence_gaps");
    assert_eq!(r["quality_approved"], false);
    assert_eq!(r["changes_acceptance"], false);
    assert_eq!(checks, before);
    assert_eq!(r["unresolved_checks"][0]["remaining"], 12);
    // Missing evidence is not a zero-defect verdict.
    let missing = summarize(&json!([]), &Value::Null, &Value::Null, &Value::Null);
    assert_eq!(missing["next_action"], "record_current_checks");
    assert!(missing["measured_constraint_violations"].is_null());
}

#[test]
fn actual_failures_and_visual_observations_keep_distinct_routes() {
    let checks = json!([{"id":"readability","status":"fail"}]);
    let clean = json!({"defect_objects":0,"unknown_objects":0});
    let comparisons = json!({"defects":0,"unknown":0});
    let measured = summarize(&checks, &native(1, 10), &clean, &comparisons);
    assert_eq!(
        measured["next_action"],
        "resolve_measured_constraint_violations"
    );
    let visual = summarize(
        &checks,
        &native(0, 10),
        &json!({"defect_objects":1}),
        &comparisons,
    );
    assert_eq!(visual["next_action"], "review_reported_defects");
    let pass = summarize(
        &json!([{"id":"readability","status":"pass"}]),
        &native(0, 0),
        &clean,
        &comparisons,
    );
    assert_eq!(pass["next_action"], "required_checks_pass");
    assert_eq!(pass["quality_approved"], false);
    let unknown_native = summarize(
        &json!([{"id":"readability","status":"pass"}]),
        &Value::Null,
        &clean,
        &comparisons,
    );
    assert_eq!(unknown_native["next_action"], "resolve_evidence_gaps");
}
