//! Batch plumbing tests use synthetic, explicitly unapproved observations.
use octos_pptx::Context;
use serde_json::json;

#[test]
fn whole_page_view_binds_pixels_and_fits_the_reader() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::new(dir.path()).unwrap();
    let input = ctx.path("reference.png").unwrap();
    let image = image::RgbaImage::from_fn(2048, 1024, |x, _| {
        if x < 1024 {
            image::Rgba([43, 87, 162, 255])
        } else {
            image::Rgba([202, 82, 19, 128])
        }
    });
    image.save(&input).unwrap();
    let request = json!({"action":"review_preview","input":"reference.png","output":"qa/view.png"});
    let result = ctx.execute(request.clone()).unwrap();
    assert_eq!(result["input"], ctx.binding(&input).unwrap());
    assert_eq!(result["dimensions"], json!([1024, 512]));
    assert_eq!(result["model_media"], json!(["qa/view.png"]));
    assert_eq!(result["quality_approved"], false);
    let file = ctx.verify(&result["preview"]).unwrap();
    assert!(std::fs::metadata(&file).unwrap().len() <= 5 * 1024 * 1024);
    let view = image::open(&file).unwrap().to_rgba8();
    for y in 0..view.height() {
        assert_eq!(view.get_pixel(255, y).0, [43, 87, 162, 255]);
        assert_eq!(view.get_pixel(768, y).0, [202, 82, 19, 128]);
    }
    assert_eq!(ctx.execute(request.clone()).unwrap(), result);
    std::fs::write(&file, b"another review").unwrap();
    assert!(ctx.execute(request).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), b"another review");
}

fn fixture() -> (tempfile::TempDir, Context) {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = Context::new(dir.path()).unwrap();
    ctx.continuation_owner = Some("session:maker".into());
    image::RgbImage::from_pixel(320, 180, image::Rgb([250, 250, 250]))
        .save(dir.path().join("blank.png"))
        .unwrap();
    ctx.write_json("spec.json", &json!({"schema_version":1,
        "canvas":{"width_px":320,"height_px":180,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":18}},
        "pages":[
            {"id":"first","background":"blank.png","design":"blank.png",
             "copy":{"a":"Alpha"},"texts":[{"id":"a","text":"Alpha","style":"body","box":[20,30,250,55]}]},
            {"id":"second","background":"blank.png","design":"blank.png",
             "copy":{"b":"Beta"},"texts":[{"id":"b","text":"Beta","style":"body","box":[20,30,250,55]}]}
        ]})).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"spec.json","output":"cycle","width":320}))
        .unwrap();
    ctx.execute(
        json!({"action":"evidence_init","task_id":"batch-fixture","stagnant_attempts":3,
        "verification_mode":"independent_visual","reviewer_agent":"session:reviewer",
        "deliverables":["cycle/editable.pptx"]}),
    )
    .unwrap();
    ctx.continuation_owner = Some("session:reviewer".into());
    (dir, ctx)
}

fn submit_unknown(ctx: &Context, page: &str, prefix: &str) -> String {
    let pack = ctx
        .execute(json!({"action":"review_pack","cycle":"cycle/cycle.json",
        "pages":[page],"output":format!("{prefix}-pack")}))
        .unwrap();
    let mut observations = ctx
        .json(ctx.verify(&pack["observations_template"]).unwrap())
        .unwrap();
    for row in observations["measurement_reviews"].as_array_mut().unwrap() {
        row["basis"] = json!("Synthetic fixture: this item remains unreviewed, not an approval.");
    }
    let authored = format!("{prefix}-authored.json");
    let output = format!("{prefix}-bound.json");
    ctx.write_json(&authored, &observations).unwrap();
    ctx.execute(
        json!({"action":"review_submit","pack":pack["report"]["path"],
        "observations":authored,"output":output}),
    )
    .unwrap();
    output
}

#[test]
fn unscoped_multi_page_pack_returns_a_plan_without_generating_crops() {
    let (dir, ctx) = fixture();
    let result = ctx
        .execute(json!({"action":"review_pack","cycle":"cycle/cycle.json","output":"plan"}))
        .unwrap();
    assert_eq!(result["mode"], "page_batches");
    assert_eq!(result["pack_created"], false);
    assert_eq!(result["batches"].as_array().unwrap().len(), 2);
    assert_eq!(result["batches"][0]["pages"], json!(["first"]));
    assert!(!dir.path().join("plan/object-review-template.json").exists());
    assert!(
        std::fs::read_dir(dir.path().join("plan"))
            .unwrap()
            .all(|p| p.unwrap().path().extension().is_some_and(|e| e == "json"))
    );
}

#[test]
fn selected_page_scopes_comparisons_and_measurements_as_well_as_images() {
    let (_dir, ctx) = fixture();
    let result = ctx
        .execute(json!({"action":"review_pack","cycle":"cycle/cycle.json",
        "pages":["second"],"output":"selected"}))
        .unwrap();
    let template = ctx
        .json(ctx.verify(&result["observations_template"]).unwrap())
        .unwrap();
    assert_eq!(template["comparisons"].as_array().unwrap().len(), 1);
    assert_eq!(template["comparisons"][0]["page"], "second");
    assert!(
        template["measurement_reviews"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["page"] == "second" || r["page"] == 2)
    );
}

#[test]
fn page_pack_defers_unselected_comparison_validation_to_whole_deck_recording() {
    let (_dir, ctx) = fixture();
    let cycle = ctx.json("cycle/cycle.json").unwrap();
    let manifest = ctx
        .json(ctx.verify(&cycle["comparisons"]).unwrap())
        .unwrap();
    let path = ctx.verify(&manifest["pages"][0]["report"]).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.push(b' ');
    std::fs::write(path, bytes).unwrap();
    assert!(
        ctx.execute(json!({"action":"review_pack","cycle":"cycle/cycle.json",
        "pages":["second"],"output":"selected-with-unrelated-stale-report"}))
            .is_ok()
    );
    submit_unknown(&ctx, "second", "scoped-submit");
    // This is only a page batch, never permission to omit final validation.
    ctx.execute(json!({"action":"review_merge","cycle":"cycle/cycle.json",
        "reviews":[],"output":"empty.json"}))
        .unwrap_err();
    assert!(
        octos_pptx::comparison_review::assess(
            &ctx,
            &cycle,
            &serde_json::from_value(ctx.json(ctx.verify(&cycle["spec"]).unwrap()).unwrap())
                .unwrap(),
            &json!({}),
            &json!({}),
            None
        )
        .is_err()
    );
}

#[test]
fn bound_page_reviews_merge_without_manufacturing_passes() {
    let (_dir, mut ctx) = fixture();
    let a = submit_unknown(&ctx, "first", "a");
    let b = submit_unknown(&ctx, "second", "b");
    ctx.continuation_owner = Some("session:maker".into());
    let result = ctx
        .execute(json!({"action":"review_merge","cycle":"cycle/cycle.json",
        "reviews":[a,b],"output":"merged.json"}))
        .unwrap();
    let review = ctx.json(ctx.verify(&result["review"]).unwrap()).unwrap();
    assert_eq!(review["object_reviews"].as_array().unwrap().len(), 2);
    assert_eq!(review["comparisons"].as_array().unwrap().len(), 2);
    assert_eq!(result["quality_approved"], false);
    assert!(
        octos_pptx::review_routes::authorized(
            &ctx,
            &ctx.json(".octos/continuation.json").unwrap(),
            &review,
            &ctx.binding("cycle/cycle.json").unwrap()
        )
        .unwrap()
    );
    // The merged body is derived from source receipts, not authored by Maker.
    let mut fabricated = review.clone();
    fabricated["object_reviews"][0]["checks"]["ownership"]["status"] = json!("pass");
    assert!(
        octos_pptx::review_routes::authorized(
            &ctx,
            &ctx.json(".octos/continuation.json").unwrap(),
            &fabricated,
            &ctx.binding("cycle/cycle.json").unwrap()
        )
        .is_err()
    );
    assert!(
        ctx.execute(json!({"action":"review_merge","cycle":"cycle/cycle.json",
        "reviews":["merged.json"],"output":"nested.json"}))
            .is_err()
    );
    let recorded = ctx
        .execute(
            json!({"action":"evidence_record","cycle":"cycle/cycle.json","review":"merged.json"}),
        )
        .unwrap();
    assert!(
        recorded["object_coverage"]["unknown_objects"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        recorded["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["status"] != "pass")
    );
    let evidence = ctx.json(".octos/iteration-evidence.json").unwrap();
    assert!(
        evidence["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["path"] == "a-bound.json")
    );
    // The native API exposes a consumed durable handoff to an outer supervisor.
    let mut state = json!({"attempt":recorded["attempt"],"flat_attempts":0,"warned":false,
        "policy_digest":ctx.binding(".octos/continuation.json").unwrap()["sha256"],
        "snapshot_digest":ctx.binding(".octos/iteration-evidence.json").unwrap()["sha256"],
        "completion_gap_attempts":3,"completion_blocked":true});
    ctx.write_json(".octos/continuation-state.json", &state)
        .unwrap();
    let status = ctx.execute(json!({"action":"evidence_status"})).unwrap();
    assert_eq!(status["runtime_consumed_current_evidence"], true);
    assert_eq!(status["completion_blocked"], true);
    assert_eq!(status["handoff"]["automatic_retry_allowed"], false);
    state["snapshot_digest"] = json!("stale");
    ctx.replace_json(".octos/continuation-state.json", &state)
        .unwrap();
    let stale = ctx.execute(json!({"action":"evidence_status"})).unwrap();
    assert_eq!(stale["runtime_consumed_current_evidence"], false);
    assert!(stale.get("completion_blocked").is_none());
    // A numeric verdict is never a replacement for bound review evidence.
    ctx.write_json("score-only.json", &json!({"accepted":true,"score":100}))
        .unwrap();
    assert!(
        ctx.execute(json!({"action":"review_merge","cycle":"cycle/cycle.json",
        "reviews":["score-only.json"],"output":"forged.json"}))
            .is_err()
    );
    // Tampering with a previously attested review invalidates the merge.
    let mut altered = ctx.json("a-bound.json").unwrap();
    altered["object_reviews"][0]["checks"]["ownership"]["status"] = json!("pass");
    std::fs::write(
        ctx.path("a-bound.json").unwrap(),
        serde_json::to_vec(&altered).unwrap(),
    )
    .unwrap();
    assert!(
        ctx.execute(json!({"action":"review_merge","cycle":"cycle/cycle.json",
        "reviews":["a-bound.json","b-bound.json"],"output":"changed.json"}))
            .is_err()
    );
}

#[test]
fn merging_rejects_conflicts_wrong_reviewers_and_stale_cycles() {
    let (_dir, mut ctx) = fixture();
    submit_unknown(&ctx, "first", "a");
    let mut other = ctx.json("a-bound.json").unwrap();
    other["object_reviews"][0]["checks"]["ownership"]["observation"] =
        json!("A different explicitly unknown synthetic observation");
    let cycle = ctx.binding("cycle/cycle.json").unwrap();
    octos_pptx::review_routes::attest(&ctx, &cycle, &mut other).unwrap();
    ctx.write_json("conflict.json", &other).unwrap();
    let merge = |ctx: &Context, paths: &[&str], output: &str| {
        ctx.execute(json!({
        "action":"review_merge","cycle":"cycle/cycle.json","reviews":paths,"output":output}))
    };
    assert!(
        merge(
            &ctx,
            &["a-bound.json", "conflict.json"],
            "bad-conflict.json"
        )
        .unwrap_err()
        .to_string()
        .contains("conflicting")
    );
    assert!(merge(&ctx, &["a-bound.json", "a-bound.json"], "dedup.json").is_ok());
    // Even a valid native receipt from Maker cannot become independent review.
    ctx.continuation_owner = Some("session:maker".into());
    octos_pptx::review_routes::attest(&ctx, &cycle, &mut other).unwrap();
    ctx.write_json("wrong-reviewer.json", &other).unwrap();
    assert!(merge(&ctx, &["wrong-reviewer.json"], "bad-actor.json").is_err());
    ctx.continuation_owner = Some("session:reviewer".into());
    octos_pptx::review_routes::attest(&ctx, &ctx.binding("spec.json").unwrap(), &mut other)
        .unwrap();
    ctx.write_json("wrong-cycle.json", &other).unwrap();
    assert!(merge(&ctx, &["wrong-cycle.json"], "bad-cycle.json").is_err());
}

#[test]
fn a_page_batch_reuses_only_its_own_prior_comparisons() {
    let (_dir, ctx) = fixture();
    submit_unknown(&ctx, "first", "first");
    submit_unknown(&ctx, "second", "second");
    ctx.execute(json!({"action":"review_merge","cycle":"cycle/cycle.json",
        "reviews":["first-bound.json","second-bound.json"],"output":"prior.json"}))
        .unwrap();
    let pack = ctx
        .execute(json!({"action":"review_pack","cycle":"cycle/cycle.json",
        "previous_cycle":"cycle/cycle.json","previous_review":"prior.json",
        "pages":["second"],"output":"reused-second"}))
        .unwrap();
    let report = ctx.json(ctx.verify(&pack["report"]).unwrap()).unwrap();
    assert_eq!(report["reuse"]["comparisons"].as_array().unwrap().len(), 1);
    assert_eq!(report["reuse"]["comparisons"][0]["page"], "second");
    let mut observations = ctx
        .json(ctx.verify(&pack["observations_template"]).unwrap())
        .unwrap();
    observations["measurement_reviews"] = json!([]); // Deliberately unreviewed.
    ctx.write_json("reused-observations.json", &observations)
        .unwrap();
    ctx.execute(
        json!({"action":"review_submit","pack":pack["report"]["path"],
        "observations":"reused-observations.json","output":"reused-bound.json"}),
    )
    .unwrap();
}
