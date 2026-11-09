use octos_pptx::{Context, qa};
use serde_json::json;

#[test]
fn separates_measured_violations_questions_and_measurement_gaps_without_approving_them() {
    let findings = vec![
        json!({"code":"INK_OUTSIDE_OWNER","status":"fail","page":1,"object_id":"a"}),
        json!({"code":"ARTWORK_CONTACT_UNVERIFIED","status":"unknown","page":1,"object_id":"b"}),
        json!({"code":"OWNER_BOUNDARY_UNVERIFIED","status":"unknown","page":1,"object_id":"b"}),
        json!({"code":"OWNER_BOUNDARY_UNVERIFIED","status":"unknown","page":2,"object_id":"c"}),
        json!({"code":"FUTURE_UNKNOWN_CHECK","status":"unknown","page":2,"object_id":"d"}),
    ];
    let diagnosis = qa::diagnosis(&findings);
    assert_eq!(diagnosis["counts"]["measured_constraint_violations"], 1);
    assert_eq!(diagnosis["counts"]["visual_questions"], 1);
    assert_eq!(diagnosis["counts"]["measurement_gaps"], 3);
    assert_eq!(diagnosis["distinct_affected_objects"], 4);
    assert_eq!(
        diagnosis["next_step"],
        "repair_measured_constraints_then_review"
    );
    assert_eq!(diagnosis["quality_approved"], false);
    assert_eq!(diagnosis["changes_acceptance"], false);
    let grouped = diagnosis["by_code"].as_array().unwrap();
    assert_eq!(grouped.len(), 4);
    assert_eq!(
        grouped
            .iter()
            .find(|v| v["code"] == "OWNER_BOUNDARY_UNVERIFIED")
            .unwrap()["occurrences"],
        2
    );
    let gap_only = qa::diagnosis(&findings[2..]);
    assert_eq!(
        gap_only["next_step"],
        "resolve_measurement_scope_or_report_blocker"
    );
    assert_ne!(gap_only["next_step"], "pass");
    assert_eq!(
        qa::diagnosis(&findings[1..2])["next_step"],
        "independent_visual_adjudication"
    );
    assert_eq!(
        qa::diagnosis(&[])["next_step"],
        "complete_independent_visual_review"
    );
}

#[test]
fn a_failed_check_is_never_downgraded_by_its_code_name() {
    let d = qa::diagnosis(&[
        json!({"code":"ARTWORK_CONTACT_UNVERIFIED","status":"fail","page":1,"object_id":"caption"}),
    ]);
    assert_eq!(d["counts"]["measured_constraint_violations"], 1);
    assert_eq!(d["counts"]["visual_questions"], 0);
}

#[test]
fn cycle_and_evidence_return_diagnostics_but_do_not_silently_clear_borderless_unknowns() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = Context::new(tmp.path()).unwrap();
    image::RgbImage::from_pixel(600, 400, image::Rgb([245; 3]))
        .save(tmp.path().join("art.png"))
        .unwrap();
    ctx.write_json("source.json", &json!({"schema_version":1,
        "canvas":{"width_px":600,"height_px":400,"width_in":6,"height_in":4},
        "styles":{"body":{"font":"Inter","size_pt":20}},
        "pages":[{"id":"note","background":"art.png","copy":{"caption":"A borderless caption"},
            "texts":[{"id":"caption","text":"A borderless caption","style":"body","box":[60,90,400,80]}]}]})).unwrap();
    let built = ctx
        .execute(json!({"action":"cycle","spec":"source.json","output":"cycle","width":600}))
        .unwrap();
    assert_eq!(built["native_qa"]["status"], "unknown");
    assert_eq!(
        built["native_qa"]["diagnosis"]["next_step"],
        "resolve_measurement_scope_or_report_blocker"
    );
    let checked = ctx
        .execute(json!({"action":"qa_check","cycle":"cycle/cycle.json","output":"check.json"}))
        .unwrap();
    assert_eq!(checked["diagnosis"], built["native_qa"]["diagnosis"]);
    let pack = ctx
        .execute(json!({"action":"review_pack","cycle":"cycle/cycle.json","output":"pack"}))
        .unwrap();
    assert_eq!(pack["finding_summary"]["diagnosis"], checked["diagnosis"]);
    ctx.execute(json!({"action":"evidence_init","task_id":"diagnostic-test","owner_agent":"maker","stagnant_attempts":3,"deliverables":["cycle/editable.pptx"]})).unwrap();
    let recorded = ctx
        .execute(json!({"action":"evidence_record","cycle":"cycle/cycle.json"}))
        .unwrap();
    assert_eq!(recorded["native_qa"]["status"], "unknown");
    assert_eq!(recorded["native_qa"]["diagnosis"], checked["diagnosis"]);
    let status = ctx
        .execute(json!({"action":"evidence_status","owner_agent":"maker"}))
        .unwrap();
    assert_eq!(status["native_qa"]["diagnosis"], checked["diagnosis"]);
    assert_eq!(status["quality_approved"], false);
}
