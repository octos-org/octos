use octos_pptx::{geometry, relations};
use serde_json::json;

#[test]
fn typographic_range_matches_geometry_without_certifying_copy() {
    let ocr = json!({"detections":[{"text":"12-18 units","confidence":0.8,
        "quad":geometry::frame([100.,80.,200.,30.],-11.)}]});
    let r = relations::reference("12–18 units", [110., 90., 170., 25.], &ocr, [1., 1.]).unwrap();
    assert_eq!(r["partial"], false);
    assert_eq!(r["copy_verified"], false);
    assert!((r["angle_deg"].as_f64().unwrap() + 11.).abs() < 0.001);
    // Numeric changes, punctuation loss and minus operators remain distinct.
    for wrong in ["12–19 units", "1218 units", "12−18 units", "12:18 units"] {
        assert!(relations::reference(wrong, [110., 90., 170., 25.], &ocr, [1., 1.]).is_none());
    }
    let duplicates = json!({"detections":[ocr["detections"][0],ocr["detections"][0]]});
    assert!(
        relations::reference("12–18 units", [110., 90., 170., 25.], &duplicates, [1., 1.])
            .is_none()
    );
}

#[test]
fn colour_surface_measures_tilt_without_page_or_text_rules() {
    let mut im = image::RgbImage::from_pixel(500, 300, image::Rgb([240, 240, 240]));
    let quad = geometry::frame([100., 80., 280., 100.], -9.);
    for (x, y, p) in im.enumerate_pixels_mut() {
        if geometry::contains(quad, [x as f64, y as f64], 0.) {
            *p = image::Rgb([30, 100, 160]);
        }
    }
    let s = relations::surface(&im, [155., 110., 150., 28.]).unwrap();
    assert!((s["angle_deg"].as_f64().unwrap() + 9.).abs() < 0.5);
    assert_eq!(s["semantic_owner_verified"], false);
    // An unbounded background is not a verified label surface.
    let blank = image::RgbImage::from_pixel(500, 300, image::Rgb([240, 240, 240]));
    assert!(relations::surface(&blank, [155., 110., 150., 28.]).is_none());
    assert!(relations::surface(&blank, [-500., -10., 30., 20.]).is_none());
}

#[test]
fn uncertain_irregular_edges_are_not_rectangular_owners() {
    let mut im = image::RgbImage::from_pixel(500, 300, image::Rgb([240, 240, 240]));
    for (x, y, p) in im.enumerate_pixels_mut() {
        let top = 80. + 18. * (x as f64 / 20.).sin();
        if x > 100 && x < 380 && y as f64 > top && y < 200 {
            *p = image::Rgb([70, 80, 10]);
        }
    }
    assert!(relations::surface(&im, [150., 115., 150., 30.]).is_none());
}

#[test]
fn reference_requires_confident_unambiguous_text_and_preserves_angles() {
    let row = json!({"text":"Generic label","confidence":0.95,"quad":geometry::frame([100.,80.,200.,30.],7.)});
    let ocr = json!({"detections":[row]});
    let r = relations::reference("Generic label", [110., 90., 170., 25.], &ocr, [1., 1.]).unwrap();
    assert!((r["angle_deg"].as_f64().unwrap() - 7.).abs() < 0.001);
    assert!(relations::reference("Changed copy", [110., 90., 170., 25.], &ocr, [1., 1.]).is_none());
    let ambiguous = json!({"detections":[row,row]});
    assert!(
        relations::reference(
            "Generic label",
            [110., 90., 170., 25.],
            &ambiguous,
            [1., 1.]
        )
        .is_none()
    );
    let weak = json!({"detections":[{"text":"Generic label","confidence":0.2,"quad":geometry::frame([100.,80.,200.,30.],7.)}]});
    let low =
        relations::reference("Generic label", [110., 90., 170., 25.], &weak, [1., 1.]).unwrap();
    assert_eq!(low["confidence"], 0.2);
    let partial = relations::reference(
        "Generic label\nSecond line",
        [110., 90., 170., 50.],
        &ocr,
        [1., 1.],
    )
    .unwrap();
    assert_eq!(partial["partial"], true);
}

#[test]
fn measured_candidate_cannot_be_skipped_or_admitted_as_defect_then_passed() {
    let signals = vec![
        json!({"id":"generic-signal","page":"any-scene","object_id":"caption","check":"direction","code":"SURFACE_DIRECTION_DIFFERS","observed":{"delta_degrees":9}}),
    ];
    let mut obs = json!({"object_reviews":[{"page":"any-scene","object_id":"caption","checks":{"direction":{"status":"pass"}}}]});
    assert!(relations::validate_responses(&signals, &obs).is_err());
    obs["relation_reviews"] = json!([{"id":"generic-signal","status":"defect","basis":"text baseline conflicts with sign"}]);
    assert!(relations::validate_responses(&signals, &obs).is_err());
    obs["object_reviews"][0]["checks"]["direction"]["status"] = json!("defect");
    assert!(relations::validate_responses(&signals, &obs).is_ok());
    // Deliberately horizontal overprinted caption is a valid design choice.
    obs["relation_reviews"][0]["status"] = json!("intentional");
    obs["relation_reviews"][0]["basis"] =
        json!("This is an external horizontal caption, not text printed on the tilted sign.");
    obs["object_reviews"][0]["checks"]["direction"]["status"] = json!("pass");
    // A prose claim is no longer sufficient to establish that intent.
    assert!(relations::validate_responses(&signals, &obs).is_err());
    let mut supported = signals.clone();
    let options = octos_pptx::intent_evidence::options(
        "SURFACE_DIRECTION_DIFFERS",
        &json!({}),
        ([100., 100., 150., 25.], 0.),
        &json!({"angle_deg":-9}),
        &json!({"angle_deg":0,"quad":geometry::frame([100.,100.,150.,25.],0.)}),
        &[],
        true,
    );
    obs["relation_reviews"][0]["support_id"] = options[0]["id"].clone();
    supported[0]["intentional_options"] = json!(options);
    assert!(relations::validate_responses(&supported, &obs).is_ok());
    obs["relation_reviews"][0]["id"] = json!("stale");
    assert!(relations::validate_responses(&signals, &obs).is_err());
}
