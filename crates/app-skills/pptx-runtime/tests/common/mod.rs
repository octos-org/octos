//! Shared synthetic PPTX fixtures and explicit judgments, never production approvals.
#![allow(dead_code)] // Integration targets consume different helper subsets.
use image::{Rgb, RgbImage};
use octos_pptx::{Context, fs::sha_file, object_review, review};
use serde_json::{Value, json};

pub fn bordered(x: u32, y: u32, w: u32, h: u32) -> RgbImage {
    RgbImage::from_fn(640, 360, move |px, py| {
        let horizontal = (y..=y + 2).contains(&py) || (y + h - 2..=y + h).contains(&py);
        let vertical = (x..=x + 2).contains(&px) || (x + w - 2..=x + w).contains(&px);
        if (x..=x + w).contains(&px) && horizontal || (y..=y + h).contains(&py) && vertical {
            Rgb([30, 60, 45])
        } else {
            Rgb([245, 240, 225])
        }
    })
}

pub fn labeled_fixture(task_id: &str, page_id: &str, owner: &str) -> (tempfile::TempDir, Context) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::new(dir.path()).unwrap();
    bordered(40, 40, 360, 160)
        .save(ctx.root.join("blank.png"))
        .unwrap();
    let mut spec: Value =
        serde_json::from_str(include_str!("../fixtures/labeled-deck.json")).unwrap();
    spec["pages"][0]["id"] = json!(page_id);
    ctx.write_json("deck.json", &spec).unwrap();
    ctx.execute(json!({"action":"evidence_init","task_id":task_id,"owner_agent":owner,"stagnant_attempts":3,"deliverables":["final.pptx"]})).unwrap();
    (dir, ctx)
}

pub fn copy(ctx: &Context, from: &str, to: &str) {
    std::fs::copy(ctx.root.join(from), ctx.root.join(to)).unwrap();
}

pub fn cycle(ctx: &Context, name: &str) {
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":name,"width":640}))
        .unwrap();
    copy(ctx, &format!("{name}/editable.pptx"), "final.pptx");
}

pub fn edit_spec(ctx: &Context, f: impl FnOnce(&mut Value)) {
    let mut spec = ctx.json("deck.json").unwrap();
    f(&mut spec);
    ctx.replace_json("deck.json", &spec).unwrap();
}

pub fn record(
    ctx: &Context,
    name: &str,
    comparisons: Value,
    manifest: Option<&str>,
    findings: Value,
    native_qa_pass: bool,
) -> anyhow::Result<Value> {
    let receipt = ctx.json(format!("{name}/cycle.json")).unwrap();
    let pages = receipt["pages"].clone();
    let mut review = json!({"cycle_sha256":sha_file(&ctx.root.join(name).join("cycle.json")).unwrap(),
        "inspected":{"copy":pages,"artwork":pages,"readability":pages},"findings":findings,
        "object_reviews":object_judgments(ctx,name),"comparisons":comparisons});
    if native_qa_pass {
        review["qa"] = json!({"status":"pass","findings":[]});
        review["note"] = json!("Everything is fine; all overlap is intentional");
    }
    ctx.replace_json("review.json", &review).unwrap();
    let mut action = json!({"action":"evidence_record","cycle":format!("{name}/cycle.json"),"review":"review.json"});
    if let Some(manifest) = manifest {
        action["comparisons"] = json!(manifest);
    }
    ctx.execute(action)
}

// These are synthetic fixture judgments, not production visual approvals.
pub fn accept(ctx: &Context, manifest: impl AsRef<std::path::Path>) -> Value {
    let manifest = ctx.json(manifest).unwrap();
    json!(manifest["pages"].as_array().unwrap().iter().filter(|p|p["status"]=="ready").map(|p| {
        let report = ctx.json(ctx.verify(&p["report"]).unwrap()).unwrap();
        let ids:Vec<_> = report["regions"].as_array().unwrap().iter().map(|r|r["id"].clone()).collect();
        let labels:Vec<_> = report["label_evidence"]["findings"].as_array().unwrap().iter().map(|f|
            json!({"id":f["id"],"evidence_sha256":f["evidence"]["sha256"],"classification":"acceptable_difference",
                "reason_code":"intentional_change","note":"Accept this synthetic fixture difference; alignment is judged separately"})).collect();
        json!({"page":p["page"],"comparison_key":p["comparison_key"],"inspected":true,
            "label_findings":labels,"note":"Inspect whole-page and original views",
            "regions":if ids.is_empty(){json!([])}else{json!([{"ids":ids,"classification":"acceptable_difference","note":"Accept every generated region difference"}])}})
    }).collect::<Vec<_>>())
}

pub fn facts() -> Value {
    serde_json::from_str(include_str!("../fixtures/fidelity-facts.json")).unwrap()
}

pub fn object_judgments(ctx: &Context, name: &str) -> Value {
    let (cycle, spec) =
        review::bound_cycle(ctx, &ctx.path(format!("{name}/cycle.json")).unwrap()).unwrap();
    json!(object_review::inventory(&cycle,&spec).unwrap().iter().map(|v| {
        let checks:serde_json::Map<String,Value>=object_review::CHECKS.iter().map(|k|(k.to_string(),json!({"status":"pass","observation":"Synthetic fixture has an explicit expected arrangement; this is test data, not a production visual approval."}))).collect();
        let mut row=json!({"page":v["page"],"object_id":v["object_id"],"evidence_hash":v["evidence_hash"],"checks":checks});
        if v["design_available"]==true {
            let mut fidelity = facts();
            fidelity["evidence"] = octos_pptx::fidelity::evidence(v);
            row["fidelity"] = fidelity;
        }
        row
    }).collect::<Vec<_>>())
}
