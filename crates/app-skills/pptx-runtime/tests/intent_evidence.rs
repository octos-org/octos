use octos_pptx::{geometry, intent_evidence as intent, relations};
use serde_json::json;

fn reference(b: [f64; 4]) -> serde_json::Value {
    json!({"partial":false,"quad":geometry::frame(b,0.),"angle_deg":0})
}

#[test]
fn keeping_left_edge_cannot_excuse_lost_reference_center() {
    let surface = json!({"quad":geometry::frame([100.,80.,400.,100.],0.),"angle_deg":0});
    let original = reference([150., 110., 300., 40.]);
    let ink = [150., 115., 150., 30.];
    let centers = intent::centers(ink, &original, &surface, true);
    let x = centers.iter().find(|r| r["axis"] == "x").unwrap();
    assert_eq!(x["reference_offset_px"], 0.);
    assert_eq!(x["current_offset_px"], -75.);
    assert_eq!(x["changed"], true);
    let options = intent::options(
        "REFERENCE_ANCHOR_DIFFERS",
        &json!({"delta_px":[-75,0]}),
        (ink, 0.),
        &surface,
        &original,
        &centers,
        true,
    );
    assert!(options.is_empty());
    let fixed = intent::centers([225., 115., 150., 30.], &original, &surface, true);
    assert!(fixed.iter().all(|r| r["changed"] == false));
}

#[test]
fn genuine_left_anchor_and_horizontal_caption_are_supported() {
    let surface = json!({"quad":geometry::frame([100.,80.,400.,100.],0.),"angle_deg":0});
    let original = reference([120., 115., 140., 30.]);
    let ink = [120., 115., 70., 30.];
    let centers = intent::centers(ink, &original, &surface, true);
    let options = intent::options(
        "REFERENCE_ANCHOR_DIFFERS",
        &json!({"delta_px":[-35,0]}),
        (ink, 0.),
        &surface,
        &original,
        &centers,
        true,
    );
    assert_eq!(options.len(), 1);
    assert_eq!(options[0]["measured"]["axes"][0]["anchor"], "left/top");
    let h = intent::options(
        "SURFACE_DIRECTION_DIFFERS",
        &json!({}),
        (ink, 0.),
        &json!({"angle_deg":-8}),
        &original,
        &centers,
        true,
    );
    assert_eq!(h.len(), 1);
    assert_eq!(h[0]["kind"], "preserved_reference_direction");
    let wrong_axis = intent::options(
        "REFERENCE_ANCHOR_DIFFERS",
        &json!({"delta_px":[-35,30]}),
        ([120., 145., 70., 30.], 0.),
        &surface,
        &original,
        &centers,
        true,
    );
    assert!(wrong_axis.is_empty());
}

#[test]
fn incomplete_reference_or_unregistered_artwork_cannot_invent_center_contract() {
    let surface = json!({"quad":geometry::frame([100.,80.,400.,100.],0.),"angle_deg":0});
    let mut original = reference([150., 110., 300., 40.]);
    let ink = [150., 115., 150., 30.];
    assert!(intent::centers(ink, &original, &surface, false).is_empty());
    assert!(
        intent::options(
            "REFERENCE_ANCHOR_DIFFERS",
            &json!({"delta_px":[-75,0]}),
            (ink, 0.),
            &surface,
            &original,
            &[],
            false
        )
        .is_empty()
    );
    original["partial"] = json!(true);
    assert!(intent::centers(ink, &original, &surface, true).is_empty());
    assert!(
        intent::options(
            "REFERENCE_ANCHOR_DIFFERS",
            &json!({"delta_px":[-75,0]}),
            (ink, 0.),
            &surface,
            &original,
            &[],
            true
        )
        .is_empty()
    );
}

#[test]
fn direction_and_position_evidence_are_not_interchangeable() {
    let signals = vec![
        json!({"id":"a","page":"scene","object_id":"label","check":"alignment","code":"REFERENCE_CENTER_RELATION_CHANGED","intentional_options":[]}),
        json!({"id":"b","page":"scene","object_id":"caption","check":"direction","code":"SURFACE_DIRECTION_DIFFERS","intentional_options":[{"id":"direction-support"}]}),
    ];
    let response = json!({"relation_reviews":[{"id":"a","status":"intentional","support_id":"direction-support","basis":"Both are horizontal"}]});
    let err = relations::validate_responses(&signals, &response)
        .unwrap_err()
        .to_string();
    assert!(err.contains("intentional requires a support_id"));
    assert!(err.contains("requires exactly one disposition")); // all issues returned together
    let unknown = json!({"relation_reviews":[{"id":"a","status":"unknown","basis":"Ownership unresolved"},{"id":"b","status":"intentional","support_id":"direction-support","basis":"Original caption is horizontal"}],
        "object_reviews":[{"page":"scene","object_id":"label","checks":{"alignment":{"status":"unknown"}}}]});
    assert!(relations::validate_responses(&signals, &unknown).is_ok());
}
