use octos_pptx::{Context, spec};
use serde_json::json;

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

#[test]
fn help_reports_page_and_guarded_text_capabilities() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let patch = ctx
        .execute(json!({"action":"help","topic":"patch"}))
        .unwrap();
    let batch = ctx
        .execute(json!({"action":"help","topic":"batch"}))
        .unwrap();
    assert_eq!(patch["supports_output_page"], true);
    assert!(
        patch["page_scoped_assets"]
            .as_str()
            .unwrap()
            .contains("only that page")
    );
    assert_eq!(batch["supports_guarded_text_edits"], true);
}

#[test]
fn page_patches_can_be_assembled_without_losing_other_formatting() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut source = ctx.json("source.json").unwrap();
    let mut second = source["pages"][0].clone();
    second["id"] = json!("scene-b");
    second["artwork_layers"] = json!([{"id":"shared-art","image":"art.png",
        "box":[10,10,100,100],"basis":"shared illustration"}]);
    source["pages"].as_array_mut().unwrap().push(second);
    ctx.write_json("two-pages.json", &source).unwrap();
    let original = ctx.read("two-pages.json").unwrap();

    let barrier = std::sync::Barrier::new(2);
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            ctx.execute(json!({"action":"patch","spec":"two-pages.json",
                "output":"pages/first.json","output_page":"scene",
                "edits":[{"page":"scene","id":"caption","set":{"format":{"color":"333333"}}}]}))
                .unwrap()
        });
        let second = scope.spawn(|| {
            barrier.wait();
            ctx.execute(json!({"action":"patch","spec":"two-pages.json",
                "output":"pages/second.json","output_page":"scene-b",
                "edits":[{"page":"scene-b","id":"caption","set":{"format":{"size_pt":19}}}]}))
                .unwrap()
        });
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_eq!(first["output_kind"], "page");
    assert_eq!(first["page_id"], "scene");
    assert_eq!(second["page_id"], "scene-b");
    assert_eq!(first["sources"], second["sources"]);
    assert!(first.get("spec").is_none());
    assert_eq!(ctx.read("two-pages.json").unwrap(), original);

    source["pages"] = json!([{"path":"pages/first.json"},{"path":"pages/second.json"}]);
    ctx.write_json("assembled.json", &source).unwrap();
    let assembled = spec::load(&ctx, &ctx.path("assembled.json").unwrap(), None).unwrap();
    assert_eq!(assembled.pages.len(), 2);
    let first_style = assembled.style(&assembled.pages[0].texts[0]).unwrap();
    let second_style = assembled.style(&assembled.pages[1].texts[0]).unwrap();
    assert_eq!(
        (
            first_style.size_pt,
            first_style.align.as_str(),
            first_style.color.as_str()
        ),
        (17., "center", "333333")
    );
    assert_eq!(
        (
            second_style.size_pt,
            second_style.align.as_str(),
            second_style.color.as_str()
        ),
        (19., "center", "222222")
    );
    assert_eq!(assembled.pages[0].texts[1].text, "Review");
    assert_eq!(assembled.pages[1].texts[1].text, "Review");
    assert_eq!(assembled.pages[1].artwork_layers.len(), 1);
    let artwork_path = ctx.path("art.png").unwrap();
    assert_eq!(
        assembled.pages[1].artwork_layers[0].image["path"].as_str(),
        artwork_path.to_str()
    );
}

#[test]
fn concurrent_page_patches_cover_every_page_and_assemble_in_order() {
    const PAGES: usize = 12;
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut source = ctx.json("source.json").unwrap();
    let template = source["pages"][0].clone();
    source["pages"] = json!(
        (0..PAGES)
            .map(|index| {
                let mut page = template.clone();
                page["id"] = json!(format!("page-{index}"));
                page
            })
            .collect::<Vec<_>>()
    );
    ctx.write_json("all-pages.json", &source).unwrap();
    let original = ctx.read("all-pages.json").unwrap();
    let barrier = std::sync::Barrier::new(PAGES);
    let results = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for index in 0..PAGES {
            let ctx = &ctx;
            let barrier = &barrier;
            handles.push(scope.spawn(move || {
                let page_id = format!("page-{index}");
                let output = format!("pages/page-{index}.json");
                barrier.wait();
                ctx.execute(json!({"action":"patch","spec":"all-pages.json",
                    "output":output,"output_page":&page_id,
                    "edits":[{"page":&page_id,"id":"caption",
                        "set":{"format":{"size_pt":20 + index}}}]}))
                    .unwrap()
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(ctx.read("all-pages.json").unwrap(), original);
    assert_eq!(results.len(), PAGES);
    for (index, result) in results.iter().enumerate() {
        let expected_page = format!("page-{index}");
        assert_eq!(result["output_kind"], "page");
        assert_eq!(result["page_id"].as_str(), Some(expected_page.as_str()));
        assert_eq!(result["sources"], results[0]["sources"]);
    }
    source["pages"] = json!(
        (0..PAGES)
            .map(|index| { json!({"path":format!("pages/page-{index}.json")}) })
            .collect::<Vec<_>>()
    );
    ctx.write_json("all-assembled.json", &source).unwrap();
    let assembled = spec::load(&ctx, &ctx.path("all-assembled.json").unwrap(), None).unwrap();
    assert_eq!(assembled.pages.len(), PAGES);
    for (index, page) in assembled.pages.iter().enumerate() {
        assert_eq!(page.id, format!("page-{index}"));
        let style = assembled.style(&page.texts[0]).unwrap();
        assert_eq!(style.size_pt, (20 + index) as f64);
        assert_eq!(style.align, "center");
        assert_eq!(style.color, "222222");
        assert_eq!(page.texts[1].text, "Review");
    }
}

#[test]
fn page_patch_binds_all_sources_but_checks_only_its_own_assets() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut source = ctx.json("source.json").unwrap();
    let mut first = source["pages"][0].clone();
    first["background"] = json!("../art.png");
    let mut second = first.clone();
    second["id"] = json!("scene-b");
    second["background"] = json!("../missing.png");
    ctx.write_json("pages/first.json", &first).unwrap();
    ctx.write_json("pages/second.json", &second).unwrap();
    source["pages"] = json!([
        {"path":"pages/first.json"},
        {"path":"pages/second.json"}
    ]);
    ctx.write_json("paged.json", &source).unwrap();
    let original = ctx.read("paged.json").unwrap();

    let result = ctx
        .execute(json!({"action":"patch","spec":"paged.json",
        "output":"out/first.json","output_page":"scene","edits":[]}))
        .unwrap();
    let sources = result["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 3);
    for path in ["paged.json", "pages/first.json", "pages/second.json"] {
        assert!(sources.contains(&ctx.binding(path).unwrap()));
    }
    assert_eq!(ctx.read("paged.json").unwrap(), original);
    assert!(temp.path().join("out/first.json").exists());

    for (output, selected) in [
        ("out/full.json", None),
        ("out/second.json", Some("scene-b")),
    ] {
        let mut request = json!({"action":"patch","spec":"paged.json",
            "output":output,"edits":[]});
        if let Some(page) = selected {
            request["output_page"] = json!(page);
        }
        assert!(ctx.execute(request).is_err());
        assert!(!temp.path().join(output).exists());
    }
    // A full load still verifies every page's assets before final assembly.
    assert!(spec::load(&ctx, &ctx.path("paged.json").unwrap(), None).is_err());

    // The selected page's declared asset hash must still match the file.
    let mut mismatched = first;
    mismatched["background"] = json!({"path":"../art.png","sha256":"0".repeat(64)});
    ctx.write_json("pages/mismatched.json", &mismatched)
        .unwrap();
    let mut mismatched_root = source.clone();
    mismatched_root["pages"][0]["path"] = json!("pages/mismatched.json");
    ctx.write_json("mismatched-root.json", &mismatched_root)
        .unwrap();
    assert!(
        ctx.execute(json!({"action":"patch","spec":"mismatched-root.json",
        "output":"out/mismatched.json","output_page":"scene","edits":[]}))
            .is_err()
    );
    assert!(!temp.path().join("out/mismatched.json").exists());

    // Unrelated page JSON is still structurally validated and source-bound.
    let mut invalid = second;
    invalid["texts"][0]["style"] = json!("missing-style");
    ctx.write_json("pages/invalid.json", &invalid).unwrap();
    source["pages"][1]["path"] = json!("pages/invalid.json");
    ctx.write_json("invalid-root.json", &source).unwrap();
    assert!(
        ctx.execute(json!({"action":"patch","spec":"invalid-root.json",
        "output":"out/invalid.json","output_page":"scene","edits":[]}))
            .is_err()
    );
    assert!(!temp.path().join("out/invalid.json").exists());
}

#[test]
fn page_patch_binds_image_bytes_for_later_full_load() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let original_image = ctx.binding("art.png").unwrap();
    let result = ctx
        .execute(json!({"action":"patch","spec":"source.json",
        "output":"pages/scene.json","output_page":"scene","edits":[]}))
        .unwrap();
    let page = ctx.json(result["page"]["path"].as_str().unwrap()).unwrap();
    assert_eq!(page["background"]["sha256"], original_image["sha256"]);

    let mut source = ctx.json("source.json").unwrap();
    source["pages"] = json!([{"path":"pages/scene.json"}]);
    ctx.write_json("assembled.json", &source).unwrap();
    let assembled = ctx.path("assembled.json").unwrap();
    assert!(spec::load(&ctx, &assembled, None).is_ok());

    image::RgbImage::from_pixel(800, 450, image::Rgb([1, 2, 3]))
        .save(temp.path().join("art.png"))
        .unwrap();
    assert_ne!(
        ctx.binding("art.png").unwrap()["sha256"],
        original_image["sha256"]
    );
    assert!(spec::load(&ctx, &assembled, None).is_err());
}

#[test]
fn page_patch_rejects_edits_outside_its_page_without_writing_output() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut source = ctx.json("source.json").unwrap();
    let mut second = source["pages"][0].clone();
    second["id"] = json!("scene-b");
    source["pages"].as_array_mut().unwrap().push(second);
    ctx.write_json("two-pages.json", &source).unwrap();

    let requests = [
        json!({"action":"patch","spec":"two-pages.json","output":"bad-edit.json",
            "output_page":"scene","edits":[{"page":"scene-b","id":"caption",
                "set":{"format":{"color":"333333"}}}]}),
        json!({"action":"patch","spec":"two-pages.json","output":"bad-art.json",
            "output_page":"scene","edits":[],"artwork_layers":[{"page":"scene-b","layers":[]}]}),
        json!({"action":"patch","spec":"two-pages.json","output":"bad-page.json",
            "output_page":"missing","edits":[]}),
    ];
    for request in requests {
        let output = request["output"].as_str().unwrap().to_owned();
        assert!(ctx.execute(request).is_err());
        assert!(!temp.path().join(output).exists());
    }
}

#[test]
fn guarded_batch_refuses_broad_text_replacements_without_an_output() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    for command in [
        json!({"command":"text.set","params":{"text":"new"}}),
        json!({"command":"text.replace","params":{"text":"old","with":"new"}}),
        json!({"command":"text.replace","params":{"text":"old","with":"new","at":{"slide":0}}}),
    ] {
        let request = json!({"action":"batch","guarded_text_edits":true,
            "output":"blocked.pptx","commands":[command]});
        assert!(ctx.execute(request).is_err());
        assert!(!temp.path().join("blocked.pptx").exists());
    }
}

#[test]
fn guarded_batch_replaces_only_the_selected_slide_match() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    ctx.execute(json!({"action":"batch","output":"original.pptx","commands":[
        {"command":"file.new","params":{"blank":true}},
        {"command":"slide.new","params":{"layout":"blank"}},
        {"command":"shape.insert","params":{"preset":"rect","rect":[10,10,200,60],"text":"Example"}},
        {"command":"slide.new","params":{"layout":"blank"}},
        {"command":"shape.insert","params":{"preset":"rect","rect":[10,10,200,60],"text":"Example"}}
    ]})).unwrap();
    let found = ctx
        .execute(json!({"action":"batch","input":"original.pptx","commands":[
            {"command":"text.find","params":{"text":"Example"}}
        ]}))
        .unwrap();
    let matches = found["results"][0].as_array().unwrap();
    assert_eq!(matches.len(), 2);
    let first = matches
        .iter()
        .find(|entry| entry["slide"] == 0)
        .unwrap()
        .clone();
    let changed = ctx
        .execute(json!({"action":"batch","input":"original.pptx",
        "output":"changed.pptx","guarded_text_edits":true,"commands":[
            {"command":"text.replace","params":{"text":"Example","with":"Changed","at":first}}
        ]}))
        .unwrap();
    assert_eq!(changed["guarded_text_edits"], true);
    assert_eq!(changed["results"][0]["replaced"], 1);
    let check = ctx
        .execute(json!({"action":"batch","input":"changed.pptx","commands":[
            {"command":"text.find","params":{"text":"Example"}},
            {"command":"text.find","params":{"text":"Changed"}}
        ]}))
        .unwrap();
    assert_eq!(check["results"][0].as_array().unwrap().len(), 1);
    assert_eq!(check["results"][0][0]["slide"], 1);
    assert_eq!(check["results"][1].as_array().unwrap().len(), 1);
    assert_eq!(check["results"][1][0]["slide"], 0);
}

#[test]
fn two_slide_slices_keep_selected_text_pictures_and_chart_data_after_reuse() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let image_path = ctx.path("art.png").unwrap().to_string_lossy().into_owned();
    let mut commands = vec![json!({"command":"file.new","params":{"blank":true}})];
    for (index, text) in ["First", "Shared", "Shared", "Fourth"]
        .into_iter()
        .enumerate()
    {
        commands.push(json!({"command":"slide.new","params":{"layout":"blank"}}));
        commands.push(json!({"command":"shape.insert","params":{
            "preset":"rect","rect":[10,10,180,60],"text":text
        }}));
        if index == 1 || index == 2 {
            commands.push(json!({"command":"insert.picture","params":{
                "path":image_path.clone(),"rect":[10,100,80,40]
            }}));
            commands.push(json!({"command":"insert.chart","params":{
                "type":"column","rect":[240,30,250,180],"categories":["A","B"],
                "series":[{"name":"Series","values":[1,2]}]
            }}));
        }
    }
    ctx.execute(json!({"action":"batch","output":"baseline.pptx","commands":commands}))
        .unwrap();
    let found = ctx
        .execute(json!({"action":"batch","input":"baseline.pptx","commands":[
            {"command":"text.find","params":{"text":"Shared"}}
        ]}))
        .unwrap();
    let matches = found["results"][0].as_array().unwrap();
    assert_eq!(matches.len(), 2);
    for (index, name, replacement) in [
        (1, "one.pptx", "Second revised"),
        (2, "two.pptx", "Third revised"),
    ] {
        let target = matches
            .iter()
            .find(|entry| entry["slide"] == index)
            .unwrap()
            .clone();
        let result = ctx
            .execute(json!({"action":"batch","input":"baseline.pptx",
            "output":name,"guarded_text_edits":true,"commands":[
                {"command":"text.replace","params":{"text":"Shared","with":replacement,"at":target}}
            ]}))
            .unwrap();
        assert_eq!(result["guarded_text_edits"], true);
        assert_eq!(result["results"][0]["replaced"], 1);
    }
    let merged = ctx.execute(json!({"action":"batch","input":"baseline.pptx",
        "output":"merged.pptx","commands":[
            {"command":"slide.delete","params":{"index":2}},
            {"command":"slide.reuse","params":{"path":"two.pptx","slides":[2],"at":2,"keepFormatting":true}},
            {"command":"slide.delete","params":{"index":1}},
            {"command":"slide.reuse","params":{"path":"one.pptx","slides":[1],"at":1,"keepFormatting":true}}
        ]})).unwrap();
    assert_eq!(merged["quality_approved"], false);
    let inspected = ctx
        .execute(json!({"action":"batch","input":"merged.pptx","commands":[
            {"command":"slide.inspect","params":{"index":0}},
            {"command":"slide.inspect","params":{"index":1}},
            {"command":"slide.inspect","params":{"index":2}},
            {"command":"slide.inspect","params":{"index":3}}
        ]}))
        .unwrap();
    let slides = inspected["results"].as_array().unwrap();
    for (index, expected) in ["First", "Second revised", "Third revised", "Fourth"]
        .into_iter()
        .enumerate()
    {
        let shapes = slides[index]["shapes"].as_array().unwrap();
        assert!(shapes.iter().any(|shape| shape["text"] == expected));
        if index == 1 || index == 2 {
            assert!(shapes.iter().any(|shape| shape["kind"] == "picture"));
            let chart = shapes
                .iter()
                .find(|shape| shape["kind"] == "chart")
                .unwrap();
            assert_eq!(chart["chart"]["categories"], json!(["A", "B"]));
            assert_eq!(chart["chart"]["series"][0]["name"], "Series");
            assert_eq!(chart["chart"]["series"][0]["values"][0].as_f64(), Some(1.));
            assert_eq!(chart["chart"]["series"][0]["values"][1].as_f64(), Some(2.));
        }
    }
}
