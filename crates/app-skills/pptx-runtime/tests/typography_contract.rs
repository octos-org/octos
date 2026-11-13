use octos_pptx::Context;
use serde_json::{Value, json};

fn fixture(root: &std::path::Path) -> Context {
    let ctx = Context::new(root).unwrap();
    image::RgbImage::from_pixel(800, 450, image::Rgb([250, 250, 245]))
        .save(root.join("art.png"))
        .unwrap();
    ctx.write_json("source.json", &json!({"schema_version":1,
        "canvas":{"width_px":800,"height_px":450,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":20},"small":{"font":"Inter","size_pt":15}},
        "pages":[{"id":"slide","background":"art.png","copy":{"a":"First row","b":"Second row","title":"Heading"},
        "typography_groups":[{"id":"list","members":["a","b"],"fields":["font","size_pt"],"basis":"two same-level list labels; heading is separate"}],
        "texts":[{"id":"a","text":"First row","style":"body","box":[100,100,350,60]},
            {"id":"b","text":"Second row","style":"small","box":[100,200,350,60]},
            {"id":"title","text":"Heading","style":"body","format":{"size_pt":32,"bold":true},"box":[80,20,600,60]}]}]})).unwrap();
    ctx
}

fn cycle(ctx: &Context, out: &str) -> Value {
    ctx.execute(json!({"action":"cycle","spec":"source.json","output":out,"width":800}))
        .unwrap();
    ctx.execute(json!({"action":"qa_check","cycle":format!("{out}/cycle.json")}))
        .unwrap()
}

#[test]
fn typography_constraints_cannot_disappear_during_a_repair() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = fixture(tmp.path());
    let before = cycle(&ctx, "before");
    let mut source = ctx.json("source.json").unwrap();
    source["pages"][0]["typography_groups"] = json!([]);
    ctx.replace_json("source.json", &source).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"source.json","output":"after","width":800}))
        .unwrap();
    let (cy, spec) =
        octos_pptx::review::bound_cycle(&ctx, &ctx.path("after/cycle.json").unwrap()).unwrap();
    let inspect = ctx
        .execute(json!({"action":"inspect","cycle":"after/cycle.json"}))
        .unwrap();
    let qa = octos_pptx::qa::assess(&ctx, &cy, &spec, &inspect, &before["contract"]).unwrap();
    assert!(
        qa["constraint_changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["field"] == "typography_groups")
    );
    assert_eq!(
        qa["contract"]["pages"]["slide"]["typography_groups"],
        before["contract"]["pages"]["slide"]["typography_groups"]
    );
    assert_ne!(qa["status"], "pass");
}
