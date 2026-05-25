//! RFC-2: every bundled `crates/app-skills/*/manifest.json` must pass
//! the strict manifest validator. If any of these regress, the daemon
//! refuses to load them at startup — so a failure here is a release
//! blocker, not a soft warning.
//!
//! This is also exactly what `scripts/validate-skill-manifests.sh` runs
//! in CI via the `validate_manifests` bin. The integration test layer
//! gives us a `cargo test`-friendly hook so editor-driven workflows
//! catch a regression before the operator ever sees the CI failure.

use std::fs;
use std::path::PathBuf;

use octos_plugin::{PluginManifest, ValidationProfile, validate_manifest_schemas_with};

fn app_skills_dir() -> PathBuf {
    // Tests run with CWD = crate root (octos-plugin), so we go up two
    // levels to reach the workspace root and dive into app-skills.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir).join("..").join("app-skills")
}

#[test]
fn bundled_app_skills_manifests_pass_strict_validator() {
    let dir = app_skills_dir();
    assert!(
        dir.exists(),
        "bundled app-skills dir not found at {}",
        dir.display()
    );

    let mut visited = 0;
    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    for entry in fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }
        visited += 1;
        let raw = fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("could not read {}: {e}", manifest_path.display()));
        let manifest = match PluginManifest::from_json(&raw) {
            Ok(m) => m,
            Err(e) => {
                failures.push((manifest_path, format!("{e:#}")));
                continue;
            }
        };
        if let Err(errs) = validate_manifest_schemas_with(&manifest, ValidationProfile::Strict) {
            failures.push((
                manifest_path,
                errs.iter()
                    .map(|e| format!("  - {e}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
        }
    }

    assert!(
        visited > 0,
        "no bundled manifests found under {}",
        dir.display()
    );
    if !failures.is_empty() {
        let detail = failures
            .iter()
            .map(|(p, m)| format!("{}\n{m}", p.display()))
            .collect::<Vec<_>>()
            .join("\n\n");
        panic!(
            "{} bundled manifest(s) failed strict validation:\n\n{detail}",
            failures.len()
        );
    }
}
