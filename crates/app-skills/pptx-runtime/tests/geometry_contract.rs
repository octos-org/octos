//! Deck-independent contracts of the shared label_geometry helper: rotate about
//! the centre, inset into the actual polygon, and report non-rigid planes.
use octos_pptx::geometry;
use serde_json::json;

#[test]
fn inset_preserves_local_angle_and_padding_across_sizes_and_rotations() {
    for (w, h) in [(180., 60.), (60., 180.), (120., 120.)] {
        for angle in [-25., 0., 18., 123., 179.] {
            let original = [210., 140., w, h];
            let q = geometry::frame(original, angle);
            let proposal = geometry::proposal(&json!({"region":q,"padding":6.})).unwrap();
            let b: [f64; 4] = serde_json::from_value(proposal["box"].clone()).unwrap();
            let actual_angle = proposal["rotation"].as_f64().unwrap();
            assert!(geometry::angle(actual_angle, angle).abs() < 1e-9);
            let scale = (1. - 12. / w).min(1. - 12. / h);
            assert!((b[2] - w * scale).abs() < 1e-5);
            assert!((b[3] - h * scale).abs() < 1e-5);
            assert!(
                geometry::frame(b, actual_angle)
                    .iter()
                    .all(|p| geometry::contains(q, *p, -5.99999))
            );
            assert_eq!(proposal["perspective_or_shear"], false);
        }
    }
}

#[test]
fn trapezoid_is_not_misreported_as_exact_rotation() {
    let q = [[10., 20.], [270., 40.], [230., 110.], [35., 105.]];
    let proposal = geometry::proposal(&json!({"region":q,"padding":5.})).unwrap();
    assert_eq!(proposal["perspective_or_shear"], true);
    assert_eq!(proposal["visual_review_required"], true);
    let b = serde_json::from_value(proposal["box"].clone()).unwrap();
    let angle = proposal["rotation"].as_f64().unwrap();
    assert!(
        geometry::frame(b, angle)
            .iter()
            .all(|p| geometry::contains(q, *p, -4.99999))
    );
}
