use image::{Rgb, RgbImage};
use octos_pptx::Context;
use serde_json::{Value, json};

fn rectangle(im: &mut RgbImage, x: u32, y: u32, w: u32, h: u32, color: Rgb<u8>) {
    for yy in y..y + h {
        for xx in x..x + w {
            im.put_pixel(xx, yy, color);
        }
    }
}

fn fixture(scale: u32, light: bool, shifted: bool) -> (tempfile::TempDir, Context, Value) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::new(dir.path()).unwrap();
    let (paper, ink) = if light {
        (Rgb([250, 250, 250]), Rgb([15, 15, 15]))
    } else {
        (Rgb([10, 20, 30]), Rgb([235, 245, 255]))
    };
    let mut blank = RgbImage::from_pixel(500 * scale, 270 * scale, paper);
    for y in [60, 130, 200] {
        rectangle(&mut blank, 40 * scale, y * scale, 140 * scale, scale, ink);
    }
    let mut design = blank.clone();
    let mut render = blank.clone();
    let mut labels = Vec::new();
    for (i, y) in [50, 120, 190].into_iter().enumerate() {
        let x = if shifted && i == 0 { 140 } else { 200 };
        for letter in 0..5 {
            rectangle(
                &mut design,
                (200 + letter * 12) * scale,
                y * scale,
                8 * scale,
                20 * scale,
                ink,
            );
            rectangle(
                &mut render,
                (x + letter * 12) * scale,
                y * scale,
                8 * scale,
                20 * scale,
                ink,
            );
        }
        labels.push(json!({"id":format!("caption-{i}"),"text":"Sample",
            "box":[x*scale,(y-3)*scale,100*scale,27*scale],"font_px":24*scale}));
    }
    blank.save(dir.path().join("blank.png")).unwrap();
    design.save(dir.path().join("design.png")).unwrap();
    render.save(dir.path().join("render.png")).unwrap();
    (
        dir,
        ctx,
        json!({"action":"compare","before":"design.png","background":"blank.png",
        "after":"render.png","output":"proof.png","labels":labels}),
    )
}

#[test]
fn shifted_labels_expose_reference_clearance_and_peer_alignment_without_ocr() {
    for (scale, light) in [(1, false), (2, true)] {
        let (_dir, ctx, request) = fixture(scale, light, true);
        let report = ctx.execute(request).unwrap();
        let evidence = &report["label_evidence"];
        let first = &evidence["labels"][0];
        assert_eq!(
            first["metrics"]["reference_ink_box"][0].as_f64(),
            Some(f64::from(200 * scale))
        );
        assert_eq!(
            first["metrics"]["candidate_ink_box"][0].as_f64(),
            Some(f64::from(140 * scale))
        );
        assert!(ctx.verify(&first["panel"]).is_ok());
        let findings = evidence["findings"].as_array().unwrap();
        let collision = findings
            .iter()
            .find(|f| f["kind"] == "artwork_line_clearance")
            .unwrap();
        assert!(
            collision["measurements"]["candidate_clearance_px"]
                .as_f64()
                .unwrap()
                < 0.
        );
        assert!(
            collision["measurements"]["reference_clearance_px"]
                .as_f64()
                .unwrap()
                > 0.
        );
        assert!(
            findings
                .iter()
                .any(|f| f["kind"] == "peer_left_alignment_changed")
        );
        assert_eq!(report["quality_approved"], false);
    }
}

#[test]
fn unchanged_labels_stay_clean_but_rotated_reference_is_not_claimed_measured() {
    let (_dir, ctx, request) = fixture(1, false, false);
    let report = ctx.execute(request.clone()).unwrap();
    assert_eq!(report["label_evidence"]["findings"], json!([]));
    let mut rotated = request;
    rotated["output"] = json!("rotated.png");
    rotated["labels"][0]["rotation"] = json!(20);
    let report = ctx.execute(rotated).unwrap();
    assert!(
        report["label_evidence"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["object_id"] == "caption-0" && f["kind"] == "reference_geometry_uncertain")
    );
}

#[test]
fn reference_underline_is_evidence_not_an_automatic_failure() {
    let (dir, ctx, mut request) = fixture(1, true, false);
    let mut blank = image::open(dir.path().join("blank.png")).unwrap().to_rgb8();
    let mut design = image::open(dir.path().join("design.png"))
        .unwrap()
        .to_rgb8();
    rectangle(&mut blank, 195, 68, 75, 1, Rgb([15, 15, 15]));
    rectangle(&mut design, 195, 68, 75, 1, Rgb([15, 15, 15]));
    blank.save(dir.path().join("blank.png")).unwrap();
    design.save(dir.path().join("design.png")).unwrap();
    request["after"] = json!("design.png");
    let report = ctx.execute(request).unwrap();
    let findings = report["label_evidence"]["findings"].as_array().unwrap();
    let line = findings
        .iter()
        .find(|f| f["kind"] == "artwork_line_clearance")
        .unwrap();
    assert_eq!(
        line["measurements"]["candidate_clearance_px"],
        line["measurements"]["reference_clearance_px"]
    );
    assert!(line.get("classification").is_none());
}

#[test]
fn perpendicular_near_miss_is_measured_and_off_canvas_keeps_context_evidence() {
    let (dir, ctx, mut request) = fixture(1, true, false);
    let mut blank = image::open(dir.path().join("blank.png")).unwrap().to_rgb8();
    let mut design = image::open(dir.path().join("design.png"))
        .unwrap()
        .to_rgb8();
    rectangle(&mut blank, 195, 72, 75, 1, Rgb([15, 15, 15]));
    rectangle(&mut design, 195, 72, 75, 1, Rgb([15, 15, 15]));
    blank.save(dir.path().join("blank.png")).unwrap();
    design.save(dir.path().join("design.png")).unwrap();
    request["after"] = json!("design.png");
    let report = ctx.execute(request.clone()).unwrap();
    assert!(
        report["label_evidence"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["kind"] == "artwork_line_clearance"
                && f["measurements"]["candidate_clearance_px"]
                    .as_f64()
                    .is_some_and(|gap| gap > 0. && gap < 4.))
    );
    request["output"] = json!("off-canvas.png");
    request["labels"][0]["box"] = json!([-2000, -2000, 100, 30]);
    let report = ctx.execute(request).unwrap();
    let finding = report["label_evidence"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["object_id"] == "caption-0")
        .unwrap();
    assert!(ctx.verify(&finding["evidence"]).is_ok());
}

#[test]
fn native_ink_bounds_keep_oversized_frames_out_of_neighbor_clearance_measurement() {
    let (_dir, ctx, mut request) = fixture(1, false, false);
    // The frame includes the next row, but its isolated glyphs do not.
    for (i, y) in [50, 120, 190].into_iter().enumerate() {
        request["labels"][i]["box"] = json!([200, y - 3, 100, 100]);
        request["labels"][i]["native_ink_box"] = json!([200, y, 56, 20]);
    }
    let report = ctx.execute(request.clone()).unwrap();
    assert!(
        report["label_evidence"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["kind"] != "neighbor_label_clearance")
    );
    assert_eq!(
        report["label_evidence"]["labels"][0]["metrics"]["candidate_ink_method"],
        "isolated_native_render"
    );
    request["output"] = json!("neighbors.png");
    request["labels"][1]["native_ink_box"] = json!([200, 72, 56, 20]);
    let report = ctx.execute(request).unwrap();
    let finding = report["label_evidence"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == "neighbor_label_clearance")
        .unwrap();
    assert!(ctx.verify(&finding["evidence"]).is_ok());
    assert_eq!(finding["measurements"]["gap_px"].as_f64(), Some(2.));
}
