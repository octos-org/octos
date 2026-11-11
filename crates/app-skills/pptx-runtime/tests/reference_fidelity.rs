use image::{Rgb, RgbImage};
use octos_pptx::{Context, fs::sha_file};
use serde_json::{Value, json};

fn facts() -> Value {
    json!({"comparison_reliable":true,"layout_relation":"preserved",
        "reference_interference":"absent","current_interference":"absent",
        "interference_change":"equivalent","basis":"Synthetic original/current pixel comparison"})
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
