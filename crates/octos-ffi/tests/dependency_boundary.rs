use std::fs;
use std::path::Path;

#[test]
fn should_exclude_cli_from_all_dependencies_when_building_ffi() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path).expect("read octos-ffi manifest");
    assert!(
        !manifest.contains("octos-cli ="),
        "octos-ffi must consume config, auth, and factories from lower-level crates"
    );
}
