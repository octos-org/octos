use octos_pptx::Context;
use serde_json::json;

fn fixture(root: &std::path::Path) -> Context {
    let ctx = Context::new(root).unwrap();
    image::RgbImage::from_pixel(800, 450, image::Rgb([245, 240, 230]))
        .save(root.join("art.png"))
        .unwrap();
    ctx.write_json("source.json", &json!({"schema_version":1,
        "canvas":{"width_px":800,"height_px":450,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":20}},
        "pages":[{"id":"scene","background":"art.png","copy":{"caption":"A short label"},
            "texts":[{"id":"caption","text":"A short label","style":"body","box":[100,120,500,200]}]}]})).unwrap();
    ctx
}

#[test]
fn target_size_policy_uses_the_target_not_the_old_large_frame() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let result = ctx
        .execute(json!({"action":"fit","spec":"source.json",
        "ink_align":"middle","reflow":true,"min_size_pt":8,"max_size_pt":20,"max_lines":1,
        "targets":[{"page":"scene","id":"caption","box":[150,200,120,60]}]}))
        .unwrap();
    let row = &result["proposals"][0];
    assert_eq!(row["state"], "proposal", "{result}");
    assert_eq!(row["ink_alignment"]["status"], "proposed", "{result}");
    assert_eq!(row["lines"], 1);
    assert!(row["proposed_size_pt"].as_f64().unwrap() < 20.);
    // Reopen the actual exported artifact and verify the selected size/line count.
    let mut spec = ctx.json("source.json").unwrap();
    for (k, v) in row["set"].as_object().unwrap() {
        spec["pages"][0]["texts"][0][k] = v.clone();
    }
    ctx.write_json("fitted.json", &spec).unwrap();
    let build = ctx
        .execute(json!({"action":"build","spec":"fitted.json","output":"fitted"}))
        .unwrap();
    let inspected = ctx
        .execute(json!({"action":"inspect","input":ctx.verify(&build["editable"]).unwrap()}))
        .unwrap();
    assert_eq!(inspected["objects"][0]["layout"]["lines"], 1);
    assert_eq!(inspected["objects"][0]["layout"]["overflow"], false);
}
