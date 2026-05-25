//! CI helper: validate one or more plugin `manifest.json` files
//! against the RFC-2 schema validator.
//!
//! Usage:
//!
//! ```text
//! validate_manifests path/to/manifest.json [more.json …]
//! ```
//!
//! Exits with code 0 when every manifest passes, 1 otherwise. The
//! `scripts/validate-skill-manifests.sh` wrapper drives this binary
//! from CI and from local dev workflows.

use std::path::Path;
use std::process::ExitCode;

use octos_plugin::{PluginManifest, ValidationProfile, validate_manifest_schemas_with};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: validate_manifests <manifest.json> [<manifest.json> ...]");
        return ExitCode::from(2);
    }

    let profile = ValidationProfile::from_env();
    eprintln!("RFC-2 manifest validator (profile: {profile:?})\n");

    let mut failures: usize = 0;
    let mut pass: usize = 0;
    for arg in &args {
        let path = Path::new(arg);
        match validate_one(path, profile) {
            Ok(()) => {
                eprintln!("  PASS  {}", path.display());
                pass += 1;
            }
            Err(msg) => {
                eprintln!("  FAIL  {}", path.display());
                eprintln!("{msg}");
                failures += 1;
            }
        }
    }

    eprintln!(
        "\n{pass} pass, {failures} fail (of {} manifests)",
        args.len()
    );
    if failures > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}

fn validate_one(path: &Path, profile: ValidationProfile) -> Result<(), String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("    read error: {e}"))?;
    // Parse without going through `PluginManifest::from_json` so we
    // can run the validator explicitly with the requested profile,
    // matching the env var the wrapper script passes through.
    //
    // We still want structural manifest checks (id/version/tool names)
    // — those live in `validate()` which `from_json` calls. So we
    // try `from_json` first; if it fails for a non-validation reason
    // we surface it; if it fails due to schema validation under the
    // profile we report that with the structured detail.
    let manifest: PluginManifest = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(e) => return Err(format!("    parse error: {e}")),
    };

    // Re-run structural checks (id, version, tools shape) since we
    // bypassed `from_json`.
    if let Err(e) = manifest_struct_check(&manifest) {
        return Err(format!("    structural error: {e}"));
    }

    match validate_manifest_schemas_with(&manifest, profile) {
        Ok(()) => Ok(()),
        Err(errs) => {
            let detail = errs
                .iter()
                .map(|e| format!("      - {e}"))
                .collect::<Vec<_>>()
                .join("\n");
            Err(format!("    {} schema violation(s):\n{detail}", errs.len()))
        }
    }
}

/// Re-implementation of the structural-only piece of
/// `PluginManifest::validate()` (which is private). Kept tiny on
/// purpose; the schema validator owns the heavy lifting.
fn manifest_struct_check(manifest: &PluginManifest) -> Result<(), String> {
    if manifest.id.is_empty() {
        return Err("manifest 'id' (or 'name') must not be empty".into());
    }
    if manifest.version.is_empty() {
        return Err("manifest 'version' must not be empty".into());
    }
    for tool in &manifest.tools {
        if tool.name.is_empty() {
            return Err(format!(
                "tool in plugin '{}' has an empty name",
                manifest.id
            ));
        }
    }
    Ok(())
}
