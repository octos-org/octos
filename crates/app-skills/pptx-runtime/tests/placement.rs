use octos_pptx::Context;
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
