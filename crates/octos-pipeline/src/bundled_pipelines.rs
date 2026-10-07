//! Legacy pipeline fixtures. This crate is not linked into the Octos runtime.
//!
//! Load-bearing, generic pipelines (e.g. `deep_research`) must never depend on
//! a per-profile skill being deployed — skill drift on a fleet host silently
//! turned `run_pipeline deep_research` into `Available: (none)` during a live
//! soak. Bundling the canonical `.dot` into the binary and writing it to the
//! dedicated `<octos_home>/bundled-pipelines/` dir on bootstrap (see
//! [`bootstrap_bundled_pipelines`]) guarantees the generic pipelines are always
//! discoverable, while still letting an installed copy of the same name win
//! (that dir is searched LAST, and the bootstrap never overwrites an existing
//! file).
//!
//! Each entry is `(file_name, dot_contents)`. Bundle ONLY generic /
//! load-bearing pipelines here — profile-specific pipelines stay in their
//! skill packages.

/// `(file_name, dot_contents)` for each bundled generic pipeline.
///
/// `file_name` includes the `.dot` extension; it is written verbatim under
/// the dedicated `<octos_home>/bundled-pipelines/` dir. The pipeline's
/// discoverable *name* is the file stem (`deep_research.dot` → `deep_research`).
pub const BUNDLED_PIPELINES: &[(&str, &str)] = &[(
    "deep_research.dot",
    include_str!("assets/pipelines/deep_research.dot"),
)];

/// `(pipeline_name, ir_json)` for each bundled generic pipeline rebuilt as a
/// capability-locked typed-IR program. These are the CANONICAL sanctioned
/// pipelines: `run_pipeline` resolves a bare name to the IR here (composed via
/// the safe palette) IN PREFERENCE to the embedded `.dot`, so the shipped
/// `deep_research` runs the audited IR rather than raw DOT. An operator-INSTALLED
/// pipeline of the same name in a skill dir still wins (installed-wins).
pub const BUNDLED_IR_PIPELINES: &[(&str, &str)] = &[(
    "deep_research",
    include_str!("assets/pipelines/deep_research.ir.json"),
)];

/// Embedded IR JSON for a bundled pipeline `name` (the file stem), if any.
pub fn bundled_ir(name: &str) -> Option<&'static str> {
    BUNDLED_IR_PIPELINES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, ir)| *ir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_pipelines_is_non_empty() {
        assert_ne!(BUNDLED_PIPELINES.len(), 0);
    }

    #[test]
    fn bundled_pipelines_entries_have_dot_extension_and_content() {
        for &(file_name, dot) in BUNDLED_PIPELINES {
            assert!(
                file_name.ends_with(".dot"),
                "bundled pipeline file_name '{file_name}' must end with .dot"
            );
            assert!(!dot.is_empty(), "bundled pipeline '{file_name}' is empty");
            assert!(
                dot.contains("digraph"),
                "bundled pipeline '{file_name}' must contain a digraph"
            );
        }
    }

    #[test]
    fn bundled_pipelines_includes_deep_research() {
        assert!(
            BUNDLED_PIPELINES
                .iter()
                .any(|(name, _)| *name == "deep_research.dot"),
            "deep_research.dot (the load-bearing generic pipeline) must be bundled"
        );
    }

    #[test]
    fn bundled_ir_includes_deep_research_as_valid_json() {
        let ir = bundled_ir("deep_research").expect("deep_research IR must be bundled");
        let v: serde_json::Value = serde_json::from_str(ir).expect("bundled IR must be valid JSON");
        assert_eq!(v["id"], "deep_research");
        assert!(v["nodes"].as_array().is_some_and(|n| !n.is_empty()));
        assert!(bundled_ir("nonexistent").is_none());
    }
}

use std::path::Path;

/// Subdirectory name for bundled generic pipelines.
///
/// Gap 4.1 BLOCKER 3 (installed-wins precedence): the bundled `.dot` files
/// live in their OWN directory, deliberately SEPARATE from the user-pipeline
/// dir (`<root>/pipelines`). `octos_pipeline::discovery::PipelineDiscovery`
/// searches this dir at the LOWEST precedence (after every installed-skill /
/// installed-pipeline location), so an installed `deep_research.dot` — whether
/// in `<data>/pipelines`, `<data>/skills/<x>/`, `<octos_home>/skills/<x>/`, or
/// `<octos_home>/pipelines` — ALWAYS wins over the bundled fallback.
///
/// `RunPipelineTool::with_octos_home` appends `<octos_home>/{BUNDLED_PIPELINES_DIR}`
/// as the final search path, so anything written here is discoverable by
/// `run_pipeline` but never shadows an installed copy.
pub const BUNDLED_PIPELINES_DIR: &str = "bundled-pipelines";

/// Bootstrap bundled generic pipelines into `<octos_home>/bundled-pipelines/`.
///
/// Writes each embedded `.dot` (see [`crate::bundled_pipelines`]) so that
/// load-bearing generic pipelines (e.g. `deep_research`) are always
/// discoverable by `run_pipeline`, independent of any per-profile skill
/// deployment. Skill drift on a fleet host previously turned
/// `run_pipeline deep_research` into `Available: (none)`; bundling the `.dot`
/// into the binary closes that gap.
///
/// **Precedence (installed-wins):** the bundled dir is searched LAST (see
/// [`BUNDLED_PIPELINES_DIR`]), so an operator- or skill-installed pipeline of
/// the same name always wins over the bundled fallback. We also never clobber
/// an already-present file of the same name within the bundled dir itself.
///
/// Idempotent: returns the number of `.dot` files newly written.
///
/// NIT 1 (atomic no-clobber): the write uses
/// `OpenOptions::create_new(true)` so the "is this already installed?"
/// check and the write are a single atomic syscall — a concurrent
/// installer racing the bootstrap can never have its file clobbered
/// (the `AlreadyExists` error is treated as "skip").
pub fn bootstrap_bundled_pipelines(octos_home: &Path) -> usize {
    let target_dir = octos_home.join(BUNDLED_PIPELINES_DIR);

    if std::fs::create_dir_all(&target_dir).is_err() {
        return 0;
    }

    let mut count = 0;
    for &(file_name, dot_contents) in BUNDLED_PIPELINES {
        let dest = target_dir.join(file_name);

        // NIT 1: atomic no-clobber. `create_new(true)` fails with
        // `AlreadyExists` rather than truncating an existing file, closing
        // the exists()-then-write() TOCTOU window. A concurrent install
        // that wrote the same path first is preserved (installed-wins).
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
        {
            Ok(mut f) => {
                use std::io::Write as _;
                if f.write_all(dot_contents.as_bytes()).is_ok() {
                    count += 1;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Installed-wins: a file already exists, leave it untouched.
            }
            Err(_) => {
                // Other I/O error (permissions, etc.) — skip silently,
                // matching the prior best-effort behaviour.
            }
        }
    }

    count
}

#[cfg(test)]
mod bootstrap_tests {
    use super::*;
    #[test]
    fn bootstrap_bundled_pipelines_writes_deep_research_dot() {
        let tmp = tempfile::tempdir().unwrap();
        let octos_home = tmp.path();

        let count = bootstrap_bundled_pipelines(octos_home);
        assert!(count >= 1, "at least deep_research must be bootstrapped");

        let dot = octos_home
            .join(BUNDLED_PIPELINES_DIR)
            .join("deep_research.dot");
        assert!(
            dot.exists(),
            "bootstrap must write deep_research.dot into <octos_home>/bundled-pipelines"
        );
        let body = std::fs::read_to_string(&dot).unwrap();
        assert!(
            body.contains("digraph deep_research"),
            "written file must be the canonical deep_research pipeline"
        );
    }

    #[test]
    fn bootstrap_bundled_pipelines_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let octos_home = tmp.path();

        let first = bootstrap_bundled_pipelines(octos_home);
        assert!(first >= 1);
        // Second run: everything already present, nothing newly written.
        let second = bootstrap_bundled_pipelines(octos_home);
        assert_eq!(second, 0, "second bootstrap must be a no-op (idempotent)");
    }

    #[test]
    fn bootstrap_bundled_pipelines_writes_into_dedicated_bundled_dir() {
        // BLOCKER 3: the bundle must land in the DEDICATED bundled-pipelines
        // dir (searched last), NOT the user-pipeline dir `<root>/pipelines`
        // (which precedes `<root>/skills` and would shadow installs).
        let tmp = tempfile::tempdir().unwrap();
        let octos_home = tmp.path();

        bootstrap_bundled_pipelines(octos_home);
        assert_eq!(BUNDLED_PIPELINES_DIR, "bundled-pipelines");
        assert!(
            octos_home
                .join("bundled-pipelines")
                .join("deep_research.dot")
                .exists(),
            "bundle must be written to the dedicated <root>/bundled-pipelines dir"
        );
        assert!(
            !octos_home
                .join("pipelines")
                .join("deep_research.dot")
                .exists(),
            "bundle must NOT be written to <root>/pipelines (would shadow installs)"
        );
    }

    #[test]
    fn bootstrap_bundled_pipelines_create_new_preserves_concurrent_install() {
        // NIT 1: the write is atomic (`create_new`), so a file that already
        // exists (e.g. an installer wrote it first in a race) is preserved
        // byte-for-byte and NOT counted as newly written.
        let tmp = tempfile::tempdir().unwrap();
        let octos_home = tmp.path();
        let bundled_dir = octos_home.join(BUNDLED_PIPELINES_DIR);
        std::fs::create_dir_all(&bundled_dir).unwrap();

        let racing = bundled_dir.join("deep_research.dot");
        let racing_body = "digraph deep_research { concurrent_install [prompt=\"race\"] }";
        std::fs::write(&racing, racing_body).unwrap();

        let count = bootstrap_bundled_pipelines(octos_home);
        assert_eq!(
            count, 0,
            "an already-present (concurrently installed) file must NOT be clobbered or counted"
        );
        assert_eq!(
            std::fs::read_to_string(&racing).unwrap(),
            racing_body,
            "atomic create_new must preserve the racing installer's bytes"
        );
    }

    #[test]
    fn bootstrap_bundled_pipelines_does_not_clobber_installed_pipeline() {
        // Precedence contract: an already-present (installed) pipeline of the
        // same name must WIN over the bundled fallback — bootstrap must not
        // overwrite it.
        let tmp = tempfile::tempdir().unwrap();
        let octos_home = tmp.path();
        let pipelines_dir = octos_home.join(BUNDLED_PIPELINES_DIR);
        std::fs::create_dir_all(&pipelines_dir).unwrap();

        let installed = pipelines_dir.join("deep_research.dot");
        let installed_body = "digraph deep_research { installed [prompt=\"custom\"] }";
        std::fs::write(&installed, installed_body).unwrap();

        let count = bootstrap_bundled_pipelines(octos_home);
        // deep_research was already present, so it is NOT counted/written.
        // (Other bundled pipelines, if any, may still be written.)
        let after = std::fs::read_to_string(&installed).unwrap();
        assert_eq!(
            after, installed_body,
            "installed deep_research.dot must NOT be overwritten by the bundled fallback"
        );
        assert_eq!(
            count, 0,
            "no bundled pipeline should be written when all names are already installed"
        );
    }
}
