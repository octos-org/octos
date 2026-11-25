use octos_pptx::Context;
use serde_json::json;
mod common;

fn fixture() -> (tempfile::TempDir, Context) {
    common::labeled_fixture("visual-proof", "sample", "test")
}
#[test]
fn independent_route_preserves_unknowns_and_rejects_self_approval_and_native_failures() {
    use octos_pptx::review_routes as routes;
    let (_dir, mut ctx) = fixture();
    // A test-only alternate initial plan. Production init freezes this before work.
    let plan = routes::plan(
        &json!({"verification_mode":"independent_visual","reviewer_agent":"peer-reviewer"}),
        "test",
    )
    .unwrap();
    ctx.replace_json(routes::PLAN_PATH, &plan).unwrap();
    assert!(
        routes::plan(
            &json!({"verification_mode":"independent_visual","reviewer_agent":"test"}),
            "test"
        )
        .is_err()
    );
    common::cycle(&ctx, "route");
    let binding = ctx.binding("route/cycle.json").unwrap();
    let spec = serde_json::from_value(ctx.json("route/resolved-spec.json").unwrap()).unwrap();
    let policy = ctx.json(".octos/continuation.json").unwrap();
    let mut review = json!({"object_reviews":[{"page":"sample","object_id":"caption","checks":{}}],
        "measurement_reviews":[{"id":"owner","decision":"accepted_visual","basis":"The label sits inside the entire visible border, excluding its outer shadow."}]});
    for k in octos_pptx::object_review::CHECKS {
        review["object_reviews"][0]["checks"][k] =
            json!({"status":"pass","observation":"Checked synthetic pixels"});
    }
    let mut qa = json!({"status":"unknown","findings":[{"id":"owner","page":1,"object_id":"caption","code":"OWNER_BOUNDARY_UNVERIFIED","status":"unknown"}]});
    ctx.continuation_owner = Some("test".into());
    routes::attest(&ctx, &binding, &mut review).unwrap();
    assert!(!routes::authorized(&ctx, &policy, &review, &binding).unwrap());
    assert!(routes::adjudicated(&qa, &review, false, &spec, &plan).is_err());
    ctx.continuation_owner = Some("peer-reviewer".into());
    assert!(routes::permitted(&ctx).unwrap());
    routes::attest(&ctx, &binding, &mut review).unwrap();
    assert!(routes::authorized(&ctx, &policy, &review, &binding).unwrap());
    assert!(
        routes::adjudicated(&qa, &review, true, &spec, &plan)
            .unwrap()
            .contains("owner")
    );
    assert_eq!(qa["findings"][0]["status"], "unknown");
    let mut stale = binding.clone();
    stale["sha256"] = json!("different");
    assert!(routes::authorized(&ctx, &policy, &review, &stale).is_err());
    review["measurement_reviews"][0]["basis"] = json!("Changed after attestation");
    assert!(routes::authorized(&ctx, &policy, &review, &binding).is_err());
    qa["findings"][0]["status"] = json!("fail");
    assert!(routes::adjudicated(&qa, &review, true, &spec, &plan).is_err());
    qa["findings"][0]["status"] = json!("unknown");
    qa["findings"][0]["code"] = json!("CONSTRAINT_CHANGED");
    assert!(routes::adjudicated(&qa, &review, true, &spec, &plan).is_err());
    assert!(
        ctx.execute(json!({"action":"evidence_retarget","verification_mode":"strict"}))
            .is_err()
    );
}

#[test]
fn finding_evidence_and_display_number_must_identify_the_same_slide() {
    let (_dir, ctx) = fixture();
    let mut source = ctx.json("deck.json").unwrap();
    let mut extra = source["pages"][0].clone();
    extra["id"] = json!("other");
    source["pages"].as_array_mut().unwrap().insert(0, extra);
    ctx.replace_json("deck.json", &source).unwrap();
    common::cycle(&ctx, "two-pages");
    let c = ctx.json("two-pages/cycle.json").unwrap();
    let spec = serde_json::from_value(ctx.json("two-pages/resolved-spec.json").unwrap()).unwrap();
    let mut f = json!({"id":"local","page_number":2,"page":"sample","box":[45,45,350,130],
        "check":"copy","state":"open","evidence":"two-pages/rendered/slide-002.png"});
    assert!(octos_pptx::finding_guard::transition(&ctx, &f, None, &c, &spec).is_ok());
    f["page_number"] = json!(1);
    assert!(octos_pptx::finding_guard::transition(&ctx, &f, None, &c, &spec).is_err());
    f["page_number"] = json!(2);
    f["evidence"] = json!("two-pages/rendered/slide-001.png");
    assert!(octos_pptx::finding_guard::transition(&ctx, &f, None, &c, &spec).is_err());
    f["evidence"] = json!("two-pages/rendered/slide-002.png");
    f.as_object_mut().unwrap().remove("page");
    let resolved = octos_pptx::finding_guard::transition(&ctx, &f, None, &c, &spec).unwrap();
    assert_eq!(resolved["page"], "sample");
}

#[test]
fn should_block_mismatched_reference_even_when_all_differences_are_accepted() {
    let (_dir, ctx) = fixture();
    let other = common::bordered(200, 240, 350, 75);
    other.save(ctx.root.join("different.png")).unwrap();
    common::edit_spec(&ctx, |spec| {
        spec["pages"][0]["design"] = json!("different.png");
    });
    common::cycle(&ctx, "mismatched");

    let manifest = ctx.json("mismatched/comparisons/manifest.json").unwrap();
    let page = &manifest["pages"][0];
    assert_eq!(page["status"], "ready");
    let report = ctx.json(ctx.verify(&page["report"]).unwrap()).unwrap();
    assert_eq!(report["reference_alignment"]["status"], "incompatible");
    assert_eq!(
        report["reference_alignment"]["can_use_as_layout_reference"],
        false
    );

    let comparisons = common::accept(&ctx, "mismatched/comparisons/manifest.json");
    let recorded = common::record(&ctx, "mismatched", comparisons, None, json!([]), false).unwrap();
    let coverage = &recorded["comparison_coverage"]["pages"][0];
    assert!(
        coverage["status"] == "unknown"
            && coverage["reference_alignment"]["status"] == "incompatible"
            && coverage["defects"] == 0
            && coverage["unresolved"] == 1,
        "only alignment remains unresolved: {coverage}"
    );
}
