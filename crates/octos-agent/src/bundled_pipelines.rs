//! Embedded generic pipeline `.dot` files that ship inside the `octos` binary.
//!
//! Load-bearing, generic pipelines (e.g. `deep_research`) must never depend on
//! a per-profile skill being deployed — skill drift on a fleet host silently
//! turned `run_pipeline deep_research` into `Available: (none)` during a live
//! soak. Bundling the canonical `.dot` into the binary and writing it to the
//! dedicated `<octos_home>/bundled-pipelines/` dir on bootstrap (see
//! [`super::bootstrap`]) guarantees the generic pipelines are always
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_pipelines_is_non_empty() {
        assert!(!BUNDLED_PIPELINES.is_empty());
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
}
