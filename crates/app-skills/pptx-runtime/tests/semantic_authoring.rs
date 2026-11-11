use octos_pptx::{Context, object_review, review, spec};
use serde_json::{Value, json};

fn fixture(root: &std::path::Path) -> Context {
    let ctx = Context::new(root).unwrap();
    let mut image = image::RgbImage::from_pixel(800, 450, image::Rgb([245, 240, 230]));
    // Generic, synthetic graphical arrow; no production page data.
    for x in 325..375 {
        for y in 124..128 {
            image.put_pixel(x, y, image::Rgb([20, 20, 20]));
        }
    }
    for i in 0..12 {
        for d in 0..3 {
            image.put_pixel(375 - i, 125 - i + d, image::Rgb([20, 20, 20]));
            image.put_pixel(375 - i, 125 + i + d, image::Rgb([20, 20, 20]));
        }
    }
    image.save(root.join("art.png")).unwrap();
    ctx.write_json(
        "source.json",
        &json!({"schema_version":1,
        "canvas":{"width_px":800,"height_px":450,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":18,"align":"center"}},
        "pages":[{"id":"flow","background":"art.png","design":"art.png",
            "copy":{"line":"Source → Sink","amount":"123"},
            "texts":[{"id":"line","text":"Source → Sink","style":"body","box":[70,95,600,70]},
                {"id":"amount","text":"123","style":"body","box":[80,300,100,50]}]}]}),
    )
    .unwrap();
    ctx
}
fn split(ctx: &Context) {
    ctx.execute(json!({"action":"split_text","spec":"source.json","output":"split.json","page":"flow","id":"line",
        "parts":[{"id":"left","text":"Source","box":[80,100,190,60]},
            {"id":"arrow","text":"→","box":[310,100,80,60]},
            {"id":"right","text":"Sink","box":[440,100,190,60]}]})).unwrap();
}
fn convert(ctx: &Context) -> Value {
    ctx.execute(json!({"action":"represent_symbol","spec":"split.json","output":"mapped.json",
        "page":"flow","id":"arrow","box":[320,108,64,34],"basis":"Arrow already present between the two labels in the synthetic artwork."})).unwrap()
}

#[test]
fn graphic_mapping_preserves_copy_exports_native_labels_and_requires_visual_symbol_review() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    split(&ctx);
    let before = ctx.json("split.json").unwrap();
    let result = convert(&ctx);
    assert_eq!(result["semantic_equivalence_verified"], false);
    let s = spec::load(&ctx, &ctx.path("mapped.json").unwrap(), None).unwrap();
    assert_eq!(json!(s.pages[0].copy), before["pages"][0]["copy"]);
    assert_eq!(s.pages[0].texts.len(), 3);
    assert!(!s.pages[0].texts.iter().any(|t| t.text == "→"));
    let cycle = ctx
        .execute(json!({"action":"cycle","spec":"mapped.json","output":"out","width":800}))
        .unwrap();
    let handoff = &cycle["peer_review_handoff"];
    assert_eq!(handoff["kind"], "review_data_not_a_tool");
    assert_eq!(handoff["ready_for_independent_review"], true);
    assert_eq!(handoff["notification_sent"], false);
    assert_eq!(handoff["quality_approved"], false);
    ctx.verify(&handoff["cycle"]).unwrap();
    ctx.verify(&handoff["renders"][0]).unwrap();
    let (c, s) = review::bound_cycle(&ctx, &ctx.path("out/cycle.json").unwrap()).unwrap();
    let inventory = object_review::inventory(&c, &s).unwrap();
    let row = inventory
        .iter()
        .find(|r| r["kind"] == "artwork_symbol")
        .unwrap();
    assert_eq!(
        row["checks_required"],
        json!(["representation", "uniqueness", "visibility"])
    );
    let assessed = object_review::assess(&c, &s, &json!({}), &json!({})).unwrap();
    assert_eq!(assessed.summary["symbol_unknown"], 1);
    let pack = ctx
        .execute(json!({"action":"review_pack","cycle":"out/cycle.json","output":"pack"}))
        .unwrap();
    let report = ctx.json(ctx.verify(&pack["report"]).unwrap()).unwrap();
    let mapped = report["object_evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "artwork_symbol")
        .unwrap();
    assert_eq!(mapped["images"].as_array().unwrap().len(), 3);
    for im in mapped["images"].as_array().unwrap() {
        ctx.verify(&im["image"]).unwrap();
    }
    let mut judgments = ctx
        .json(ctx.verify(&pack["object_review_template"]).unwrap())
        .unwrap();
    for row in judgments["object_reviews"].as_array_mut().unwrap() {
        for v in row["checks"].as_object_mut().unwrap().values_mut() {
            *v = json!({"status":"pass","observation":"Synthetic fixture answer, never a production judgment."});
        }
    }
    assert_eq!(
        object_review::assess(&c, &s, &judgments, &json!({}))
            .unwrap()
            .unknown,
        0
    );
    let mut changed = c.clone();
    changed["renders"]["editable"]["pages"][0]["sha256"] = json!("new-render");
    assert_eq!(
        object_review::assess(&changed, &s, &judgments, &json!({}))
            .unwrap()
            .summary["symbol_unknown"],
        1
    );
    let mut pending = judgments.clone();
    for row in pending["object_reviews"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .filter(|r| r["object_id"] == "arrow")
    {
        row["checks"] = json!({});
    }
    assert_eq!(
        object_review::assess(&c, &s, &pending, &json!({}))
            .unwrap()
            .summary["symbol_unknown"],
        1
    );
}

#[test]
fn artwork_layers_share_the_checked_stack_and_bind_their_source_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    image::RgbImage::from_pixel(24, 12, image::Rgb([200, 40, 20]))
        .save(temp.path().join("patch.png"))
        .unwrap();
    let source = std::fs::read(temp.path().join("source.json")).unwrap();
    let patch = ctx.execute(json!({"action":"patch","spec":"source.json","output":"changed.json","edits":[{"page":"flow","id":"amount","set":{"box":[80,100,120,50]}}],
        "artwork_layers":[{"page":"flow","layers":[{"id":"restored-accent","image":"patch.png","box":[60,70,240,120],"basis":"Synthetic local artwork repair"}]}]})).unwrap();
    assert_eq!(patch["artwork_layer_changes"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read(temp.path().join("source.json")).unwrap(),
        source
    );
    ctx.execute(json!({"action":"cycle","spec":"changed.json","output":"layer-cycle","width":800}))
        .unwrap();
    let (cycle, s) =
        review::bound_cycle(&ctx, &ctx.path("layer-cycle/cycle.json").unwrap()).unwrap();
    assert_eq!(s.pages[0].artwork_layers.len(), 1);
    let full = octos_pptx::deck::open(&ctx, &ctx.verify(&cycle["editable"]).unwrap()).unwrap();
    let blank = octos_pptx::deck::open(&ctx, &ctx.verify(&cycle["blank"]).unwrap()).unwrap();
    assert_eq!(
        full.slides[0].shapes[1].name,
        "octos:artwork-layer:restored-accent"
    );
    assert!(full.slides[0].shapes[2].name.starts_with("octos:text:"));
    assert_eq!(blank.slides[0].shapes.len(), 2);
    for kind in ["editable", "blank"] {
        let img = image::open(ctx.verify(&cycle["renders"][kind]["pages"][0]).unwrap())
            .unwrap()
            .to_rgb8();
        assert_eq!(img.get_pixel(80, 80).0, [200, 40, 20]);
    }
    let full_pixels = image::open(
        ctx.verify(&cycle["renders"]["editable"]["pages"][0])
            .unwrap(),
    )
    .unwrap()
    .to_rgb8();
    assert!(
        full_pixels.enumerate_pixels().any(|(x, y, p)| x > 80
            && x < 280
            && y > 100
            && y < 160
            && p.0.iter().all(|n| *n < 30)),
        "native text must remain above the patch"
    );
    let mut bytes = std::fs::read(temp.path().join("patch.png")).unwrap();
    bytes.push(0);
    std::fs::write(temp.path().join("patch.png"), bytes).unwrap();
    assert!(review::bound_cycle(&ctx, &ctx.path("layer-cycle/cycle.json").unwrap()).is_err());
}

#[test]
fn invalid_artwork_patch_does_not_save_partial_text_changes() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    image::RgbImage::from_pixel(10, 10, image::Rgb([200, 40, 20]))
        .save(temp.path().join("patch.png"))
        .unwrap();
    let original = std::fs::read(temp.path().join("source.json")).unwrap();
    for (name, layer) in [
        (
            "outside",
            json!({"id":"accent","image":"patch.png","box":[790,10,40,40],"basis":"test"}),
        ),
        (
            "missing",
            json!({"id":"accent","image":"no.png","box":[10,10,40,40],"basis":"test"}),
        ),
        (
            "hash",
            json!({"id":"accent","image":{"path":"patch.png","sha256":"wrong"},"box":[10,10,40,40],"basis":"test"}),
        ),
        (
            "unknown",
            json!({"id":"accent","image":"patch.png","box":[10,10,40,40],"basis":"test","z_order":999}),
        ),
    ] {
        assert!(ctx.execute(json!({"action":"patch","spec":"source.json","output":format!("{name}.json"),"edits":[{"page":"flow","id":"amount","set":{"format":{"bold":true}}}],"artwork_layers":[{"page":"flow","layers":[layer]}]})).is_err());
        assert!(!temp.path().join(format!("{name}.json")).exists());
    }
    assert_eq!(
        std::fs::read(temp.path().join("source.json")).unwrap(),
        original
    );
}
