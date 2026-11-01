use std::fs;
use std::path::Path;

#[test]
fn should_exclude_bus_from_normal_dependencies_when_building_agent() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path).expect("read octos-agent manifest");
    let normal_dependencies = manifest
        .split("[dev-dependencies]")
        .next()
        .expect("manifest has a dependency section");
    assert!(
        !normal_dependencies.contains("octos-bus ="),
        "octos-agent must consume shared DTOs from octos-core, not octos-bus"
    );
}
