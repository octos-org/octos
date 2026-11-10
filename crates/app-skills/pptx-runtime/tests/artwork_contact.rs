use octos_pptx::Context;
use serde_json::{Value, json};

#[test]
fn should_ignore_flat_fills_and_low_contrast_texture_including_dark_backgrounds() {
    for base in [20u8, 120, 235] {
        let mut image = image::RgbImage::from_pixel(120, 80, image::Rgb([base; 3]));
        for (x, y, p) in image.enumerate_pixels_mut() {
            *p = image::Rgb([base + (x.wrapping_mul(13).wrapping_add(y * 7) % 12) as u8; 3]);
        }
        let edges = octos_pptx::artwork_contact::EdgeMap::new(&image, [120., 80.]);
        let mut contact = octos_pptx::artwork_contact::Contact::default();
        for y in 10..70 {
            for x in 10..110 {
                contact.sample(&edges, [x as f64, y as f64]);
            }
        }
        let measured = contact.finish(&edges);
        assert_eq!(measured["candidate"], false);
        assert_eq!(measured["contact_pixels"], 0);
        assert_eq!(measured["quality_approved"], false);
    }
}

#[test]
fn should_preserve_contact_locations_across_raster_resolution() {
    for scale in [1u32, 2, 4] {
        let mut image = image::RgbImage::from_pixel(120 * scale, 80 * scale, image::Rgb([240; 3]));
        for x in 20 * scale..100 * scale {
            for y in 38 * scale..41 * scale {
                image.put_pixel(x, y, image::Rgb([15; 3]));
            }
        }
        let edges = octos_pptx::artwork_contact::EdgeMap::new(&image, [120., 80.]);
        let mut contact = octos_pptx::artwork_contact::Contact::default();
        // Dense glyph samples at the same canvas coordinates, not text-frame area.
        for y in 140..180 {
            for x in 120..360 {
                contact.sample(&edges, [x as f64 / 4., y as f64 / 4.]);
            }
        }
        let measured = contact.finish(&edges);
        assert_eq!(measured["candidate"], true);
        for point in measured["sample_points"].as_array().unwrap() {
            assert!((35. ..=44.).contains(&point[1].as_f64().unwrap()));
        }
        assert!(measured["sample_points"].as_array().unwrap().len() <= 64);
    }
}

fn cycle(ctx: &Context, name: &str, source: &Value) -> Value {
    ctx.write_json(format!("{name}.json"), source).unwrap();
    ctx.execute(json!({"action":"cycle","spec":format!("{name}.json"),"output":name,"width":800}))
        .unwrap();
    ctx.execute(json!({"action":"inspect","cycle":format!("{name}/cycle.json")}))
        .unwrap()
}

#[test]
fn should_find_artwork_crossing_glyphs_without_ocr_or_authored_forbidden_boxes() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = Context::new(tmp.path()).unwrap();
    let mut art = image::RgbImage::from_pixel(800, 450, image::Rgb([245, 240, 230]));
    art.save(tmp.path().join("plain.png")).unwrap();
    let mut spec = json!({"schema_version":1,
        "canvas":{"width_px":800,"height_px":450,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":32}},
        "pages":[{"id":"generic-scene","background":"plain.png","copy":{"label":"MMMM MM"},
        "texts":[{"id":"label","text":"MMMM MM","style":"body","box":[100,130,400,100]}]}]});
    let plain = cycle(&ctx, "normal", &spec);
    assert_eq!(
        plain["objects"][0]["ink"]["artwork_contact"]["candidate"],
        false
    );
    let b = &plain["objects"][0]["ink"]["bounds"];
    let (x, y, w, h) = (
        b[0].as_f64().unwrap(),
        b[1].as_f64().unwrap(),
        b[2].as_f64().unwrap(),
        b[3].as_f64().unwrap(),
    );
    // This line is outside all glyphs but inside the oversized authored frame.
    for px in x as u32..(x + w) as u32 {
        for py in (y + h + 8.) as u32..(y + h + 11.) as u32 {
            art.put_pixel(px, py, image::Rgb([10, 10, 10]));
        }
    }
    art.save(tmp.path().join("underline.png")).unwrap();
    spec["pages"][0]["background"] = json!("underline.png");
    let clear = cycle(&ctx, "clear", &spec);
    assert_eq!(
        clear["objects"][0]["ink"]["artwork_contact"]["candidate"],
        false
    );
    for px in (x + 4.) as u32..(x + w - 4.) as u32 {
        for py in (y + h / 2.) as u32..(y + h / 2. + 3.) as u32 {
            art.put_pixel(px, py, image::Rgb([10, 10, 10]));
        }
    }
    art.save(tmp.path().join("crossing.png")).unwrap();
    spec["pages"][0]["background"] = json!("crossing.png");
    let crossed = cycle(&ctx, "crossed", &spec);
    let contact = &crossed["objects"][0]["ink"]["artwork_contact"];
    assert_eq!(contact["candidate"], true, "{contact}");
    assert!(contact["contact_pixels"].as_u64().unwrap() >= 6);
    assert_eq!(contact["semantic_collision_proven"], false);
    ctx.execute(json!({"action":"review_pack","cycle":"crossed/cycle.json","output":"pack"}))
        .unwrap();
    let relations = ctx.json("pack/relations.json").unwrap();
    let signals = relations["pages"][0]["signals"].as_array().unwrap();
    let s = signals
        .iter()
        .find(|s| s["code"] == "RASTER_STROKE_CONTACT")
        .expect("automatic relation, no explicit raster_overlap call required");
    assert_eq!(s["object_id"], "label");
    assert!(
        octos_pptx::relations::validate_responses(std::slice::from_ref(s), &json!({})).is_err()
    );
    let fabricated = json!({"relation_reviews":[{"id":s["id"],"status":"intentional","basis":"Looks intentional"}]});
    assert!(
        octos_pptx::relations::validate_responses(std::slice::from_ref(s), &fabricated).is_err()
    );
    // A review that omits relation evidence must still hit the freshly measured
    // native gate, including when the caller forges the cached inspection JSON.
    ctx.execute(json!({"action":"evidence_init","task_id":"contact-regression","owner_agent":"maker","stagnant_attempts":3,"deliverables":["crossed/editable.pptx"]})).unwrap();
    let mut c = ctx.json("crossed/cycle.json").unwrap();
    let cached = c["inspection"]["report"]["path"]
        .as_str()
        .unwrap()
        .to_string();
    ctx.replace_json(&cached, &json!({"objects":[],"quality_approved":true}))
        .unwrap();
    c["inspection"]["report"] = ctx.binding(&cached).unwrap();
    ctx.replace_json("crossed/cycle.json", &c).unwrap();
    ctx.write_json("claimed.json",&json!({"cycle_sha256":octos_pptx::fs::sha_file(&ctx.path("crossed/cycle.json").unwrap()).unwrap(),"findings":[],"qa":{"status":"pass"}})).unwrap();
    let record=ctx.execute(json!({"action":"evidence_record","cycle":"crossed/cycle.json","review":"claimed.json"})).unwrap();
    let fresh = ctx
        .json(ctx.verify(&record["native_qa"]["report"]).unwrap())
        .unwrap();
    assert!(
        fresh["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["code"] == "ARTWORK_CONTACT_UNVERIFIED"
                && f["model_disposition_allowed"] == false)
    );
    assert_ne!(record["checks"][3]["status"], "pass");
}
