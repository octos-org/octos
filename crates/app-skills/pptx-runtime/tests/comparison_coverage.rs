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
    ctx.replace_json("review.json", review).unwrap();
    ctx.execute(json!({"action":"evidence_record","cycle":format!("{name}/cycle.json"),"review":"review.json"}))
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
