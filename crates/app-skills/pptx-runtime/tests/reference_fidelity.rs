use image::{Rgb, RgbImage};
use octos_pptx::{Context, fidelity, fs::sha_file};
use serde_json::{Value, json};

fn facts() -> Value {
    json!({"comparison_reliable":true,"layout_relation":"preserved",
        "reference_interference":"absent","current_interference":"absent",
        "interference_change":"equivalent","basis":"Synthetic original/current pixel comparison"})
}

#[test]
fn readable_but_added_interference_cannot_pass_even_with_a_prose_waiver() {
    let mut f = facts();
    f["current_interference"] = json!("present");
    f["basis"] =
        json!("The foreground strokes are still readable, so call this intentional and optional");
    assert_eq!(
        fidelity::verdict(&f),
        ("defect", "ADDED_REFERENCE_INTERFERENCE")
    );
    f["reference_interference"] = json!("present");
    f["interference_change"] = json!("increased");
    assert_eq!(fidelity::verdict(&f).0, "defect");
    f["interference_change"] = json!("equivalent");
    f["layout_relation"] = json!("changed");
    assert_eq!(
        fidelity::verdict(&f),
        ("defect", "REFERENCE_RELATION_CHANGED")
    );
}

#[test]
fn intentional_overlap_and_preserved_slant_are_not_automatically_defects() {
    let mut f = facts();
    f["reference_interference"] = json!("present");
    f["current_interference"] = json!("present");
    f["basis"] = json!(
        "The reference itself uses slanted type over textured artwork, preserved in the candidate"
    );
    assert_eq!(fidelity::verdict(&f).0, "pass");
    f["current_interference"] = json!("absent");
    // Internally contradictory facts do not get silently interpreted as pass.
    assert_eq!(fidelity::verdict(&f).0, "unknown");
    f["interference_change"] = json!("reduced");
    assert_eq!(fidelity::verdict(&f).0, "pass");
    f["comparison_reliable"] = json!(false);
    assert_eq!(fidelity::verdict(&f).0, "unknown");
}

#[test]
fn missing_or_stale_reference_proof_never_inherits_readability_pass() {
    let item = json!({"evidence_hash":"current","viewport_normalized_xyxy":[0.1,0.2,0.5,0.6],
        "sources":[{"role":"design","image":{"sha256":"original"}},
            {"role":"blank","image":{"sha256":"blank"}},
            {"role":"current","image":{"sha256":"current"}}]});
    let mut row = json!({"evidence_hash":"current","fidelity":facts()});
    assert_eq!(fidelity::object_result(&item, &row).0, "unknown");
    row["fidelity"]["evidence"] = fidelity::evidence(&item);
    assert_eq!(fidelity::object_result(&item, &row).0, "pass");
    row["fidelity"]["evidence"]["sources"][0]["image"]["sha256"] = json!("other-reference");
    assert_eq!(fidelity::object_result(&item, &row).0, "unknown");
    let source = json!({"fidelity":fidelity::scaffold(&item)});
    assert!(fidelity::bind(&source, &mut row).is_err());
}

fn fixture() -> (tempfile::TempDir, Context) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::new(dir.path()).unwrap();
    let art = RgbImage::from_fn(640, 360, |x, y| {
        if (x > 430 || y > 230) && (x % 35 < 3 || y % 35 < 3) {
            Rgb([70, 100, 120])
        } else {
            Rgb([245, 240, 225])
        }
    });
    art.save(ctx.root.join("blank.png")).unwrap();
    ctx.write_json("deck.json", &json!({"schema_version":1,
        "canvas":{"width_px":640,"height_px":360,"width_in":12.8,"height_in":7.2},
        "styles":{"body":{"font":"Inter","size_pt":22}},
        "pages":[{"id":"example","background":"blank.png","copy":{"label":"Example label"},
            "texts":[{"id":"label","text":"Example label","style":"body","box":[75,85,300,85],"rotation":-8}]}]})).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"reference","width":640}))
        .unwrap();
    std::fs::copy(
        ctx.root.join("reference/rendered/slide-001.png"),
        ctx.root.join("original.png"),
    )
    .unwrap();
    let mut source = ctx.json("deck.json").unwrap();
    source["pages"][0]["design"] = json!("original.png");
    ctx.replace_json("deck.json", &source).unwrap();
    ctx.execute(json!({"action":"evidence_init","task_id":"reference-test","owner_agent":"maker",
        "stagnant_attempts":2,"deliverables":["final.pptx"],"verification_mode":"independent_visual",
        "reviewer_agent":"reviewer","arbiter_agent":"arbiter"})).unwrap();
    (dir, ctx)
}

// Authored test observations simulate the documented reviewer decisions. They
// test enforcement, not an automated vision model or its aesthetic accuracy.
fn observations(ctx: &Context, pack: &Value, f: Value) -> Value {
    let mut obs = ctx
        .json(ctx.verify(&pack["observations_template"]).unwrap())
        .unwrap();
    for row in obs["object_reviews"].as_array_mut().unwrap() {
        for key in octos_pptx::object_review::CHECKS {
            row["checks"][key] = json!({"status":"pass","observation":"Synthetic fixture has complete legible native text"});
        }
        let binding = row["fidelity"]["evidence"].clone();
        row["fidelity"] = f.clone();
        if binding.is_object() {
            row["fidelity"]["evidence"] = binding;
        }
    }
    for row in obs["measurement_reviews"].as_array_mut().unwrap() {
        row["decision"] = json!("accepted_visual");
        row["basis"] = json!("Synthetic reviewer claims the text is readable");
    }
    for row in obs["relation_reviews"].as_array_mut().unwrap() {
        row["status"] = json!("visual_acceptance");
        row["basis"] = json!("Synthetic fixture has a deliberate slanted standalone label");
    }
    for page in obs["comparisons"].as_array_mut().unwrap() {
        page["inspected"] = json!(true);
        page["note"] = json!("Full-page synthetic comparison checked");
        for row in page["regions"].as_array_mut().unwrap() {
            row["classification"] = json!("acceptable_difference");
            row["note"] =
                json!("A blanket visual pass must not override structured fidelity defects");
        }
        for row in page["label_findings"].as_array_mut().unwrap() {
            row["classification"] = json!("acceptable_difference");
            row["reason_code"] = json!("measurement_artifact");
            row["note"] = json!("Synthetic reference uses the same text renderer");
        }
    }
    obs
}

fn submit_record(
    ctx: &mut Context,
    cycle: &str,
    pack: &str,
    obs: &Value,
    name: &str,
    actor: &str,
) -> Value {
    ctx.write_json(format!("{name}-observations.json"), obs)
        .unwrap();
    ctx.continuation_owner = Some(actor.into());
    ctx.execute(json!({"action":"review_submit","pack":pack,"observations":format!("{name}-observations.json"),"output":format!("{name}-bound.json")})).unwrap();
    ctx.continuation_owner = Some("maker".into());
    ctx.execute(
        json!({"action":"evidence_record","cycle":cycle,"review":format!("{name}-bound.json")}),
    )
    .unwrap()
}

#[test]
fn actual_render_repair_requires_pixel_change_not_a_rewritten_review() {
    let (_dir, mut ctx) = fixture();
    let source_hash = sha_file(&ctx.root.join("original.png")).unwrap();
    let mut art = image::open(ctx.root.join("blank.png")).unwrap().to_rgb8();
    // Introduce a line through the measured glyph area without changing text.
    let inspect = ctx.json("reference/inspection.json").unwrap();
    let b = &inspect["objects"][0]["ink"]["bounds"];
    let y = (b[1].as_f64().unwrap() + b[3].as_f64().unwrap() / 2.) as u32;
    for x in 60..390 {
        for dy in 0..3 {
            art.put_pixel(x, y + dy, Rgb([55, 70, 85]));
        }
    }
    art.save(ctx.root.join("interfering.png")).unwrap();
    let mut source = ctx.json("deck.json").unwrap();
    source["pages"][0]["background"] = json!("interfering.png");
    ctx.replace_json("deck.json", &source).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"before","width":640}))
        .unwrap();
    let before_hash = sha_file(&ctx.root.join("before/editable.pptx")).unwrap();
    std::fs::copy(
        ctx.root.join("before/editable.pptx"),
        ctx.root.join("final.pptx"),
    )
    .unwrap();
    let pack = ctx
        .execute(
            json!({"action":"review_pack","cycle":"before/cycle.json","output":"review-before"}),
        )
        .unwrap();
    let mut f = facts();
    f["current_interference"] = json!("present");
    f["interference_change"] = json!("increased");
    let obs = observations(&ctx, &pack, f);
    let first = submit_record(
        &mut ctx,
        "before/cycle.json",
        "review-before/report.json",
        &obs,
        "first",
        "reviewer",
    );
    assert_eq!(first["reference_fidelity"]["defect_objects"], 1, "{first}");
    assert!(
        first["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "artwork" && c["status"] == "fail")
    );
    assert_eq!(
        first["native_qa"]["independent_visual_acceptances"],
        json!([])
    );
    let registered = ctx.json("first-bound.json").unwrap()["findings"][0].clone();
    assert!(ctx.verify(&registered["registration"]).is_ok());

    let mut rewritten = observations(&ctx, &pack, facts());
    // Drop the defect and change the semantic verdict while pixels stay put.
    let second = submit_record(
        &mut ctx,
        "before/cycle.json",
        "review-before/report.json",
        &rewritten,
        "rewritten",
        "reviewer",
    );
    assert_eq!(second["reference_fidelity"]["defect_objects"], 0);
    assert!(
        second["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["status"] == "fail")
    );
    let mut dismissed = registered.clone();
    dismissed["state"] = json!("dismissed");
    dismissed["arbitration"] = json!({"basis":"Reviewer says it is intentional now"});
    rewritten["findings"] = json!([dismissed]);
    submit_record(
        &mut ctx,
        "before/cycle.json",
        "review-before/report.json",
        &rewritten,
        "dismissed",
        "reviewer",
    );
    let history = ctx.json(".octos/pptx-evidence-history.json").unwrap();
    let id = registered["id"].as_str().unwrap();
    assert_eq!(history["findings"][id]["state"], "open");
    assert_eq!(
        history["findings"][id]["closure_check"]["code"],
        "INDEPENDENT_ARBITRATION_REQUIRED"
    );

    // A separate immutable candidate restores the original background.
    source["pages"][0]["background"] = json!("blank.png");
    ctx.replace_json("deck.json", &source).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"repaired","width":640}))
        .unwrap();
    std::fs::copy(
        ctx.root.join("repaired/editable.pptx"),
        ctx.root.join("final.pptx"),
    )
    .unwrap();
    let pack = ctx
        .execute(
            json!({"action":"review_pack","cycle":"repaired/cycle.json","output":"review-repaired",
        "previous_cycle":"before/cycle.json","previous_review":"first-bound.json"}),
        )
        .unwrap();
    assert_eq!(pack["reused_objects"], 0);
    let mut repaired = observations(&ctx, &pack, facts());
    repaired["findings"] = json!([{"id":id,"state":"resolved","note":"Line removed; original slant, full text and neighboring artwork retained"}]);
    let outcome = submit_record(
        &mut ctx,
        "repaired/cycle.json",
        "review-repaired/report.json",
        &repaired,
        "fixed",
        "reviewer",
    );
    assert_eq!(outcome["reference_fidelity"]["unknown_objects"], 0);
    assert_eq!(outcome["reference_fidelity"]["defect_objects"], 0);
    assert!(
        outcome["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["status"] == "pass"),
        "{outcome}"
    );
    assert_eq!(
        sha_file(&ctx.root.join("original.png")).unwrap(),
        source_hash
    );
    assert_eq!(
        sha_file(&ctx.root.join("before/editable.pptx")).unwrap(),
        before_hash
    );
    let reused = ctx
        .execute(
            json!({"action":"review_pack","cycle":"repaired/cycle.json","output":"review-repeat",
        "previous_cycle":"repaired/cycle.json","previous_review":"fixed-bound.json"}),
        )
        .unwrap();
    assert_eq!(reused["reused_objects"], 1);
}

#[test]
fn false_positive_requires_frozen_third_session_and_current_attestation() {
    let (_dir, mut ctx) = fixture();
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"unchanged","width":640}))
        .unwrap();
    std::fs::copy(
        ctx.root.join("unchanged/editable.pptx"),
        ctx.root.join("final.pptx"),
    )
    .unwrap();
    let pack = ctx
        .execute(json!({"action":"review_pack","cycle":"unchanged/cycle.json","output":"review"}))
        .unwrap();
    let mut f = facts();
    f["layout_relation"] = json!("changed");
    let wrong = observations(&ctx, &pack, f);
    submit_record(
        &mut ctx,
        "unchanged/cycle.json",
        "review/report.json",
        &wrong,
        "incorrect",
        "reviewer",
    );
    let mut corrected = observations(&ctx, &pack, facts());
    let mut finding = ctx.json("incorrect-bound.json").unwrap()["findings"][0].clone();
    finding["state"] = json!("dismissed");
    finding["arbitration"] = json!({"basis":"Original and current pixels are identical; the apparent slant is present in both"});
    corrected["findings"] = json!([finding]);
    let outcome = submit_record(
        &mut ctx,
        "unchanged/cycle.json",
        "review/report.json",
        &corrected,
        "arbitrated",
        "arbiter",
    );
    assert!(
        outcome["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["status"] == "pass"),
        "{outcome}"
    );
    let status = ctx.execute(json!({"action":"evidence_status"})).unwrap();
    assert_eq!(status["reference_fidelity"]["unknown_objects"], 0);
    let binding = ctx.binding("unchanged/cycle.json").unwrap();
    let policy = ctx.json(".octos/continuation.json").unwrap();
    let mut bound = ctx.json("arbitrated-bound.json").unwrap();
    bound["findings"][0]["arbitration"]["basis"] = json!("altered after review");
    assert!(
        octos_pptx::review_routes::arbiter_authorized(&ctx, &policy, &bound, &binding).is_err()
    );
    assert!(
        octos_pptx::review_routes::plan(
            &json!({"verification_mode":"independent_visual",
        "reviewer_agent":"reviewer","arbiter_agent":"session:reviewer"}),
            "maker"
        )
        .is_err()
    );
}

#[test]
fn local_background_permission_does_not_allow_collateral_edits_resizing_or_reference_swap() {
    let (_dir, mut ctx) = fixture();
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"baseline","width":640}))
        .unwrap();
    std::fs::copy(
        ctx.root.join("baseline/editable.pptx"),
        ctx.root.join("final.pptx"),
    )
    .unwrap();
    let pack=ctx.execute(json!({"action":"review_pack","cycle":"baseline/cycle.json","output":"baseline-review"})).unwrap();
    let mut flagged = facts();
    flagged["layout_relation"] = json!("changed");
    let obs = observations(&ctx, &pack, flagged);
    submit_record(
        &mut ctx,
        "baseline/cycle.json",
        "baseline-review/report.json",
        &obs,
        "baseline",
        "reviewer",
    );
    let old = ctx.json(".octos/pptx-evidence-history.json").unwrap();
    let id = old["findings"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    let plan = octos_pptx::review_routes::load_plan(&ctx).unwrap();
    let baseline_contract = old["qa_contract"].clone();
    let base = image::open(ctx.root.join("blank.png")).unwrap().to_rgb8();
    let original = ctx.json("deck.json").unwrap();

    for (name, width, outside, swap_reference) in [
        ("collateral", 640, true, false),
        ("rescaled", 1280, false, false),
        ("reference-swap", 640, false, true),
    ] {
        let mut art = base.clone();
        art.put_pixel(100, 110, Rgb([90, 80, 70]));
        if outside {
            art.put_pixel(620, 15, Rgb([0, 0, 0]));
        }
        let background = format!("{name}-blank.png");
        art.save(ctx.root.join(&background)).unwrap();
        let mut spec = original.clone();
        spec["pages"][0]["background"] = json!(background);
        if swap_reference {
            let mut ref_image = image::open(ctx.root.join("original.png"))
                .unwrap()
                .to_rgb8();
            ref_image.put_pixel(620, 15, Rgb([0, 0, 0]));
            ref_image
                .save(ctx.root.join("different-original.png"))
                .unwrap();
            spec["pages"][0]["design"] = json!("different-original.png");
        }
        ctx.replace_json("deck.json", &spec).unwrap();
        ctx.execute(json!({"action":"cycle","spec":"deck.json","output":name,"width":width}))
            .unwrap();
        let (cycle, spec) =
            octos_pptx::review::bound_cycle(&ctx, &ctx.path(format!("{name}/cycle.json")).unwrap())
                .unwrap();
        let inventory = octos_pptx::object_review::inventory(&cycle, &spec).unwrap();
        let mut object = json!({"page":"example","object_id":"label","evidence_hash":inventory[0]["evidence_hash"],
            "fidelity":facts(),"checks":{}});
        object["fidelity"]["evidence"] = fidelity::evidence(&inventory[0]);
        for k in octos_pptx::object_review::CHECKS {
            object["checks"][k] = json!({"status":"pass","observation":"Claimed good"});
        }
        let review = json!({"object_reviews":[object],"findings":[{"id":id,"state":"resolved"}]});
        let (contract, proofs) =
            fidelity::repair_contract(&ctx, &cycle, &spec, &review, &old, true, &plan).unwrap();
        assert_eq!(
            contract, baseline_contract,
            "{name} must retain frozen constraints"
        );
        let expected = match name {
            "collateral" => "BACKGROUND_CHANGED_OUTSIDE_DEFECT",
            "rescaled" => "REPAIR_RENDER_DIMENSIONS_CHANGED",
            _ => "NO_MATCHING_REGISTERED_BASELINE",
        };
        assert_eq!(proofs[0]["code"], expected, "{proofs:?}");
        let (contract, proofs) =
            fidelity::repair_contract(&ctx, &cycle, &spec, &review, &old, false, &plan).unwrap();
        assert_eq!(contract, baseline_contract);
        assert!(proofs.is_empty());
    }
    // An old frozen task must not gain these new permissions implicitly.
    assert!(!fidelity::enabled(
        &json!({"mode":"independent_visual","reviewer_agent":"reviewer"})
    ));
}

#[test]
fn direct_record_cannot_bypass_independent_fidelity_attestation() {
    let (_dir, mut ctx) = fixture();
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"candidate","width":640}))
        .unwrap();
    std::fs::copy(
        ctx.root.join("candidate/editable.pptx"),
        ctx.root.join("final.pptx"),
    )
    .unwrap();
    let pack = ctx
        .execute(json!({"action":"review_pack","cycle":"candidate/cycle.json","output":"review"}))
        .unwrap();
    let obs = observations(&ctx, &pack, facts());
    let checked = submit_record(
        &mut ctx,
        "candidate/cycle.json",
        "review/report.json",
        &obs,
        "trusted",
        "reviewer",
    );
    assert_eq!(checked["reference_fidelity"]["unknown_objects"], 0);
    let mut forged = ctx.json("trusted-bound.json").unwrap();
    forged.as_object_mut().unwrap().remove("attestation");
    forged
        .as_object_mut()
        .unwrap()
        .remove("measurement_reviews");
    forged.as_object_mut().unwrap().remove("relation_reviews");
    forged.as_object_mut().unwrap().remove("relations");
    ctx.write_json("unattested.json", &forged).unwrap();
    let outcome=ctx.execute(json!({"action":"evidence_record","cycle":"candidate/cycle.json","review":"unattested.json"})).unwrap();
    assert_eq!(outcome["reference_fidelity"]["unknown_objects"], 1);
    assert_eq!(
        outcome["reference_fidelity"]["objects"][0]["code"],
        "INDEPENDENT_FIDELITY_REVIEW_REQUIRED"
    );
    assert!(
        outcome["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["status"] != "pass")
    );
}
