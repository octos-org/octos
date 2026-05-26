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
//!
//! The bin defers to `PluginManifest::from_file()` so structural,
//! schema, and env-profile checks all run from the same code path the
//! daemon uses at startup. This means a manifest that passes here is
//! guaranteed to load at runtime (modulo external factors like
//! missing binaries, which gating handles separately).

use std::path::Path;
use std::process::ExitCode;

use octos_plugin::{PluginManifest, ValidationProfile};

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
        match validate_one(path) {
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

/// Validate one manifest file. We use `PluginManifest::from_file()`
/// which runs structural validation (`id`, `version`, tool names,
/// `type: "tool"` requires `tools`, `type: "hook"` requires `hooks`),
/// schema validation (Draft 07 sanity + the strict octos profile),
/// and honours `OCTOS_MANIFEST_VALIDATION`. Re-using this entrypoint
/// means CI cannot drift from runtime — codex review (2026-05-25, P3)
/// flagged a hand-rolled structural copy that omitted the type/tools
/// and type/hooks checks; we now share the canonical path.
fn validate_one(path: &Path) -> Result<(), String> {
    PluginManifest::from_file(path)
        .map(|_| ())
        .map_err(|e| format!("    {e:#}"))
}
