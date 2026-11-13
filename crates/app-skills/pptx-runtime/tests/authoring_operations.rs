use octos_pptx::{Context, fs::sha_file};
use serde_json::{Value, json};

fn fixture(root: &std::path::Path) -> Context {
    let ctx = Context::new(root).unwrap();
    image::RgbImage::from_pixel(800, 450, image::Rgb([245, 240, 230]))
        .save(root.join("art.png"))
        .unwrap();
    ctx.write_json("source.json", &json!({"schema_version":1,
        "canvas":{"width_px":800,"height_px":450,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":20}},
        "pages":[{"id":"scene","background":"art.png","copy":{"caption":"Before → After","other":"Review"},
            "texts":[{"id":"caption","text":"Before → After","style":"body","box":[100,100,500,80],
                "format":{"size_pt":17,"align":"center","color":"222222"}},
                {"id":"other","text":"Review","style":"body","box":[100,250,200,80]}]}]})).unwrap();
    ctx
}

fn parts() -> Value {
    json!([
        {"id":"left","text":"Before","box":[100,100,180,80]},
        {"id":"arrow","text":"→","box":[300,100,60,80]},
        {"id":"right","text":"After","box":[400,100,180,80]}
    ])
}

#[test]
fn split_keeps_canonical_copy_and_tracks_every_unicode_character_through_render() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let hash = sha_file(&temp.path().join("source.json")).unwrap();
    let r = ctx
        .execute(
            json!({"action":"split_text","spec":"source.json","output":"split.json",
        "page":"scene","id":"caption","parts":parts()}),
        )
        .unwrap();
    let s = ctx.json("split.json").unwrap();
    assert_eq!(
        s["pages"][0]["copy"],
        ctx.json("source.json").unwrap()["pages"][0]["copy"]
    );
    assert_eq!(s["pages"][0]["texts"][0]["copy_id"], "caption");
    assert_eq!(s["pages"][0]["texts"][0]["copy_range"], json!([0, 6]));
    assert_eq!(s["pages"][0]["texts"][1]["copy_range"], json!([6, 8]));
    assert_eq!(s["pages"][0]["texts"][2]["copy_range"], json!([8, 14]));
    assert_eq!(r["content_preserved"], true);
    assert_eq!(r["quality_approved"], false);
    assert_eq!(hash, sha_file(&temp.path().join("source.json")).unwrap());
    ctx.execute(json!({"action":"cycle","spec":"split.json","output":"cycle","width":800}))
        .unwrap();
    let inspection = ctx.json("cycle/inspection.json").unwrap();
    assert_eq!(inspection["objects"].as_array().unwrap().len(), 4);
}
