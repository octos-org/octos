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
fn batch_fits_a_native_label_without_smaller_partial_output_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path());
    ctx.execute(json!({"action":"build","spec":"source.json","output":"original"}))
        .unwrap();
    let path = ctx.path("original/editable.pptx").unwrap();
    let before = sha_file(&path).unwrap();
    let document = octos_pptx::deck::open(&ctx, &path).unwrap();
    let label = document.slides[0]
        .shapes
        .iter()
        .find(|s| s.name == "octos:text:caption")
        .unwrap();
    let id = label.id.0;
    let fit = |rect: [f64; 4]| {
        json!({"command":"text.fitSingleLine",
        "params":{"id":id,"box":rect,"min_pt":10}})
    };
    let good = fit([0., 0., 180., 50.]);
    let bad = fit([0., 0., 2., 2.]);
    let error = ctx
        .execute(json!({"action":"batch","input":"original/editable.pptx",
        "output":"partial.pptx","commands":[good.clone(),bad]}))
        .unwrap_err();
    assert!(error.to_string().contains("allowed minimum"));
    assert!(!dir.path().join("partial.pptx").exists());
    assert_eq!(before, sha_file(&path).unwrap());
    let other = document.slides[0]
        .shapes
        .iter()
        .find(|s| s.name == "octos:text:other")
        .unwrap()
        .id
        .0;
    let conflicts = ctx
        .execute(json!({"action":"batch","input":"original/editable.pptx",
        "output":"conflict.pptx","commands":[
          {"command":"text.fitSingleLine","params":{"id":id,"box":[0,0,100,30],"min_pt":24}},
          {"command":"text.fitSingleLine","params":{"id":other,"box":[0,0,100,30],"min_pt":25}}
        ]}))
        .unwrap_err()
        .to_string();
    for detail in [
        "octos:text:caption",
        "17 pt",
        "24 pt",
        "octos:text:other",
        "20 pt",
        "25 pt",
    ] {
        assert!(conflicts.contains(detail), "{conflicts}");
    }
    assert!(!dir.path().join("conflict.pptx").exists());
    let result = ctx
        .execute(json!({"action":"batch","input":"original/editable.pptx",
        "output":"complete.pptx","commands":[good]}))
        .unwrap();
    assert_eq!(result["results"][0]["size_pt"], 17.);
    let reparsed = octos_pptx::deck::open(&ctx, &ctx.path("complete.pptx").unwrap()).unwrap();
    assert_eq!(
        reparsed.slides[0]
            .shapes
            .iter()
            .find(|s| s.id == label.id)
            .unwrap()
            .text
            .as_ref()
            .unwrap()
            .text(),
        "Before → After"
    );
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
