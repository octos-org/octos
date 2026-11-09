use octos_pptx::{Context, fs::sha_file};
use serde_json::{Value, json};

fn fixture() -> (tempfile::TempDir, Context) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::new(dir.path()).unwrap();
    let mut art = image::RgbImage::from_pixel(640, 360, image::Rgb([248, 244, 232]));
    // A slanted rule rather than a hard-coded production slide or coordinate.
    for x in 30..600 {
        let y = 170 + x / 20;
        for dy in 0..3 {
            art.put_pixel(x, y + dy, image::Rgb([180, 40, 90]));
        }
    }
    art.save(dir.path().join("art.png")).unwrap();
    ctx.write_json("source.json",&json!({"schema_version":1,
        "canvas":{"width_px":640,"height_px":360,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":16,"wrap":true}},
        "pages":[{"id":"scene","background":"art.png","copy":{"caption":"A reusable caption with several words"},
        "texts":[{"id":"caption","text":"A reusable caption with several words","style":"body","box":[40,174,500,50]}]}]})).unwrap();
    (dir, ctx)
}
fn patch(ctx: &Context, b: Value) {
    let mut s = ctx.json("source.json").unwrap();
    s["pages"][0]["texts"][0]["box"] = b;
    ctx.replace_json("source.json", &s).unwrap();
}
fn cycle(ctx: &Context, name: &str) -> Value {
    ctx.execute(json!({"action":"cycle","spec":"source.json","output":name,"width":640}))
        .unwrap();
    ctx.execute(json!({"action":"qa_check","cycle":format!("{name}/cycle.json")}))
        .unwrap()
}
fn placement_failed(q: &Value) -> bool {
    q["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["code"] == "PLACEMENT_CONSTRAINT_FAILED")
}

#[test]
fn reflow_uses_target_width_and_does_not_edit_source() {
    let (dir, ctx) = fixture();
    let before = sha_file(&dir.path().join("source.json")).unwrap();
    let request = json!({"action":"fit","spec":"source.json","objects":["scene/caption"],"ink_align":"top",
        "targets":[{"page":"scene","id":"caption","box":[40,30,150,260]}],"reflow":true});
    let fit = ctx.execute(request.clone()).unwrap();
    assert!(fit["proposals"][0]["lines"].as_u64().unwrap() > 1, "{fit}");
    assert_eq!(
        fit["proposals"][0]["ink_alignment"]["status"], "proposed",
        "{fit}"
    );
    assert_eq!(fit["proposals"][0]["proposed_size_pt"], 16.);
    let mut old = request;
    old["reflow"] = json!(false);
    assert_eq!(ctx.execute(old).unwrap()["proposals"][0]["lines"], 1);
    assert_eq!(before, sha_file(&dir.path().join("source.json")).unwrap());
}

#[test]
fn search_avoids_slanted_artwork_and_qa_rejects_regression_and_relaxation() {
    let (_dir, ctx) = fixture();
    let search = ctx
        .execute(
            json!({"action":"layout_search","spec":"source.json","page":"scene","id":"caption",
        "area":[25,30,580,290],"register":true,"output":"search"}),
        )
        .unwrap();
    assert!(
        !search["proposals"].as_array().unwrap().is_empty(),
        "{search}"
    );
    let components = search["artwork_components"]["regions"].as_array().unwrap();
    assert!(!components.is_empty());
    let b = &components[0]["box"];
    assert!(
        b[0].as_f64().unwrap() < 40.
            && b[1].as_f64().unwrap() > 160.
            && b[2].as_f64().unwrap() > 550.,
        "{components:?}"
    );
    assert!(ctx.verify(&search["obstacles_preview"]).unwrap().is_file());
    assert!(
        placement_failed(&cycle(&ctx, "before")),
        "original text crosses the diagonal rule"
    );
    patch(&ctx, search["proposals"][0]["set"]["box"].clone());
    assert!(
        !placement_failed(&cycle(&ctx, "after")),
        "candidate must be measured again after PPTX export"
    );
    patch(&ctx, json!([40, 174, 500, 50]));
    assert!(placement_failed(&cycle(&ctx, "regressed")));
    assert!(
        ctx.execute(
            json!({"action":"layout_search","spec":"source.json","page":"scene","id":"caption",
        "area":[0,0,640,360],"output":"relax"})
        )
        .is_err()
    );
    assert!(
        ctx.execute(
            json!({"action":"layout_search","spec":"source.json","page":"scene","id":"caption",
        "avoid_artwork":false,"output":"ignore"})
        )
        .is_err()
    );
    assert_eq!(search["quality_approved"], false);
}

#[test]
fn no_fit_does_not_shrink_delete_or_claim_every_layout_impossible() {
    let (dir, ctx) = fixture();
    let before = sha_file(&dir.path().join("source.json")).unwrap();
    let result = ctx
        .execute(
            json!({"action":"layout_search","spec":"source.json","page":"scene","id":"caption",
        "area":[20,20,30,30],"output":"small"}),
        )
        .unwrap();
    assert_eq!(result["status"], "no_candidate_found");
    assert_eq!(result["quality_approved"], false);
    assert_eq!(before, sha_file(&dir.path().join("source.json")).unwrap());
}

#[test]
fn search_respects_explicit_illustration_exclusion_and_native_neighbor() {
    let (_dir, ctx) = fixture();
    let mut s = ctx.json("source.json").unwrap();
    s["pages"][0]["copy"]["neighbor"] = json!("Neighbor heading");
    s["pages"][0]["texts"].as_array_mut().unwrap().push(
        json!({"id":"neighbor","text":"Neighbor heading","style":"body","box":[340,35,240,40]}),
    );
    ctx.replace_json("source.json", &s).unwrap();
    let result = ctx
        .execute(
            json!({"action":"layout_search","spec":"source.json","page":"scene","id":"caption",
        "area":[25,20,580,300],"forbidden":[[25,20,280,300]],"register":true,"output":"excluded"}),
        )
        .unwrap();
    assert!(
        !result["proposals"].as_array().unwrap().is_empty(),
        "{result}"
    );
    patch(&ctx, result["proposals"][0]["set"]["box"].clone());
    assert!(!placement_failed(&cycle(&ctx, "good")));
    patch(&ctx, json!([40, 30, 240, 110]));
    assert!(placement_failed(&cycle(&ctx, "illustration")));
    patch(&ctx, json!([340, 35, 240, 100]));
    assert!(placement_failed(&cycle(&ctx, "neighbor")));
}

#[test]
fn registered_placement_failure_cannot_be_closed_by_prose_or_deleting_object() {
    let (_dir, ctx) = fixture();
    ctx.execute(json!({"action":"evidence_init","task_id":"placement-test","owner_agent":"maker","stagnant_attempts":3,"deliverables":["final.pptx"]})).unwrap();
    ctx.execute(json!({"action":"layout_search","spec":"source.json","page":"scene","id":"caption","area":[25,30,580,290],"register":true,"output":"search"})).unwrap();
    cycle(&ctx, "bad");
    std::fs::copy(
        ctx.root.join("bad/editable.pptx"),
        ctx.root.join("final.pptx"),
    )
    .unwrap();
    let c = ctx.json("bad/cycle.json").unwrap();
    ctx.write_json(
        "review.json",
        &json!({"cycle_sha256":sha_file(&ctx.root.join("bad/cycle.json")).unwrap(),
        "inspected":{"copy":c["pages"],"artwork":c["pages"],"readability":c["pages"]},
        "findings":[],"qa":{"status":"pass"},"note":"The overlap is fixed"}),
    )
    .unwrap();
    let e = ctx
        .execute(
            json!({"action":"evidence_record","cycle":"bad/cycle.json","review":"review.json"}),
        )
        .unwrap();
    assert_eq!(e["checks"][3]["status"], "fail", "{e}");
    let mut s = ctx.json("source.json").unwrap();
    s["pages"][0]["copy"] = json!({});
    s["pages"][0]["texts"] = json!([]);
    ctx.replace_json("source.json", &s).unwrap();
    assert!(placement_failed(&cycle(&ctx, "deleted")));
}

#[test]
fn reflow_preserves_cjk_copy_and_uses_real_line_height() {
    let (_dir, ctx) = fixture();
    let mut s = ctx.json("source.json").unwrap();
    let text = "山川与城市\n观察光照水分与生长环境";
    s["styles"]["body"]["font"] = json!("Source Han Sans SC");
    s["pages"][0]["copy"]["caption"] = json!(text);
    s["pages"][0]["texts"][0]["text"] = json!(text);
    ctx.replace_json("source.json", &s).unwrap();
    let p=ctx.execute(json!({"action":"fit","spec":"source.json","objects":["scene/caption"],"reflow":true,"ink_align":"top",
        "targets":[{"page":"scene","id":"caption","box":[40,30,95,270]}]})).unwrap();
    assert!(p["proposals"][0]["lines"].as_u64().unwrap() > 2, "{p}");
    assert_eq!(
        p["proposals"][0]["ink_alignment"]["status"], "proposed",
        "{p}"
    );
    assert_eq!(
        ctx.json("source.json").unwrap()["pages"][0]["texts"][0]["text"],
        text
    );
}

#[test]
fn malformed_search_returns_recoverable_top_level_schema() {
    let (_dir, ctx) = fixture();
    let err=ctx.execute(json!({"action":"layout_search","spec":"source.json","page":"scene","object":"caption","area":[[0,0],[1,0],[1,1],[0,1]]})).unwrap_err().to_string();
    assert!(err.contains("unknown layout_search field object"));
    assert!(
        err.contains("required_top_level")
            && err.contains("not four corners")
            && err.contains("top-level text object ID"),
        "{err}"
    );
}
