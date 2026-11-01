#[test]
fn transport_fixtures_are_opt_in_and_add_only_api() {
    let manifest: toml::Value =
        toml::from_str(include_str!("../Cargo.toml")).expect("octos-cli Cargo.toml must parse");
    let features = manifest
        .get("features")
        .and_then(toml::Value::as_table)
        .expect("features table");

    let fixture = features
        .get("transport-test-fixtures")
        .and_then(toml::Value::as_array)
        .expect("transport-test-fixtures feature");
    assert_eq!(fixture.len(), 1, "fixture feature adds no dependencies");
    assert_eq!(fixture[0].as_str(), Some("api"));

    let defaults = features
        .get("default")
        .and_then(toml::Value::as_array)
        .expect("default features");
    assert!(
        !defaults
            .iter()
            .any(|feature| feature.as_str() == Some("transport-test-fixtures")),
        "production defaults must not enable deterministic transport fixtures"
    );
}
