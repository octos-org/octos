use octos_pptx::{Context, fs::sha_file};
use serde_json::{Value, json};

#[test]
fn plugin_manifest_exposes_runtime_actions_and_new_feedback_arguments() {
    let manifest: Value = serde_json::from_str(include_str!("../manifest.json")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::new(dir.path()).unwrap();
    let help = ctx.execute(json!({"action":"help"})).unwrap();
    let properties = &manifest["tools"][0]["input_schema"]["properties"];
    for action in help["actions"].as_array().unwrap() {
        assert!(
            properties["action"]["enum"]
                .as_array()
                .unwrap()
                .contains(action),
            "action missing from plugin schema: {action}"
        );
    }
    for field in [
        "previous_cycle",
        "pages",
        "page_numbers",
        "ink_align",
        "targets",
        "image",
        "max_size_pt",
        "max_lines",
        "group",
        "parts",
        "group_id",
        "sizes_pt",
        "padding_px",
        "basis",
    ] {
        assert!(
            properties.get(field).is_some(),
            "argument missing from plugin schema: {field}"
        );
    }
}

#[test]
fn review_selection_resolves_display_numbers_without_guessing_source_ids() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut source = ctx.json("source.json").unwrap();
    let mut first = source["pages"][0].clone();
    first["id"] = json!("appendix");
    source["pages"].as_array_mut().unwrap().insert(0, first);
    ctx.replace_json("source.json", &source).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"source.json","output":"ordered","width":800}))
        .unwrap();
    let out = ctx
        .execute(json!({"action":"review_pack","cycle":"ordered/cycle.json",
        "page_numbers":[2],"output":"numbered"}))
        .unwrap();
    assert_eq!(out["page_map"][0]["page_number"], 1);
    assert_eq!(out["page_map"][0]["page_id"], "appendix");
    assert_eq!(out["page_map"][1]["page_id"], "scene");
    let report = ctx.json(ctx.verify(&out["report"]).unwrap()).unwrap();
    assert_eq!(report["coverage"].as_array().unwrap().len(), 1);
    assert_eq!(report["coverage"][0]["page"], "scene");
    assert!(
        report["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["page"] == "scene" && v["page_number"] == 2)
    );
    for numbers in [json!([]), json!([0]), json!([3]), json!(["2"])] {
        assert!(
            ctx.execute(json!({"action":"review_pack","cycle":"ordered/cycle.json",
            "page_numbers":numbers,"output":"bad"}))
                .is_err()
        );
    }
    assert!(
        ctx.execute(json!({"action":"review_pack","cycle":"ordered/cycle.json",
        "page_numbers":[2],"pages":["scene"],"output":"ambiguous"}))
            .is_err()
    );
}

fn fixture(root: &std::path::Path) -> Context {
    let ctx = Context::new(root).unwrap();
    image::RgbImage::from_pixel(800, 450, image::Rgb([245, 240, 230]))
        .save(root.join("art.png"))
        .unwrap();
    ctx.write_json("source.json", &json!({"schema_version":1,
        "canvas":{"width_px":800,"height_px":450,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":20,"align":"center","anchor":"middle"}},
        "pages":[{"id":"scene","background":"art.png","copy":{"caption":"Visible ink"},
            "texts":[{"id":"caption","text":"Visible ink","style":"body","box":[100,120,300,100]}]}]})).unwrap();
    ctx
}

#[test]
fn diagnostics_distinguish_measurement_from_placement_and_explain_geometry_schema() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let measured = ctx
        .execute(json!({"action":"fit","spec":"source.json","objects":["scene/caption"]}))
        .unwrap();
    assert_eq!(measured["mode"], "size_measurement");
    assert_eq!(measured["placement_proposal_count"], 0);
    assert!(measured["proposals"][0]["set"].get("box").is_none());
    for spec in [
        json!([{"region":[10,10,100,40]}]),
        json!({"region":[10,10,100,40]}),
    ] {
        let error = ctx
            .execute(json!({"action":"geometry","image":"art.png","spec":spec}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("file path"), "{error}");
        assert!(error.contains("Usage:"), "{error}");
        assert!(error.contains("TL,TR,BR,BL"), "{error}");
    }
}

#[test]
fn fit_aligns_actual_ink_without_changing_source_or_size() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let source_hash = sha_file(&temp.path().join("source.json")).unwrap();
    for vertical in ["top", "middle", "bottom"] {
        let proposal = ctx
            .execute(json!({"action":"fit","spec":"source.json",
            "objects":["scene/caption"],"ink_align":vertical,
            "targets":[{"page":"scene","id":"caption","box":[180,180,240,60]}]}))
            .unwrap();
        let row = &proposal["proposals"][0];
        assert_eq!(proposal["mode"], "ink_alignment");
        assert_eq!(proposal["placement_proposal_count"], 1);
        assert_eq!(row["ink_alignment"]["status"], "proposed", "{proposal}");
        assert_eq!(row["proposed_size_pt"], 20.);
        assert!(row["set"]["format"].is_null());
        let mut spec = ctx.json("source.json").unwrap();
        spec["pages"][0]["texts"][0]["box"] = row["set"]["box"].clone();
        let name = format!("{vertical}.json");
        ctx.write_json(&name, &spec).unwrap();
        let build = ctx
            .execute(json!({"action":"build","spec":name,"output":vertical}))
            .unwrap();
        let checked = ctx
            .execute(json!({"action":"inspect","input":ctx.verify(&build["editable"]).unwrap()}))
            .unwrap();
        let ink: [f64; 4] =
            serde_json::from_value(checked["objects"][0]["ink"]["bounds"].clone()).unwrap();
        // Standalone inspection uses points; the source uses 100 px/in.
        let ink = ink.map(|x| x / 0.72);
        assert!((ink[0] + ink[2] / 2. - 300.).abs() < 1., "{ink:?}");
        let (actual, expected) = match vertical {
            "top" => (ink[1], 180.),
            "middle" => (ink[1] + ink[3] / 2., 210.),
            _ => (ink[1] + ink[3], 240.),
        };
        assert!((actual - expected).abs() < 1., "{vertical} {ink:?}");
    }
    assert_eq!(
        source_hash,
        sha_file(&temp.path().join("source.json")).unwrap()
    );
}

#[test]
fn geometry_converts_full_page_pixels_to_canvas_and_rejects_cropped_aspect() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let source_hash = sha_file(&temp.path().join("source.json")).unwrap();
    image::RgbImage::from_pixel(1600, 900, image::Rgb([245, 240, 230]))
        .save(temp.path().join("preview.png"))
        .unwrap();
    let input = json!({"action":"geometry","image":"preview.png","spec":"source.json",
        "region":[200,240,600,200],"padding":0});
    let scaled = ctx.execute(input.clone()).unwrap();
    let bounds: [f64; 4] = serde_json::from_value(scaled["box"].clone()).unwrap();
    for (actual, expected) in bounds.into_iter().zip([100., 120., 300., 100.]) {
        assert!((actual - expected).abs() < 0.001, "{scaled}");
    }
    assert_eq!(scaled["geometry_units"], "canvas_pixels");
    assert_eq!(
        scaled["coordinate_transform"]["scale_xy"],
        json!([0.5, 0.5])
    );
    let mut untyped = input.clone();
    untyped.as_object_mut().unwrap().remove("spec");
    let untyped = ctx.execute(untyped).unwrap();
    assert_eq!(untyped["geometry_units"], "source_image_pixels");
    assert!(untyped["coordinate_warning"].is_string());
    image::RgbImage::from_pixel(400, 400, image::Rgb([245, 240, 230]))
        .save(temp.path().join("crop.png"))
        .unwrap();
    let mut cropped = input;
    cropped["image"] = json!("crop.png");
    assert!(
        ctx.execute(cropped)
            .unwrap_err()
            .to_string()
            .contains("aspect ratios")
    );
    assert_eq!(
        source_hash,
        sha_file(&temp.path().join("source.json")).unwrap()
    );
}

#[test]
fn rotated_fit_centers_real_exported_glyphs_in_label_axes() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    for (index, rotation) in [-38.0_f64, -12., 17., 90.].into_iter().enumerate() {
        let mut spec = ctx.json("source.json").unwrap();
        spec["pages"][0]["texts"][0]["rotation"] = json!(rotation);
        if index == 2 {
            // The CJK case must use an explicit covering face, not Inter with
            // a different fallback face for each character.
            spec["styles"]["body"]["font"] = json!("LXGW WenKai Lite");
            spec["pages"][0]["texts"][0]["text"] = json!("山海之间\n观察与表达");
            spec["pages"][0]["copy"]["caption"] = json!("山海之间\n观察与表达");
        }
        let vertical = ["top", "middle", "bottom", "middle"][index];
        let source = format!("tilt-{index}.json");
        ctx.write_json(&source, &spec).unwrap();
        let hash = sha_file(&temp.path().join(&source)).unwrap();
        let fit = ctx.execute(json!({"action":"fit","spec":source,
            "ink_align":vertical, "reflow":true,
            "targets":[{"page":"scene","id":"caption","box":[250,160,240,80],"rotation":rotation}]})).unwrap();
        let row = &fit["proposals"][0];
        assert_eq!(row["ink_alignment"]["status"], "proposed", "{fit}");
        assert_eq!(hash, sha_file(&temp.path().join(&source)).unwrap());
        assert_eq!(row["proposed_size_pt"], 20.);
        spec["pages"][0]["texts"][0]["box"] = row["set"]["box"].clone();
        let applied = format!("applied-{index}.json");
        ctx.write_json(&applied, &spec).unwrap();
        let build = ctx
            .execute(json!({"action":"build","spec":applied,"output":format!("build-{index}")}))
            .unwrap();
        let rendered = ctx.execute(json!({"action":"render","input":ctx.verify(&build["editable"]).unwrap(),"output":format!("render-{index}"),"width":800})).unwrap();
        let image = image::open(ctx.verify(&rendered["pages"][0]).unwrap())
            .unwrap()
            .to_rgb8();
        let mut lo = [f64::INFINITY; 2];
        let mut hi = [f64::NEG_INFINITY; 2];
        let (sin, cos) = (-rotation).to_radians().sin_cos();
        for (x, y, pixel) in image.enumerate_pixels() {
            if pixel.0.iter().any(|c| *c > 100) {
                continue;
            }
            let dx = x as f64 + 0.5 - 370.;
            let dy = y as f64 + 0.5 - 200.;
            let p = [370. + cos * dx - sin * dy, 200. + sin * dx + cos * dy];
            for axis in 0..2 {
                lo[axis] = lo[axis].min(p[axis]);
                hi[axis] = hi[axis].max(p[axis]);
            }
        }
        assert!(
            (lo[0] + hi[0] - 740.).abs() < 2.5,
            "angle {rotation}: {lo:?} {hi:?}"
        );
        let error = match vertical {
            "top" => lo[1] - 160.,
            "bottom" => hi[1] - 240.,
            _ => (lo[1] + hi[1]) / 2. - 200.,
        };
        assert!(
            error.abs() < 1.25,
            "angle {rotation}, {vertical}: {lo:?} {hi:?}"
        );
    }
}

#[test]
fn ink_alignment_removes_unused_frame_height_without_changing_font_or_owner() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    for (index, anchor) in ["top", "middle", "bottom"].into_iter().enumerate() {
        let rotation = if index == 1 { -16.0_f64 } else { 13.0_f64 };
        let mut spec = ctx.json("source.json").unwrap();
        spec["styles"]["body"]["anchor"] = json!(anchor);
        spec["pages"][0]["texts"][0]["box"] = json!([250, 140, 300, 180]);
        spec["pages"][0]["texts"][0]["rotation"] = json!(rotation);
        spec["pages"][0]["texts"][0]["region"] = json!("label");
        let (s, c) = rotation.to_radians().sin_cos();
        let owner = [[175., 190.], [625., 190.], [625., 290.], [175., 290.]].map(|p| {
            let (x, y) = (p[0] - 400., p[1] - 240.);
            [400. + c * x - s * y, 240. + s * x + c * y]
        });
        spec["pages"][0]["regions"] = json!({"label":{"quad":owner,"scope":"label"}});
        let original_owner = spec["pages"][0]["regions"].clone();
        ctx.replace_json("tall.json", &spec).unwrap();
        let original_hash = sha_file(&temp.path().join("tall.json")).unwrap();
        let result = ctx.execute(json!({"action":"fit","spec":"tall.json","ink_align":"middle",
            "targets":[{"page":"scene","id":"caption","box":[250,220,300,40],"rotation":rotation}]})).unwrap();
        let row = &result["proposals"][0];
        assert_eq!(row["ink_alignment"]["status"], "proposed", "{result}");
        assert_eq!(row["proposed_size_pt"], 20.);
        assert!(row["set"]["format"].is_null());
        assert!(row["set"]["box"][3].as_f64().unwrap() < 180.);
        assert_eq!(
            row["ink_alignment"]["frame_height_adjustment"]["line_layout_preserved"],
            true
        );
        assert_eq!(
            original_hash,
            sha_file(&temp.path().join("tall.json")).unwrap()
        );
        spec["pages"][0]["texts"][0]["box"] = row["set"]["box"].clone();
        assert_eq!(spec["pages"][0]["regions"], original_owner);
        ctx.replace_json("fitted.json", &spec).unwrap();
        let output = format!("fitted-{index}");
        ctx.execute(json!({"action":"cycle","spec":"fitted.json","output":output,"width":800}))
            .unwrap();
        let actual = image::open(temp.path().join(&output).join("rendered/slide-001.png"))
            .unwrap()
            .to_rgb8();
        let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
        let mut count = 0;
        for (x, y, pixel) in actual.enumerate_pixels() {
            if pixel[0] > 120 || pixel[1] > 120 || pixel[2] > 120 {
                continue;
            }
            count += 1;
            let (x, y) = (x as f64 + 0.5 - 400., y as f64 + 0.5 - 240.);
            let p = [400. + c * x + s * y, 240. - s * x + c * y];
            assert!(
                p[0] >= 175. && p[0] <= 625. && p[1] >= 190. && p[1] <= 290.,
                "{p:?}"
            );
            for a in 0..2 {
                lo[a] = lo[a].min(p[a]);
                hi[a] = hi[a].max(p[a]);
            }
        }
        assert!(count > 100);
        assert!(
            ((lo[0] + hi[0]) / 2. - 400.).abs() < 1.25,
            "{anchor}: {lo:?} {hi:?}"
        );
        assert!(
            ((lo[1] + hi[1]) / 2. - 240.).abs() < 1.25,
            "{anchor}: {lo:?} {hi:?}"
        );
    }
}

#[test]
fn rotated_fit_preserves_owner_and_requires_matching_angles() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut spec = ctx.json("source.json").unwrap();
    spec["pages"][0]["texts"][0]["rotation"] = json!(-20.);
    spec["pages"][0]["texts"][0]["region"] = json!("local");
    spec["pages"][0]["regions"] = json!({"local":[60,60,340,210]});
    ctx.write_json("tilted.json", &spec).unwrap();
    for (angle, expected) in [(0., "unsupported"), (-20., "infeasible")] {
        let fit = ctx
            .execute(
                json!({"action":"fit","spec":"tilted.json","ink_align":"middle",
          "targets":[{"page":"scene","id":"caption","box":[400,210,230,70],"rotation":angle}]}),
            )
            .unwrap();
        assert_eq!(
            fit["proposals"][0]["ink_alignment"]["status"], expected,
            "{fit}"
        );
        assert_eq!(fit["proposals"][0]["set"], json!({}));
    }
    assert!(
        ctx.execute(
            json!({"action":"fit","spec":"tilted.json","ink_align":"middle",
        "targets":[{"page":"scene","id":"caption","box":[0,0,230,70],"rotation":-20.}]})
        )
        .is_err()
    );
}

#[test]
fn fit_does_not_silently_accept_bad_targets_or_guess_rotated_placement() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    for objects in [
        json!({"page":"scene","object":"caption"}),
        json!([{"page":"scene","object":"caption"}]),
    ] {
        let err = ctx
            .execute(json!({"action":"fit","spec":"source.json","objects":objects}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("string") || err.contains("STRING"), "{err}");
        assert!(err.contains("Usage:"), "{err}");
    }
    for targets in [
        json!([{"page":"scene","id":"missing","box":[1,1,200,50]}]),
        json!([{"page":"scene","id":"caption","box":[1,1,0,50]}]),
    ] {
        assert!(
            ctx.execute(
                json!({"action":"fit","spec":"source.json","ink_align":"middle","targets":targets})
            )
            .is_err()
        );
    }
    let small = ctx
        .execute(
            json!({"action":"fit","spec":"source.json","ink_align":"middle",
        "targets":[{"page":"scene","id":"caption","box":[200,200,8,8]}]}),
        )
        .unwrap();
    assert_eq!(
        small["proposals"][0]["ink_alignment"]["status"],
        "infeasible"
    );
    assert_eq!(small["proposals"][0]["set"], json!({}));
    let mut spec = ctx.json("source.json").unwrap();
    spec["pages"][0]["texts"][0]["rotation"] = json!(-12);
    ctx.write_json("rotated.json", &spec).unwrap();
    let tilted = ctx
        .execute(json!({"action":"fit","spec":"rotated.json","ink_align":"middle"}))
        .unwrap();
    assert_eq!(
        tilted["proposals"][0]["ink_alignment"]["status"],
        "unsupported"
    );
    assert_eq!(tilted["proposals"][0]["set"], json!({}));
}

#[test]
fn review_pack_covers_unflagged_regions_and_shows_revisions_without_claiming_repairs() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    ctx.execute(json!({"action":"cycle","spec":"source.json","output":"before","width":800}))
        .unwrap();
    let original_hash = sha_file(&temp.path().join("before/editable.pptx")).unwrap();
    let mut spec = ctx.json("source.json").unwrap();
    spec["pages"][0]["texts"][0]["box"] = json!([110, 125, 300, 100]);
    ctx.write_json("revised.json", &spec).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"revised.json","output":"after","width":800}))
        .unwrap();
    let out = ctx
        .execute(json!({"action":"review_pack","cycle":"after/cycle.json",
        "previous_cycle":"before/cycle.json","output":"feedback"}))
        .unwrap();
    let report = ctx.json(ctx.verify(&out["report"]).unwrap()).unwrap();
    assert_eq!(report["quality_approved"], false);
    assert!(
        report["finding_summary"]["unknown"].as_u64().unwrap() > 0,
        "Numeric QA page indices must map to the selected source page IDs"
    );
    assert_eq!(report["coverage"][0]["page"], "scene");
    assert_eq!(report["coverage"][0]["sweep_viewports"], 4);
    assert_eq!(report["coverage"][0]["inspected"], false);
    assert_eq!(report["revisions"].as_array().unwrap().len(), 1);
    assert_eq!(report["revisions"][0]["object_id"], "caption");
    assert_eq!(report["revisions"][0]["quality_proven"], false);
    assert!(report["finding_changes"]["persisting"].as_array().is_some());
    for picture in report["artifacts"].as_array().unwrap() {
        ctx.verify(&picture["image"]).unwrap();
    }
    let views = report["artifacts"].as_array().unwrap();
    assert_eq!(
        views
            .iter()
            .filter(|a| a["kind"] == "sweep" && a["role"] == "current")
            .count(),
        4
    );
    assert!(!views.iter().any(|a| a["role"] == "design")); // Blank is never mislabeled as the original.
    assert!(
        views
            .iter()
            .any(|a| a["kind"] == "revision" && a["role"] == "before")
    );
    assert_eq!(
        original_hash,
        sha_file(&temp.path().join("before/editable.pptx")).unwrap()
    );
    assert!(ctx.execute(json!({"action":"review_pack","cycle":"after/cycle.json","pages":["missing"],"output":"invalid"})).is_err());
}

#[test]
fn changed_acceptance_target_cannot_count_as_a_repair_in_feedback() {
    let before = json!({"algorithm":"same","contract":{"target":1},"findings":[
        {"page":"a","object_id":"t","code":"OVERFLOW","status":"fail"}]});
    let after = json!({"algorithm":"same","contract":{"target":2},"findings":[]});
    let diff = octos_pptx::feedback::finding_changes(&before, &after);
    assert_eq!(diff["comparable"], false);
    assert_eq!(diff["no_longer_detected"], json!([]));
    let missing: Value = json!({"algorithm":"same","contract":{"target":1}});
    assert_eq!(
        octos_pptx::feedback::finding_changes(&before, &missing)["comparable"],
        false
    );
    let same = json!({"algorithm":"same","contract":{"target":1},"findings":[]});
    let comparable = octos_pptx::feedback::finding_changes(&before, &same);
    assert_eq!(comparable["comparable"], true);
    assert_eq!(
        comparable["no_longer_detected"].as_array().unwrap().len(),
        1
    );
    assert_eq!(comparable["quality_proven"], false);
    let mut engine_changed = same;
    engine_changed["engine_revision"] = json!("different");
    assert_eq!(
        octos_pptx::feedback::finding_changes(&before, &engine_changed)["comparable"],
        false
    );
}

#[test]
fn fit_respects_owner_boundaries_and_uses_margins_and_horizontal_alignment() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    for horizontal in ["left", "right"] {
        let mut spec = ctx.json("source.json").unwrap();
        spec["styles"]["body"]["align"] = json!(horizontal);
        spec["styles"]["body"]["anchor"] = json!("bottom");
        spec["styles"]["body"]["margins_px"] = json!([9, 7, 5, 3]);
        spec["pages"][0]["regions"] = json!({"card":[40,40,700,350]});
        spec["pages"][0]["texts"][0]["region"] = json!("card");
        let name = format!("{horizontal}.json");
        ctx.write_json(&name, &spec).unwrap();
        let result = ctx
            .execute(json!({"action":"fit","spec":name,"ink_align":"middle",
            "targets":[{"page":"scene","id":"caption","box":[200,200,260,70]}]}))
            .unwrap();
        let row = &result["proposals"][0];
        assert_eq!(row["ink_alignment"]["status"], "proposed", "{result}");
        let ink: [f64; 4] =
            serde_json::from_value(row["ink_alignment"]["after"]["bounds"].clone()).unwrap();
        let edge = if horizontal == "left" {
            ink[0]
        } else {
            ink[0] + ink[2]
        };
        assert!((edge - if horizontal == "left" { 200. } else { 460. }).abs() < 1.);
        let outside = ctx
            .execute(json!({"action":"fit","spec":name,"ink_align":"top",
            "targets":[{"page":"scene","id":"caption","box":[0,0,260,70]}]}))
            .unwrap();
        assert_eq!(
            outside["proposals"][0]["ink_alignment"]["status"],
            "infeasible"
        );
        assert_eq!(outside["proposals"][0]["set"], json!({}));
    }
}

#[test]
fn review_pack_refuses_stale_render_and_keeps_source_absence_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    ctx.execute(json!({"action":"cycle","spec":"source.json","output":"a","width":400}))
        .unwrap();
    let output=ctx.execute(json!({"action":"review_pack","cycle":"a/cycle.json","previous_cycle":"a/cycle.json","output":"same"})).unwrap();
    let report = ctx.json(ctx.verify(&output["report"]).unwrap()).unwrap();
    assert!(report["revisions"].as_array().unwrap().is_empty());
    let current: Vec<_> = report["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["role"] == "current")
        .collect();
    // The four overlapping views cover the whole page at native resolution.
    for (x, y) in [(0., 0.), (0.5, 0.5), (1., 1.), (0., 1.), (1., 0.)] {
        assert!(current.iter().any(|r| {
            let a = &r["viewport_normalized_xyxy"];
            a[0].as_f64().unwrap() <= x
                && a[1].as_f64().unwrap() <= y
                && a[2].as_f64().unwrap() >= x
                && a[3].as_f64().unwrap() >= y
        }));
    }
    for r in current {
        assert_eq!(r["resized"], false);
    }
    std::fs::write(temp.path().join("a/rendered/slide-001.png"), b"stale").unwrap();
    assert!(
        ctx.execute(json!({"action":"review_pack","cycle":"a/cycle.json","output":"stale"}))
            .is_err()
    );
    assert!(!temp.path().join("stale").exists());
}

#[test]
fn operations_returns_all_results_pixels_and_stops_before_later_writes() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path());
    let result = ctx
        .execute(json!({"action":"operations","steps":[
        {"action":"crop","input":"art.png","box":[0,0,100,100],"output":"crop.png"},
        {"action":"image_info","input":"crop.png"}],"images":["crop.png"]}))
        .unwrap();
    assert_eq!(result["completed_steps"], 2);
    assert_eq!(result["results"][1]["result"]["width"], 100);
    assert_eq!(result["model_media"], json!(["crop.png"]));
    assert_eq!(result["quality_approved"], false);
    let failure = ctx
        .execute(json!({"action":"operations","steps":[
        {"action":"image_info","input":"crop.png"},
        {"action":"crop","input":"art.png","box":[799,449,10,10],"output":"bad.png"},
        {"action":"crop","input":"art.png","box":[0,0,10,10],"output":"must-not-exist.png"}]}))
        .unwrap();
    assert_eq!(failure["tool_success"], false);
    assert_eq!(failure["completed_steps"], 1);
    assert!(!dir.path().join("must-not-exist.png").exists());
    assert!(
        ctx.execute(json!({"action":"operations","steps":[{"action":"operations","steps":[]}]}))
            .is_err()
    );
}

#[test]
fn review_view_binds_cycle_and_pixels_and_never_approves_them() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path());
    ctx.execute(json!({"action":"cycle","spec":"source.json","output":"a","width":800}))
        .unwrap();
    ctx.execute(json!({"action":"review_pack","cycle":"a/cycle.json","output":"pack"}))
        .unwrap();
    let request = json!({"action":"review_view","cycle":"a/cycle.json","index":"pack/view-index.json","groups":[0,1]});
    let view = ctx.execute(request.clone()).unwrap();
    assert!(!view["model_media"].as_array().unwrap().is_empty());
    assert_eq!(view["selected"].as_array().unwrap().len(), 2);
    assert_eq!(view["quality_approved"], false);
    let mut wrong = request.clone();
    wrong["groups"] = json!([9999]);
    assert!(ctx.execute(wrong).is_err());
    let file = view["model_media"][0].as_str().unwrap();
    std::fs::write(dir.path().join(file), b"tampered").unwrap();
    assert!(ctx.execute(request).is_err());
}

#[test]
fn crop_reuses_only_matching_content_viewport_and_verified_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path());
    let request = json!({"action":"crop","input":"art.png","box":[0,0,60,60],"output":"a.png","receipt":"a.json"});
    let a = ctx.execute(request.clone()).unwrap();
    let mut next = request.clone();
    next["output"] = json!("b.png");
    next["receipt"] = json!("b.json");
    next["reuse"] = a["receipt"].clone();
    let b = ctx.execute(next.clone()).unwrap();
    assert_eq!(b["processing_reused"], true);
    assert_eq!(a["crop"]["sha256"], b["crop"]["sha256"]);
    next["box"] = json!([0, 0, 50, 60]);
    next["output"] = json!("c.png");
    next["receipt"] = json!("c.json");
    assert_eq!(
        ctx.execute(next.clone()).unwrap()["processing_reused"],
        false
    );
    image::RgbImage::from_pixel(800, 450, image::Rgb([0, 0, 0]))
        .save(dir.path().join("art.png"))
        .unwrap();
    next["box"] = json!([0, 0, 60, 60]);
    next["output"] = json!("d.png");
    next["receipt"] = json!("d.json");
    assert_eq!(
        ctx.execute(next.clone()).unwrap()["processing_reused"],
        false
    );
    std::fs::write(dir.path().join("a.json"), b"{}").unwrap();
    next["output"] = json!("e.png");
    next["receipt"] = json!("e.json");
    assert!(ctx.execute(next).is_err());
}
