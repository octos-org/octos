//! Route unresolved evidence without treating every warning as a visual defect.
//! This summary changes no gate, observation, contract or continuation counter.
use serde_json::{Value, json};

pub fn summarize(checks: &Value, native: &Value, objects: &Value, comparisons: &Value) -> Value {
    let unresolved: Vec<_> = checks
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["status"] != "pass")
        .cloned()
        .collect();
    let package_failed = unresolved.iter().any(|c| c["id"] == "package");
    let measured = native["diagnosis"]["counts"]["measured_constraint_violations"].as_u64();
    let object_defects = objects["defect_objects"].as_u64();
    let comparison_defects = comparisons["defects"].as_u64();
    let native_known = measured.is_some();
    let review_known = object_defects.is_some() && comparison_defects.is_some();
    let action = if checks.as_array().is_none_or(|c| c.is_empty()) {
        "record_current_checks"
    } else if package_failed {
        "repair_package"
    } else if measured.is_some_and(|n| n > 0) {
        "resolve_measured_constraint_violations"
    } else if object_defects.is_some_and(|n| n > 0) || comparison_defects.is_some_and(|n| n > 0) {
        "review_reported_defects"
    } else if !unresolved.is_empty() || !native_known || !review_known {
        "resolve_evidence_gaps"
    } else {
        "required_checks_pass"
    };
    json!({"next_action":action,"unresolved_checks":unresolved,
        "measured_constraint_violations":measured,
        "object_defects":object_defects,"comparison_defects":comparison_defects,
        "measurement_gaps":native["diagnosis"]["counts"]["measurement_gaps"],
        "visual_questions":native["diagnosis"]["counts"]["visual_questions"],
        "unknown_objects":objects["unknown_objects"],"comparison_unknown":comparisons["unknown"],
        "counts_are":"Different evidence channels; not additive counts of unique visual defects.",
        "instruction":match action {
            "resolve_evidence_gaps" => "No repair is established by this summary. Inspect missing/stale coverage, source correspondence and unsupported geometry. Supply evidence or return the concrete tool limitation. Do not move correct text, enlarge owners or rewrite acceptance criteria just to lower unknown counts.",
            "resolve_measured_constraint_violations" => "Inspect the measured violation and its frozen target. Repair a demonstrated artifact error; if the target is wrong, report a contract conflict without silently widening it.",
            "review_reported_defects" => "Use the independent review's stable finding IDs, impact and unchanged acceptance conditions. Recheck changed regions and neighbours. These counts do not distinguish mandatory defects from optional refinements.",
            "required_checks_pass" => "Required evidence checks pass. Independent visual acceptance and current delivery bindings are still required; the tool does not judge aesthetics.",
            "repair_package" => "Repair or complete package validation before delivery.",
            _ => "Record a checked cycle and inspect the returned evidence before claiming completion."
        },"changes_acceptance":false,"quality_approved":false})
}
