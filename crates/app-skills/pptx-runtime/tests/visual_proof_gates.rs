use image::{Rgb, RgbImage};
use octos_pptx::{Context, fs::sha_file};
use serde_json::{Value, json};

fn rectangle(im: &mut RgbImage, x: u32, y: u32, w: u32, h: u32) {
    for px in x..=x + w {
        for dy in 0..3 {
            im.put_pixel(px, y + dy, Rgb([30, 60, 45]));
            im.put_pixel(px, y + h - dy, Rgb([30, 60, 45]));
        }
    }
    for py in y..=y + h {
        for dx in 0..3 {
            im.put_pixel(x + dx, py, Rgb([30, 60, 45]));
            im.put_pixel(x + w - dx, py, Rgb([30, 60, 45]));
        }
    }
}

fn fixture() -> (tempfile::TempDir, Context) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::new(dir.path()).unwrap();
    let mut art = RgbImage::from_pixel(640, 360, Rgb([245, 240, 225]));
    rectangle(&mut art, 40, 40, 360, 160);
    art.save(ctx.root.join("blank.png")).unwrap();
    ctx.write_json("deck.json", &json!({"schema_version":1,
      "canvas":{"width_px":640,"height_px":360,"width_in":12.8,"height_in":7.2},
      "styles":{"body":{"font":"Inter","size_pt":18}},
      "pages":[{"id":"sample","background":"blank.png","copy":{"caption":"Example label"},
      "regions":{"card":{"quad":[[40,40],[400,40],[400,200],[40,200]],"scope":"label","basis":"A model says this is measured"}},
      "texts":[{"id":"caption","text":"Example label","style":"body","box":[60,60,280,80],"region":"card"}]}]})).unwrap();
    ctx.execute(json!({"action":"evidence_init","task_id":"visual-proof","owner_agent":"test","stagnant_attempts":3,"deliverables":["final.pptx"]})).unwrap();
    (dir, ctx)
}
fn cycle(ctx: &Context, name: &str) {
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":name,"width":640}))
        .unwrap();
    std::fs::copy(
        ctx.root.join(name).join("editable.pptx"),
        ctx.root.join("final.pptx"),
    )
    .unwrap();
}
fn record(ctx: &Context, name: &str, state: &str) -> Value {
    ctx.replace_json("review.json",&json!({"cycle_sha256":sha_file(&ctx.root.join(name).join("cycle.json")).unwrap(),
    "inspected":{"copy":["sample"],"artwork":["sample"],"readability":["sample"]},
    "findings":[{"id":"caption-review","page":"sample","box":[45,45,350,130],"check":"readability","state":state,
      "note":"Synthetic local visual defect","evidence":format!("{name}/rendered/slide-001.png")}]})).unwrap();
    ctx.execute(json!({"action":"evidence_record","cycle":format!("{name}/cycle.json"),"review":"review.json"})).unwrap()
}

#[test]
fn should_not_verify_an_artwork_owner_from_prose_alone() {
    let (_dir, ctx) = fixture();
    RgbImage::from_pixel(640, 360, Rgb([245, 240, 225]))
        .save(ctx.root.join("blank.png"))
        .unwrap();
    cycle(&ctx, "blank-owner");
    let qa = ctx
        .execute(json!({"action":"qa_check","cycle":"blank-owner/cycle.json"}))
        .unwrap();
    assert!(
        qa["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["code"] == "OWNER_BOUNDARY_UNVERIFIED")
    );
    assert_ne!(qa["status"], "pass");
}

#[test]
fn should_probe_actual_artwork_before_rebuilding_without_editing_it() {
    let (_dir, ctx) = fixture();
    let before = sha_file(&ctx.root.join("blank.png")).unwrap();
    let good=ctx.execute(json!({"action":"geometry","image":"blank.png","quad":[[40,40],[400,40],[400,200],[40,200]]})).unwrap();
    assert_eq!(good["owner_boundary"]["status"], "supported");
    let wrong=ctx.execute(json!({"action":"geometry","image":"blank.png","quad":[[80,70],[350,70],[350,170],[80,170]]})).unwrap();
    assert_eq!(wrong["owner_boundary"]["status"], "unknown");
    assert_eq!(sha_file(&ctx.root.join("blank.png")).unwrap(), before);
}

#[test]
fn should_keep_unchanged_local_defect_open_despite_resolved_claim() {
    let (_dir, ctx) = fixture();
    cycle(&ctx, "first");
    record(&ctx, "first", "open");
    cycle(&ctx, "second");
    let result = record(&ctx, "second", "resolved");
    assert_ne!(result["checks"][3]["status"], "pass");
    let history = ctx.json(".octos/pptx-evidence-history.json").unwrap();
    assert_eq!(history["findings"]["caption-review"]["state"], "open");
    assert_eq!(
        history["findings"]["caption-review"]["closure_check"]["code"],
        "UNCHANGED_DEFECT_REGION"
    );
}

#[test]
fn finding_registration_captures_pixels_before_any_final_evidence_record() {
    let (_dir, ctx) = fixture();
    cycle(&ctx, "first");
    let registered = ctx
        .execute(
            json!({"action":"finding_register", "cycle":"first/cycle.json",
        "findings":[{"id":"local", "page":"sample", "object_id":"caption", "check":"readability",
        "state":"open", "note":"Caption is misplaced inside its card"}]}),
        )
        .unwrap();
    let finding = registered["findings"][0].clone();
    assert!(finding["registration"].is_object());
    let receipt = ctx
        .json(ctx.verify(&finding["registration"]).unwrap())
        .unwrap();
    assert!(ctx.verify(&receipt["views"]["current"]).is_ok());
    assert!(ctx.verify(&receipt["views"]["blank"]).is_ok());
    let mut spec = ctx.json("deck.json").unwrap();
    spec["pages"][0]["texts"][0]["box"] = json!([80, 70, 280, 80]);
    ctx.replace_json("deck.json", &spec).unwrap();
    cycle(&ctx, "fixed");
    let c = ctx.json("fixed/cycle.json").unwrap();
    let s = serde_json::from_value(ctx.json("fixed/resolved-spec.json").unwrap()).unwrap();
    let mut resolved = finding;
    resolved["state"] = json!("resolved");
    resolved["evidence"] = json!("fixed/rendered/slide-001.png");
    let result = octos_pptx::finding_guard::transition(&ctx, &resolved, None, &c, &s).unwrap();
    assert_eq!(result["state"], "resolved");
    assert_eq!(result["closure_check"]["quality_proven"], false);
    resolved["box"] = json!([0, 0, 640, 360]);
    assert!(octos_pptx::finding_guard::transition(&ctx, &resolved, None, &c, &s).is_err());
}

#[test]
fn incomplete_new_defects_are_rejected_at_registration_not_after_repair() {
    let (_dir, ctx) = fixture();
    cycle(&ctx, "first");
    let c = ctx.json("first/cycle.json").unwrap();
    let s = serde_json::from_value(ctx.json("first/resolved-spec.json").unwrap()).unwrap();
    let f = json!({"id":"missing", "check":"readability", "state":"open", "note":"misaligned"});
    assert!(octos_pptx::finding_guard::transition(&ctx, &f, None, &c, &s).is_err());
    let err = ctx
        .execute(json!({"action":"finding_register", "cycle":"first/cycle.json", "findings":[f]}))
        .unwrap_err();
    assert!(err.to_string().contains("page"));
}

#[test]
fn repair_evidence_cannot_be_bypassed_by_calling_a_defect_copy() {
    let (_dir, ctx) = fixture();
    cycle(&ctx, "first");
    let c = ctx.json("first/cycle.json").unwrap();
    let spec = serde_json::from_value(ctx.json("first/resolved-spec.json").unwrap()).unwrap();
    for category in ["copy", "artwork", "readability"] {
        let mut finding = json!({"id":"local","page":"sample","box":[45,45,350,130],
            "check":category,"state":"open","note":"local defect", "evidence":"first/rendered/slide-001.png"});
        let open = octos_pptx::finding_guard::transition(&ctx, &finding, None, &c, &spec).unwrap();
        finding["state"] = json!("resolved");
        let next =
            octos_pptx::finding_guard::transition(&ctx, &finding, Some(&open), &c, &spec).unwrap();
        assert_eq!(next["state"], "open", "{category}");
        assert_eq!(next["closure_check"]["code"], "UNCHANGED_DEFECT_REGION");
        let ungrounded =
            octos_pptx::finding_guard::transition(&ctx, &finding, None, &c, &spec).unwrap();
        assert_eq!(ungrounded["state"], "unknown");
        let legacy = octos_pptx::finding_guard::refresh(&ctx, &finding, &c, &spec).unwrap();
        assert_eq!(legacy["state"], "unknown");
    }
}

#[test]
fn finding_evidence_and_display_number_must_identify_the_same_slide() {
    let (_dir, ctx) = fixture();
    let mut source = ctx.json("deck.json").unwrap();
    let mut extra = source["pages"][0].clone();
    extra["id"] = json!("other");
    source["pages"].as_array_mut().unwrap().insert(0, extra);
    ctx.replace_json("deck.json", &source).unwrap();
    cycle(&ctx, "two-pages");
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
    let mut other = RgbImage::from_pixel(640, 360, Rgb([245, 240, 225]));
    rectangle(&mut other, 200, 240, 350, 75);
    other.save(ctx.root.join("different.png")).unwrap();
    let result=ctx.execute(json!({"action":"compare","before":"different.png","background":"blank.png","after":"blank.png","output":"comparison.png"})).unwrap();
    assert_eq!(result["reference_alignment"]["status"], "incompatible");
    assert_eq!(
        result["reference_alignment"]["can_use_as_layout_reference"],
        false
    );
}

#[test]
fn should_accept_unchanged_artwork_with_added_reference_text() {
    let (_dir, ctx) = fixture();
    let mut design = image::open(ctx.root.join("blank.png")).unwrap().to_rgb8();
    for y in 70..90 {
        for x in 70..250 {
            if x % 12 < 6 {
                design.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
    }
    design.save(ctx.root.join("design.png")).unwrap();
    let result=ctx.execute(json!({"action":"compare","before":"design.png","background":"blank.png","after":"blank.png","output":"comparison.png"})).unwrap();
    assert_eq!(result["reference_alignment"]["status"], "supported");
}

#[test]
fn should_accept_changed_local_pixels_but_not_treat_change_as_quality_proof() {
    let (_dir, ctx) = fixture();
    cycle(&ctx, "first");
    record(&ctx, "first", "open");
    let mut spec = ctx.json("deck.json").unwrap();
    spec["pages"][0]["texts"][0]["box"] = json!([80, 70, 280, 80]);
    ctx.replace_json("deck.json", &spec).unwrap();
    cycle(&ctx, "fixed");
    record(&ctx, "fixed", "resolved");
    let history = ctx.json(".octos/pptx-evidence-history.json").unwrap();
    assert_eq!(history["findings"]["caption-review"]["state"], "resolved");
    assert_eq!(
        history["findings"]["caption-review"]["closure_check"]["quality_proven"],
        false
    );
    record(&ctx, "fixed", "resolved");
    assert_eq!(
        ctx.json(".octos/pptx-evidence-history.json").unwrap()["findings"]["caption-review"]["state"],
        "resolved"
    );
}

#[test]
fn should_not_close_local_defect_when_only_an_unrelated_caption_changes() {
    let (_dir, ctx) = fixture();
    cycle(&ctx, "first");
    record(&ctx, "first", "open");
    let mut spec = ctx.json("deck.json").unwrap();
    spec["pages"][0]["copy"]["footer"] = json!("New unrelated footer");
    spec["pages"][0]["texts"].as_array_mut().unwrap().push(
        json!({"id":"footer","text":"New unrelated footer","style":"body","box":[50,270,350,50]}),
    );
    ctx.replace_json("deck.json", &spec).unwrap();
    cycle(&ctx, "elsewhere");
    record(&ctx, "elsewhere", "resolved");
    let history = ctx.json(".octos/pptx-evidence-history.json").unwrap();
    assert_eq!(history["findings"]["caption-review"]["state"], "open");
    assert_eq!(
        history["findings"]["caption-review"]["closure_check"]["code"],
        "UNCHANGED_DEFECT_REGION"
    );
}

#[test]
fn should_measure_a_slanted_boundary_and_reject_a_horizontal_substitute() {
    let mut blank = RgbImage::from_pixel(420, 280, Rgb([245, 240, 225]));
    let q = octos_pptx::geometry::frame([70., 80., 260., 90.], -12.);
    for i in 0..4 {
        let a = q[i];
        let b = q[(i + 1) % 4];
        for s in 0..800 {
            let t = f64::from(s) / 799.;
            let x = (a[0] + (b[0] - a[0]) * t).round() as u32;
            let y = (a[1] + (b[1] - a[1]) * t).round() as u32;
            for dx in 0..3 {
                for dy in 0..3 {
                    blank.put_pixel(x + dx - 1, y + dy - 1, Rgb([20, 20, 20]));
                }
            }
        }
    }
    assert_eq!(
        octos_pptx::visual_proof::owner_boundary(&blank, q)["status"],
        "supported"
    );
    assert_eq!(
        octos_pptx::visual_proof::owner_boundary(
            &blank,
            [[70., 80.], [330., 80.], [330., 170.], [70., 170.]]
        )["status"],
        "unknown"
    );
}

#[test]
fn should_reopen_closed_finding_after_local_render_changes() {
    let (_dir, ctx) = fixture();
    cycle(&ctx, "first");
    record(&ctx, "first", "open");
    let mut spec = ctx.json("deck.json").unwrap();
    spec["pages"][0]["texts"][0]["box"] = json!([80, 70, 280, 80]);
    ctx.replace_json("deck.json", &spec).unwrap();
    cycle(&ctx, "fixed");
    record(&ctx, "fixed", "resolved");
    spec["pages"][0]["texts"][0]["box"] = json!([60, 60, 280, 80]);
    ctx.replace_json("deck.json", &spec).unwrap();
    cycle(&ctx, "regressed");
    ctx.execute(json!({"action":"evidence_record","cycle":"regressed/cycle.json"}))
        .unwrap();
    assert_eq!(
        ctx.json(".octos/pptx-evidence-history.json").unwrap()["findings"]["caption-review"]["state"],
        "unknown"
    );
}

#[test]
fn should_not_let_model_region_approvals_override_bad_reference_pair() {
    let (_dir, ctx) = fixture();
    let mut other = RgbImage::from_pixel(640, 360, Rgb([245, 240, 225]));
    rectangle(&mut other, 200, 240, 350, 75);
    other.save(ctx.root.join("different.png")).unwrap();
    let mut spec = ctx.json("deck.json").unwrap();
    spec["pages"][0]["design"] = json!("different.png");
    ctx.replace_json("deck.json", &spec).unwrap();
    cycle(&ctx, "bad-pair");
    let manifest = ctx.json("bad-pair/comparisons/manifest.json").unwrap();
    let entry = &manifest["pages"][0];
    let report = ctx.json(ctx.verify(&entry["report"]).unwrap()).unwrap();
    let ids: Vec<_> = report["regions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].clone())
        .collect();
    let labels:Vec<_>=report["label_evidence"]["findings"].as_array().unwrap().iter().map(|f|json!({"id":f["id"],"evidence_sha256":f["evidence"]["sha256"],"classification":"acceptable_difference","reason_code":"intentional_change","note":"Accept every warning"})).collect();
    ctx.replace_json("review.json",&json!({"cycle_sha256":sha_file(&ctx.root.join("bad-pair/cycle.json")).unwrap(),
      "inspected":{"copy":["sample"],"artwork":["sample"],"readability":["sample"]},"findings":[],
      "comparisons":[{"page":"sample","comparison_key":entry["comparison_key"],"inspected":true,"note":"Looks fine",
      "regions":[{"ids":ids,"classification":"acceptable_difference","note":"Accept all differences"}],"label_findings":labels}]})).unwrap();
    let outcome=ctx.execute(json!({"action":"evidence_record","cycle":"bad-pair/cycle.json","review":"review.json"})).unwrap();
    assert_eq!(outcome["comparison_coverage"]["unknown"], 1);
    assert_ne!(outcome["checks"][3]["status"], "pass");
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
    cycle(&ctx, "route");
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
fn local_review_reuse_invalidates_changes_without_promoting_old_unknowns() {
    let (_dir, ctx) = fixture();
    let mut s = ctx.json("deck.json").unwrap();
    s["pages"][0]["copy"]["distant"] = json!("Distant");
    s["pages"][0]["texts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"distant","text":"Distant","style":"body","box":[470,300,150,50]}));
    ctx.replace_json("deck.json", &s).unwrap();
    cycle(&ctx, "reuse-old");
    let (c, spec) =
        octos_pptx::review::bound_cycle(&ctx, &ctx.path("reuse-old/cycle.json").unwrap()).unwrap();
    let items = octos_pptx::object_review::inventory(&c, &spec).unwrap();
    let mut rows=items.iter().map(|i|json!({"page":i["page"],"object_id":i["object_id"],"evidence_hash":i["evidence_hash"],"checks":{}})).collect::<Vec<_>>();
    for row in &mut rows {
        for key in octos_pptx::object_review::CHECKS {
            row["checks"][key] = json!({"status":"pass","observation":"Checked synthetic pixels"});
        }
    }
    let prior = json!({"cycle_sha256":ctx.binding("reuse-old/cycle.json").unwrap()["sha256"],"object_reviews":rows});
    ctx.write_json("prior.json", &prior).unwrap();
    s["pages"][0]["texts"][1]["box"] = json!([480, 300, 150, 50]);
    ctx.replace_json("deck.json", &s).unwrap();
    cycle(&ctx, "reuse-new");
    let collect = || {
        octos_pptx::review_reuse::collect(
            &ctx,
            &ctx.path("reuse-new/cycle.json").unwrap(),
            "reuse-old/cycle.json",
            "prior.json",
            &[],
        )
        .unwrap()
    };
    let reuse = collect();
    assert_eq!(reuse["object_reviews"].as_array().unwrap().len(), 1);
    assert_eq!(reuse["object_reviews"][0]["object_id"], "caption");
    let mut unknown = prior.clone();
    unknown["object_reviews"][0]["checks"]["alignment"]["status"] = json!("unknown");
    ctx.replace_json("prior.json", &unknown).unwrap();
    assert!(collect()["object_reviews"].as_array().unwrap().is_empty());
    ctx.replace_json("prior.json", &prior).unwrap();
    s["pages"][0]["texts"][0]["box"] = json!([70, 60, 280, 80]);
    ctx.replace_json("deck.json", &s).unwrap();
    cycle(&ctx, "reuse-local-change");
    let reuse = octos_pptx::review_reuse::collect(
        &ctx,
        &ctx.path("reuse-local-change/cycle.json").unwrap(),
        "reuse-old/cycle.json",
        "prior.json",
        &[],
    )
    .unwrap();
    assert!(reuse["object_reviews"].as_array().unwrap().is_empty());
}

#[test]
fn independent_review_submission_is_bound_and_native_status_remains_visible() {
    let (_dir, mut ctx) = fixture();
    ctx.replace_json(
        octos_pptx::review_routes::PLAN_PATH,
        &octos_pptx::review_routes::plan(
            &json!({"verification_mode":"independent_visual","reviewer_agent":"reviewer"}),
            "test",
        )
        .unwrap(),
    )
    .unwrap();
    RgbImage::from_pixel(640, 360, Rgb([245, 240, 225]))
        .save(ctx.root.join("blank.png"))
        .unwrap();
    let mut art = RgbImage::from_pixel(640, 360, Rgb([245, 240, 225]));
    for y in (0..340).step_by(40) {
        for x in (0..620).step_by(40) {
            if x >= 420 || y >= 220 {
                rectangle(&mut art, x, y, 20, 20);
            }
        }
    }
    art.save(ctx.root.join("blank.png")).unwrap();
    cycle(&ctx, "reference-source");
    std::fs::copy(
        ctx.root.join("reference-source/rendered/slide-001.png"),
        ctx.root.join("reference.png"),
    )
    .unwrap();
    let mut source = ctx.json("deck.json").unwrap();
    source["pages"][0]["design"] = json!("reference.png");
    ctx.replace_json("deck.json", &source).unwrap();
    cycle(&ctx, "submission");
    let pack = ctx
        .execute(
            json!({"action":"review_pack","cycle":"submission/cycle.json","output":"review-pack"}),
        )
        .unwrap();
    let draft = ctx
        .json(ctx.verify(&pack["observations_template"]).unwrap())
        .unwrap();
    let index = ctx.json(ctx.verify(&pack["view_index"]).unwrap()).unwrap();
    assert_eq!(
        draft["measurement_reviews"],
        index["native_review_candidates"]
    );
    assert!(
        draft["measurement_reviews"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["decision"] == "unknown" && r["basis"] == "")
    );
    let mut obs = ctx
        .json(ctx.verify(&pack["object_review_template"]).unwrap())
        .unwrap();
    for row in obs["object_reviews"].as_array_mut().unwrap() {
        for key in octos_pptx::object_review::CHECKS {
            row["checks"][key] = json!({"status":"pass","observation":"Synthetic independent fixture: clear glyphs in the intended label area"});
        }
        if row["fidelity"].is_object() {
            row["fidelity"]["comparison_reliable"] = json!(true);
            row["fidelity"]["layout_relation"] = json!("preserved");
            row["fidelity"]["reference_interference"] = json!("absent");
            row["fidelity"]["current_interference"] = json!("absent");
            row["fidelity"]["interference_change"] = json!("equivalent");
            row["fidelity"]["basis"] = json!(
                "Synthetic original and candidate are the same render; no new relationship or interference"
            );
        }
    }
    for row in obs["relation_reviews"].as_array_mut().unwrap() {
        row["status"] = json!("visual_acceptance");
        row["basis"] = json!("Synthetic fixture independently checked against actual pixels");
    }
    let qa = ctx
        .execute(json!({"action":"qa_check","cycle":"submission/cycle.json"}))
        .unwrap();
    obs["measurement_reviews"] = octos_pptx::review_routes::candidates(
        &qa,
        &octos_pptx::review_routes::load_plan(&ctx).unwrap(),
    );
    assert!(!obs["measurement_reviews"].as_array().unwrap().is_empty());
    for row in obs["measurement_reviews"].as_array_mut().unwrap() {
        row["decision"] = json!("accepted_visual");
        row["basis"] =
            json!("Clear borderless label against uniform background; no visible interference");
    }
    for page in obs["comparisons"].as_array_mut().unwrap() {
        page["inspected"] = json!(true);
        page["note"] = json!(
            "Synthetic reference is the exact same raster; full-page comparison independently inspected"
        );
        for region in page["regions"].as_array_mut().unwrap() {
            region["classification"] = json!("acceptable_difference");
            region["note"] = json!("Synthetic matching render independently inspected");
        }
        for label in page["label_findings"].as_array_mut().unwrap() {
            label["classification"] = json!("acceptable_difference");
            label["reason_code"] = json!("measurement_artifact");
            label["note"] = json!(
                "Synthetic reference was copied from this exact renderer and source; compare exact pixels"
            );
        }
    }
    ctx.write_json("observations.json", &obs).unwrap();
    ctx.continuation_owner = Some("test".into());
    assert!(ctx.execute(json!({"action":"review_submit","pack":"review-pack/report.json","observations":"observations.json","output":"self-review.json"})).is_err());
    ctx.continuation_owner = Some("reviewer".into());
    ctx.execute(json!({"action":"review_submit","pack":"review-pack/report.json","observations":"observations.json","output":"bound-review.json"})).unwrap();
    assert_eq!(
        ctx.json("bound-review.json").unwrap()["inspected"]["copy"],
        json!(["sample"])
    );
    ctx.continuation_owner = Some("test".into());
    let result=ctx.execute(json!({"action":"evidence_record","cycle":"submission/cycle.json","review":"bound-review.json"})).unwrap();
    assert_eq!(result["native_qa"]["status"], "unknown");
    assert!(
        !result["native_qa"]["independent_visual_acceptances"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        result["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["status"] == "pass"),
        "{result}"
    );
    let status = ctx.execute(json!({"action":"evidence_status"})).unwrap();
    assert_eq!(
        status["verification_route"]["independent_visual_acceptances"],
        result["native_qa"]["independent_visual_acceptances"]
    );
    ctx.replace_json(
        octos_pptx::review_routes::PLAN_PATH,
        &json!({"mode":"strict"}),
    )
    .unwrap();
    assert!(ctx.execute(json!({"action":"evidence_record","cycle":"submission/cycle.json","review":"bound-review.json"})).is_err());
}

#[test]
fn raw_peer_session_receipt_and_host_identity_resolve_to_the_same_reviewer() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = Context::new(dir.path()).unwrap();
    ctx.continuation_owner = Some("session:deepseek:cli:chat#peer-maker".into());
    ctx.execute(json!({"action":"evidence_init","task_id":"identity","stagnant_attempts":3,"deliverables":["final.pptx"],"verification_mode":"independent_visual","reviewer_agent":"deepseek:cli:chat#peer-reviewer"})).unwrap();
    assert!(!octos_pptx::review_routes::permitted(&ctx).unwrap());
    ctx.continuation_owner = Some("session:deepseek:cli:chat#peer-reviewer".into());
    assert!(octos_pptx::review_routes::permitted(&ctx).unwrap());
    assert!(octos_pptx::review_routes::plan(&json!({"verification_mode":"independent_visual","reviewer_agent":"deepseek:cli:chat#peer-maker"}),"session:deepseek:cli:chat#peer-maker").is_err());
}

#[test]
fn doctor_exposes_host_identity_and_help_scopes_evidence_instructions() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = Context::new(dir.path()).unwrap();
    assert_eq!(
        ctx.execute(json!({"action":"doctor"})).unwrap()["continuation"]["host_identity"],
        Value::Null
    );
    ctx.continuation_owner = Some("session:profile:peer".into());
    assert_eq!(
        ctx.execute(json!({"action":"doctor"})).unwrap()["continuation"]["host_identity"],
        "session:profile:peer"
    );
    let help = ctx
        .execute(json!({"action":"help","topic":"evidence_record"}))
        .unwrap();
    assert_eq!(help["required"], json!(["cycle"]));
    assert!(help.get("fit").is_none());
    assert_eq!(
        ctx.execute(json!({"action":"help","topic":"review"}))
            .unwrap()["quality_approved"],
        false
    );
}

#[test]
fn review_reuse_carries_registered_defect_metadata_without_manufacturing_resolution() {
    let before = json!({"id":"label","page":"sample","object_id":"caption","check":"readability","box":[1,2,30,40],"registration":{"path":"receipt.json","sha256":"test"},"state":"open","note":"Before repair"});
    let reuse = json!({"findings":[before]});
    let mut obs =
        json!({"findings":[{"id":"label","state":"resolved","note":"Checked after repair"}]});
    octos_pptx::review_reuse::merge(&mut obs, &reuse).unwrap();
    assert_eq!(
        obs["findings"][0]["registration"],
        reuse["findings"][0]["registration"]
    );
    assert_eq!(obs["findings"][0]["box"], reuse["findings"][0]["box"]);
    assert_eq!(obs["findings"][0]["state"], "resolved");
    let mut omitted = json!({});
    octos_pptx::review_reuse::merge(&mut omitted, &reuse).unwrap();
    assert_eq!(omitted["findings"][0]["state"], "open");
    let mut forged = json!({"findings":[{"id":"label","state":"resolved","box":[0,0,999,999]}]});
    assert!(octos_pptx::review_reuse::merge(&mut forged, &reuse).is_err());
}

#[test]
fn registered_finding_survives_real_pack_submit_and_record_without_manual_receipt_copy() {
    let (_dir, ctx) = fixture();
    cycle(&ctx, "origin");
    let pack = ctx
        .execute(
            json!({"action":"review_pack","cycle":"origin/cycle.json","output":"review-origin"}),
        )
        .unwrap();
    let mut obs = ctx
        .json(ctx.verify(&pack["observations_template"]).unwrap())
        .unwrap();
    obs["findings"] = json!([{"id":"caption-placement","page":"sample","object_id":"caption","check":"readability","state":"open","note":"Synthetic misplaced caption"}]);
    ctx.write_json("origin-observations.json", &obs).unwrap();
    ctx.execute(json!({"action":"review_submit","pack":"review-origin/report.json","observations":"origin-observations.json","output":"origin-bound.json"})).unwrap();
    let before = ctx.json("origin-bound.json").unwrap()["findings"][0].clone();
    assert!(before["registration"].is_object());
    let mut spec = ctx.json("deck.json").unwrap();
    spec["pages"][0]["texts"][0]["box"] = json!([80, 70, 280, 80]);
    ctx.replace_json("deck.json", &spec).unwrap();
    cycle(&ctx, "repair");
    let pack = ctx.execute(json!({"action":"review_pack","cycle":"repair/cycle.json","output":"review-repair","previous_cycle":"origin/cycle.json","previous_review":"origin-bound.json"})).unwrap();
    let mut obs = ctx
        .json(ctx.verify(&pack["observations_template"]).unwrap())
        .unwrap();
    obs["findings"] = json!([{"id":"caption-placement","state":"resolved","note":"Synthetic reviewer inspected repaired caption and neighbors"}]);
    ctx.write_json("repair-observations.json", &obs).unwrap();
    ctx.execute(json!({"action":"review_submit","pack":"review-repair/report.json","observations":"repair-observations.json","output":"repair-bound.json"})).unwrap();
    // The first V4 packs did not serialize findings into their reuse section.
    // Their predecessor is still bound and sufficient to recover the receipt.
    let mut legacy_pack = ctx.json("review-repair/report.json").unwrap();
    legacy_pack["reuse"]
        .as_object_mut()
        .unwrap()
        .remove("findings");
    ctx.replace_json("review-repair/report.json", &legacy_pack)
        .unwrap();
    ctx.execute(json!({"action":"review_submit","pack":"review-repair/report.json","observations":"repair-observations.json","output":"legacy-repair-bound.json"})).unwrap();
    assert_eq!(
        ctx.json("legacy-repair-bound.json").unwrap()["findings"],
        ctx.json("repair-bound.json").unwrap()["findings"]
    );
    let after = ctx.json("repair-bound.json").unwrap()["findings"][0].clone();
    assert_eq!(after["registration"], before["registration"]);
    assert_eq!(after["box"], before["box"]);
    assert_eq!(
        ctx.path(after["evidence"].as_str().unwrap()).unwrap(),
        ctx.path("repair/rendered/slide-001.png").unwrap(),
        "before render must not become after evidence"
    );
    ctx.execute(json!({"action":"evidence_record","cycle":"repair/cycle.json","review":"repair-bound.json"})).unwrap();
    let history = ctx.json(".octos/pptx-evidence-history.json").unwrap();
    let finding = &history["findings"]["caption-placement"];
    assert_eq!(finding["state"], "resolved", "{finding}");
    assert_eq!(finding["closure_check"]["code"], "LOCAL_CHANGE_RECORDED");
    assert_eq!(finding["closure_check"]["quality_proven"], false);
}

#[test]
fn new_unknown_categories_do_not_silently_expand_an_old_frozen_route() {
    use octos_pptx::review_routes as routes;
    let (_dir, ctx) = fixture();
    cycle(&ctx, "unboxed");
    let spec = serde_json::from_value(ctx.json("unboxed/resolved-spec.json").unwrap()).unwrap();
    let current = routes::plan(
        &json!({"verification_mode":"independent_visual","reviewer_agent":"reviewer"}),
        "test",
    )
    .unwrap();
    let legacy =
        json!({"mode":"independent_visual","eligible_unknown_codes":["OWNER_BOUNDARY_UNVERIFIED"]});
    let qa = json!({"findings":[{"id":"unboxed","page":1,"object_id":"caption","code":"OWNER_UNVERIFIED","status":"unknown"}]});
    let mut review = json!({"object_reviews":[{"page":"sample","object_id":"caption","checks":{}}],"measurement_reviews":[{"id":"unboxed","decision":"accepted_visual","basis":"Unboxed heading independently inspected with neighboring artwork"}]});
    for key in octos_pptx::object_review::CHECKS {
        review["object_reviews"][0]["checks"][key] =
            json!({"status":"pass","observation":"Synthetic independent observation"});
    }
    assert!(routes::adjudicated(&qa, &review, true, &spec, &legacy).is_err());
    assert!(
        routes::candidates(&qa, &legacy)
            .as_array()
            .unwrap()
            .is_empty()
    );
    let accepted = routes::adjudicated(&qa, &review, true, &spec, &current).unwrap();
    assert!(accepted.contains("unboxed"));
    let blocked = routes::feedback(&qa, &review, &Default::default(), &legacy);
    assert_eq!(
        blocked["remaining"][0]["next_action"],
        "measurement_or_contract_blocker"
    );
    let pending = routes::feedback(&qa, &json!({}), &Default::default(), &current);
    assert_eq!(
        pending["remaining"][0]["next_action"],
        "independent_review_required"
    );
}

#[test]
fn resolution_change_alone_cannot_close_a_registered_defect() {
    let (_dir, ctx) = fixture();
    cycle(&ctx, "before-scale");
    let registered = ctx.execute(json!({"action":"finding_register","cycle":"before-scale/cycle.json","findings":[{"id":"caption","page":"sample","object_id":"caption","check":"readability","state":"open","note":"Synthetic misplaced caption"}]})).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"scaled","width":1280}))
        .unwrap();
    let cycle = ctx.json("scaled/cycle.json").unwrap();
    let spec = serde_json::from_value(ctx.json("scaled/resolved-spec.json").unwrap()).unwrap();
    let mut claimed = registered["findings"][0].clone();
    claimed["state"] = json!("resolved");
    claimed["evidence"] = cycle["renders"]["editable"]["pages"][0]["path"].clone();
    let outcome =
        octos_pptx::finding_guard::transition(&ctx, &claimed, None, &cycle, &spec).unwrap();
    assert_eq!(outcome["state"], "open");
    assert_eq!(
        outcome["closure_check"]["code"],
        "REPAIR_RENDER_DIMENSIONS_CHANGED"
    );
}
