use octos_pptx::Context;
use serde_json::json;

fn fixture(root: &std::path::Path) -> Context {
    let ctx = Context::new(root).unwrap();
    image::RgbImage::from_pixel(800, 450, image::Rgb([245, 245, 245]))
        .save(root.join("art.png"))
        .unwrap();
    ctx.write_json(
        "source.json",
        &json!({"schema_version":1,
        "canvas":{"width_px":800,"height_px":450,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":24}},
        "pages":[{"id":"slide","background":"art.png","copy":{"a":"Visible label"},
        "texts":[{"id":"a","text":"Visible label","style":"body","box":[100,100,400,100]}]}]}),
    )
    .unwrap();
    ctx
}

#[test]
fn composite_attributes_actual_visible_characters_not_just_isolated_glyphs() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = fixture(tmp.path());
    ctx.execute(json!({"action":"cycle","spec":"source.json","output":"normal","width":800}))
        .unwrap();
    let r = ctx
        .execute(json!({"action":"inspect","cycle":"normal/cycle.json"}))
        .unwrap();
    assert_eq!(r["objects"][0]["ink"]["status"], "measured");
    assert_eq!(
        r["objects"][0]["composite_ink"]["status"], "attributed",
        "{r}"
    );
    assert_eq!(
        r["objects"][0]["composite_ink"]["missing_character_cells"],
        0
    );
    assert!(r["objects"][0]["composite_ink"]["pixels"].as_u64().unwrap() > 0);
}

#[test]
fn rotated_multiline_cjk_and_ligatures_use_actual_character_geometry() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = fixture(tmp.path());
    let original = ctx.json("source.json").unwrap();
    for (i, text, font, angle) in [
        (0, "office\nfinal label", "Inter", -17.),
        (1, "观察山海\n记录变化", "LXGW WenKai Lite", 15.),
    ] {
        let mut source = original.clone();
        source["styles"]["body"]["font"] = json!(font);
        source["pages"][0]["copy"]["a"] = json!(text);
        source["pages"][0]["texts"][0]["text"] = json!(text);
        source["pages"][0]["texts"][0]["rotation"] = json!(angle);
        source["pages"][0]["texts"][0]["box"] = json!([150, 150, 400, 150]);
        ctx.replace_json("source.json", &source).unwrap();
        ctx.execute(
            json!({"action":"cycle","spec":"source.json","output":format!("r{i}"),"width":800}),
        )
        .unwrap();
        let r = ctx
            .execute(json!({"action":"inspect","cycle":format!("r{i}/cycle.json")}))
            .unwrap();
        assert_eq!(
            r["objects"][0]["composite_ink"]["status"], "attributed",
            "{r}"
        );
    }
}

#[test]
fn invisible_text_and_overlapping_text_stay_unknown_in_actual_export() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = fixture(tmp.path());
    let original = ctx.json("source.json").unwrap();
    let mut spec = original.clone();
    spec["styles"]["body"]["color"] = json!("F5F5F5");
    ctx.replace_json("source.json", &spec).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"source.json","output":"invisible","width":800}))
        .unwrap();
    let r = ctx
        .execute(json!({"action":"inspect","cycle":"invisible/cycle.json"}))
        .unwrap();
    assert_eq!(r["objects"][0]["ink"]["status"], "measured");
    assert_eq!(r["objects"][0]["composite_ink"]["status"], "unknown");
    assert!(
        r["objects"][0]["composite_ink"]["missing_character_cells"]
            .as_u64()
            .unwrap()
            > 0
    );
    let qa = ctx
        .execute(json!({"action":"qa_check","cycle":"invisible/cycle.json"}))
        .unwrap();
    assert!(
        qa["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["code"] == "COMPOSITE_INK_UNVERIFIED")
    );
    let mut spec = original;
    let mut second = spec["pages"][0]["texts"][0].clone();
    second["id"] = json!("b");
    spec["pages"][0]["texts"]
        .as_array_mut()
        .unwrap()
        .push(second);
    spec["pages"][0]["copy"]["b"] = json!("Visible label");
    ctx.replace_json("source.json", &spec).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"source.json","output":"overlap","width":800}))
        .unwrap();
    let r = ctx
        .execute(json!({"action":"inspect","cycle":"overlap/cycle.json"}))
        .unwrap();
    for o in r["objects"].as_array().unwrap() {
        assert_eq!(o["ink"]["status"], "measured");
        assert_eq!(o["composite_ink"]["status"], "unknown");
        assert!(o["composite_ink"]["ambiguous_components"].as_u64().unwrap() > 0);
    }
}
