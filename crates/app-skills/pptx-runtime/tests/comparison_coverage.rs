use octos_pptx::{Context, fs::sha_file};
use serde_json::{Value, json};
mod common;

fn fixture(root: &std::path::Path, pages: usize) -> Context {
    let ctx = Context::new(root).unwrap();
    std::fs::create_dir_all(root.join("outputs")).unwrap();
    let mut art = image::RgbImage::from_pixel(640, 360, image::Rgb([255, 255, 255]));
    for x in 40..=600 {
        for dy in 0..3 {
            art.put_pixel(x, 40 + dy, image::Rgb([20, 20, 20]));
            art.put_pixel(x, 300 - dy, image::Rgb([20, 20, 20]));
        }
    }
    for y in 40..=300 {
        for dx in 0..3 {
            art.put_pixel(40 + dx, y, image::Rgb([20, 20, 20]));
            art.put_pixel(600 - dx, y, image::Rgb([20, 20, 20]));
        }
    }
    art.save(root.join("blank.png")).unwrap();
    let pages:Vec<_> = (0..pages).map(|i|json!({"id":format!("page-{i}"),"background":"blank.png","design":"blank.png",
        "copy":{"label":"Synthetic label"},
        "regions":{"area":{"quad":[[40,40],[600,40],[600,300],[40,300]],"scope":"label","basis":"Independent synthetic source area"}},
        "texts":[{"id":"label","text":"Synthetic label","style":"body","box":[50,50,260,80],"region":"area"}]})).collect();
    ctx.write_json(
        "deck.json",
        &json!({"schema_version":1,
        "canvas":{"width_px":640,"height_px":360,"width_in":12.8,"height_in":7.2},
        "styles":{"body":{"font":"Inter","size_pt":18}},"pages":pages}),
    )
    .unwrap();
    ctx.execute(json!({"action":"evidence_init","task_id":"synthetic-coverage","owner_agent":"test","stagnant_attempts":3,"deliverables":["outputs/final.pptx"]})).unwrap();
    ctx
}

fn cycle(ctx: &Context, name: &str, reuse: Option<&str>) -> Value {
    let out = ctx.execute(json!({"action":"cycle","spec":"deck.json","output":name,"width":640,"reuse_comparisons":reuse})).unwrap();
    std::fs::copy(
        ctx.root.join(name).join("editable.pptx"),
        ctx.root.join("outputs/final.pptx"),
    )
    .unwrap();
    out
}

// These are synthetic fixture judgments, not production visual approvals.
fn judgments(ctx: &Context, manifest: &str) -> Value {
    let manifest = ctx.json(manifest).unwrap();
    json!(manifest["pages"].as_array().unwrap().iter().filter(|p|p["status"]=="ready").map(|p| {
        let report = ctx.json(ctx.verify(&p["report"]).unwrap()).unwrap();
        let ids:Vec<_> = report["regions"].as_array().unwrap().iter().map(|r|r["id"].clone()).collect();
        let labels:Vec<_> = report["label_evidence"]["findings"].as_array().unwrap().iter().map(|f|
            json!({"id":f["id"],"evidence_sha256":f["evidence"]["sha256"],"classification":"acceptable_difference",
                "reason_code":"intentional_change","note":"Synthetic reference intentionally has no label; context confirms the added fixture text"})).collect();
        json!({"page":p["page"],"comparison_key":p["comparison_key"],"inspected":true,
            "label_findings":labels,
            "note":"Synthetic fixture: inspect whole-page and original views",
            "regions":if ids.is_empty(){json!([])}else{json!([{"ids":ids,"classification":"acceptable_difference","note":"Synthetic reference has no label; fixture intentionally adds one"}])}})
    }).collect::<Vec<_>>())
}

fn review(ctx: &Context, name: &str) -> Value {
    let receipt = ctx.json(format!("{name}/cycle.json")).unwrap();
    let pages = receipt["pages"].clone();
    json!({"cycle_sha256":sha_file(&ctx.root.join(name).join("cycle.json")).unwrap(),
        "inspected":{"copy":pages,"artwork":pages,"readability":pages},"findings":[],
        "object_reviews":common::object_judgments(ctx,name)})
}

fn record(ctx: &Context, name: &str, review: &Value) -> anyhow::Result<Value> {
    ctx.replace_json("review.json", review)?;
    ctx.execute(json!({"action":"evidence_record","cycle":format!("{name}/cycle.json"),"review":"review.json"}))
}

#[test]
fn claimed_page_coverage_cannot_skip_regions_or_hide_defects() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path(), 2);
    cycle(&ctx, "a", None);
    let manifest = ctx.json("a/comparisons/manifest.json").unwrap();
    let report = ctx
        .json(ctx.verify(&manifest["pages"][0]["report"]).unwrap())
        .unwrap();
    assert_eq!(
        report["label_evidence"]["labels"][0]["metrics"]["candidate_ink_method"],
        "isolated_native_render"
    );
    assert_eq!(manifest["pages"].as_array().unwrap().len(), 2);
    assert!(
        manifest["pages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["status"] == "ready")
    );
    let mut r = review(&ctx, "a");
    let missing = record(&ctx, "a", &r).unwrap();
    assert_eq!(missing["checks"][1]["status"], "pass");
    assert_eq!(missing["checks"][2]["status"], "unknown");
    assert_eq!(missing["comparison_coverage"]["unknown"], 2);
    let status = ctx
        .execute(json!({"action":"evidence_status","owner_agent":"test"}))
        .unwrap();
    assert_eq!(status["comparison_coverage"]["unknown"], 2);
    assert!(status["native_qa"]["report"].is_object());
    assert_eq!(status["delivery_bindings_current"], true);
    let snapshot = ctx.json(".octos/iteration-evidence.json").unwrap();
    assert!(
        snapshot.get("comparison_coverage").is_none(),
        "PPTX details must not extend the kernel's strict snapshot schema"
    );
    assert!(snapshot["artifacts"].as_array().unwrap().iter().any(|a| {
        std::path::Path::new(a["path"].as_str().unwrap())
            .starts_with(".octos/pptx-comparison-records")
    }));
    let all = judgments(&ctx, "a/comparisons/manifest.json");
    r["comparisons"] = json!([all[0]]);
    assert_eq!(
        record(&ctx, "a", &r).unwrap()["comparison_coverage"]["unknown"],
        1
    );
    r["comparisons"] = all.clone();
    r["comparisons"][1]["regions"] = json!([]);
    assert_eq!(
        record(&ctx, "a", &r).unwrap()["checks"][3]["status"],
        "unknown"
    );
    r["comparisons"] = all.clone();
    r["comparisons"][1]["regions"][0]["classification"] = json!("defect");
    let failed = record(&ctx, "a", &r).unwrap();
    assert_eq!(failed["checks"][2]["status"], "fail");
    // Omitting a previously reported defect cannot clear it.
    r["comparisons"] = json!([all[0]]);
    assert_eq!(
        record(&ctx, "a", &r).unwrap()["checks"][3]["status"],
        "fail"
    );
    r["comparisons"] = all.clone();
    r["comparisons"][1]["regions"][0]["classification"] = json!("unresolved");
    assert_eq!(
        record(&ctx, "a", &r).unwrap()["checks"][3]["status"],
        "unknown"
    );
    r["comparisons"] = all.clone();
    r["comparisons"][0]["label_findings"] = json!([]);
    let missing_labels = record(&ctx, "a", &r).unwrap();
    assert_eq!(
        missing_labels["checks"][3]["status"], "unknown",
        "page-region dispositions cannot bypass label findings"
    );
    r["comparisons"] = all.clone();
    r["comparisons"][0]["label_findings"][0]["reason_code"] = json!("font_metrics");
    assert!(
        record(&ctx, "a", &r)
            .unwrap_err()
            .to_string()
            .contains("font metrics cannot dismiss")
    );
    r["comparisons"] = all.clone();
    r["comparisons"][0]["label_findings"][0]["evidence_sha256"] = json!("stale");
    assert!(record(&ctx, "a", &r).is_err());
    r["comparisons"] = all.clone();
    let passed = record(&ctx, "a", &r).unwrap();
    assert!(
        passed["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["status"] == "pass")
    );
    assert_eq!(record(&ctx, "a", &r).unwrap()["attempt"], passed["attempt"]);

    let saved = sha_file(&ctx.root.join(".octos/iteration-evidence.json")).unwrap();
    r["comparisons"][0]["comparison_key"] = json!("stale");
    assert!(
        record(&ctx, "a", &r)
            .unwrap_err()
            .to_string()
            .contains("stale comparison review")
    );
    assert_eq!(
        sha_file(&ctx.root.join(".octos/iteration-evidence.json")).unwrap(),
        saved
    );
    r["comparisons"] = all.clone();
    r["comparisons"][0]["regions"][0]["ids"] = json!(["not-a-region"]);
    assert!(record(&ctx, "a", &r).is_err());
    r["comparisons"] = all.clone();
    let duplicate = r["comparisons"][0]["regions"][0].clone();
    r["comparisons"][0]["regions"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert!(record(&ctx, "a", &r).is_err());
    r["comparisons"] = all;
    r["comparisons"][0]["note"] = json!(" ");
    assert!(record(&ctx, "a", &r).is_err());
}

#[test]
fn changed_page_invalidates_only_its_review_and_reference_removal_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path(), 2);
    cycle(&ctx, "a", None);
    let mut r = review(&ctx, "a");
    r["comparisons"] = judgments(&ctx, "a/comparisons/manifest.json");
    record(&ctx, "a", &r).unwrap();
    let old = ctx.json("a/comparisons/manifest.json").unwrap();
    let mut spec = ctx.json("deck.json").unwrap();
    spec["pages"][0]["texts"][0]["box"][0] = json!(260);
    ctx.replace_json("deck.json", &spec).unwrap();
    cycle(&ctx, "b", Some("a/comparisons/manifest.json"));
    let fresh = ctx.json("b/comparisons/manifest.json").unwrap();
    assert_eq!(fresh["pages"][0]["reused"], false);
    assert_eq!(fresh["pages"][1]["reused"], true);
    assert_eq!(fresh["pages"][1]["report"], old["pages"][1]["report"]);
    let mut next = review(&ctx, "b");
    let pending = record(&ctx, "b", &next).unwrap();
    assert_eq!(pending["comparison_coverage"]["unknown"], 1);
    assert_eq!(
        pending["comparison_coverage"]["pages"][0]["status"],
        "unreviewed"
    );
    assert_eq!(
        pending["comparison_coverage"]["pages"][1]["reused_review"],
        true
    );
    next["comparisons"] = json!([judgments(&ctx, "b/comparisons/manifest.json")[0]]);
    assert!(
        record(&ctx, "b", &next).unwrap()["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["status"] == "pass")
    );
    spec["pages"][1].as_object_mut().unwrap().remove("design");
    ctx.replace_json("deck.json", &spec).unwrap();
    cycle(&ctx, "c", None);
    assert!(
        record(&ctx, "c", &review(&ctx, "c"))
            .unwrap_err()
            .to_string()
            .contains("reference was removed")
    );
}

#[test]
fn legacy_cycles_need_real_reports_and_no_reference_decks_do_not_invent_them() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path(), 1);
    cycle(&ctx, "a", None);
    let mut receipt = ctx.json("a/cycle.json").unwrap();
    receipt.as_object_mut().unwrap().remove("comparisons");
    // Emulate an old cycle format. Never mutate a production receipt.
    ctx.replace_json("a/cycle.json", &receipt).unwrap();
    let mut r = review(&ctx, "a");
    assert_eq!(
        record(&ctx, "a", &r).unwrap()["comparison_coverage"]["pages"][0]["status"],
        "missing_comparison"
    );
    ctx.execute(json!({"action":"compare_cycle","cycle":"a/cycle.json","output":"retrofit"}))
        .unwrap();
    r["comparisons"] = judgments(&ctx, "retrofit/manifest.json");
    ctx.replace_json("review.json", &r).unwrap();
    let result = ctx.execute(json!({"action":"evidence_record","cycle":"a/cycle.json","review":"review.json","comparisons":"retrofit/manifest.json"})).unwrap();
    assert_eq!(result["checks"][3]["status"], "pass");
    let report = ctx.json("retrofit/manifest.json").unwrap()["pages"][0]["report"].clone();
    std::fs::write(ctx.verify(&report).unwrap(), b"changed report").unwrap();
    assert!(ctx.execute(json!({"action":"evidence_record","cycle":"a/cycle.json","review":"review.json","comparisons":"retrofit/manifest.json"})).is_err());

    let dir2 = tempfile::tempdir().unwrap();
    let ctx = fixture(dir2.path(), 1);
    let mut spec = ctx.json("deck.json").unwrap();
    spec["pages"][0].as_object_mut().unwrap().remove("design");
    ctx.replace_json("deck.json", &spec).unwrap();
    cycle(&ctx, "native", None);
    assert_eq!(
        ctx.json("native/comparisons/manifest.json").unwrap()["pages"][0]["status"],
        "no_reference"
    );
    assert_eq!(
        record(&ctx, "native", &review(&ctx, "native")).unwrap()["checks"][3]["status"],
        "pass"
    );
}

#[test]
fn zero_difference_still_needs_review_and_four_k_analysis_is_not_downsampled() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path(), 1);
    // This fixture specifically needs identical raster pixels, without edge
    // resampling at the PPTX image corners.
    image::RgbImage::from_pixel(640, 360, image::Rgb([255, 255, 255]))
        .save(ctx.root.join("blank.png"))
        .unwrap();
    let mut spec = ctx.json("deck.json").unwrap();
    spec["pages"][0]["texts"] = json!([]);
    spec["pages"][0]["copy"] = json!({});
    ctx.replace_json("deck.json", &spec).unwrap();
    cycle(&ctx, "a", None);
    let report = ctx.json("a/comparisons/page-001.json").unwrap();
    assert_eq!(report["regions"], json!([]));
    let mut r = review(&ctx, "a");
    assert_eq!(
        record(&ctx, "a", &r).unwrap()["checks"][3]["status"],
        "unknown"
    );
    r["comparisons"] = judgments(&ctx, "a/comparisons/manifest.json");
    assert_eq!(
        record(&ctx, "a", &r).unwrap()["checks"][3]["status"],
        "pass"
    );

    image::RgbImage::from_pixel(3840, 2160, image::Rgb([255, 255, 255]))
        .save(ctx.root.join("4k.png"))
        .unwrap();
    let report = ctx.execute(json!({"action":"compare","before":"4k.png","background":"4k.png","after":"4k.png","output":"4k-proof.png"})).unwrap();
    assert_eq!(report["normalized_to"], json!([3840, 2160]));
    assert_eq!(
        image::image_dimensions(ctx.verify(&report["views"]["difference"]).unwrap()).unwrap(),
        (3840, 2160)
    );
    let size = image::image_dimensions(ctx.verify(&report["panel"]).unwrap()).unwrap();
    assert!(size.0 * size.1 <= 6_000_000);
}
