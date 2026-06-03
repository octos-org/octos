//! Pipeline file discovery — finds .dot pipeline files from standard locations.

use std::path::{Path, PathBuf};

use eyre::Result;

/// Information about a discoverable pipeline.
pub struct PipelineInfo {
    pub name: String,
    pub path: PathBuf,
}

/// Subdirectory name (under an octos root) where the binary writes its
/// embedded generic pipelines. Mirrors `octos_agent::bootstrap::BUNDLED_PIPELINES_DIR`.
///
/// Gap 4.1 BLOCKER 3: this is a DEDICATED dir, separate from the
/// user-pipeline dir (`<root>/pipelines`), and is always searched LAST so
/// an installed pipeline of the same name wins over the bundled fallback.
pub const BUNDLED_PIPELINES_DIR: &str = "bundled-pipelines";

/// Discovers pipeline files from standard locations.
pub struct PipelineDiscovery {
    /// Ordered, first-found-wins search paths for installed pipelines /
    /// skills. NEVER contains the bundled-pipelines dir — that is held
    /// separately and materialized LAST (see `bundled_dirs`).
    search_paths: Vec<PathBuf>,
    /// Bundled-pipelines dirs (lowest precedence). Always appended AFTER
    /// every `search_paths` entry when resolving / listing, regardless of
    /// the order `with_octos_home` / `add_bundled_pipelines_dir` are
    /// called — so an installed `deep_research.dot` always shadows the
    /// bundled copy (installed-wins, BLOCKER 3).
    bundled_dirs: Vec<PathBuf>,
}

impl PipelineDiscovery {
    pub fn new(data_dir: &Path, working_dir: &Path) -> Self {
        Self {
            search_paths: vec![
                // Project-level pipelines
                working_dir.join(".octos").join("pipelines"),
                // User-level pipelines
                data_dir.join("pipelines"),
                // Installed skills (each skill dir may contain .dot files)
                data_dir.join("skills"),
            ],
            bundled_dirs: Vec::new(),
        }
    }

    /// Add an installed-pipeline / installed-skill search path (e.g. global
    /// `octos_home/skills/`). These are searched at HIGHER precedence than
    /// any bundled-pipelines dir.
    pub fn add_search_path(&mut self, path: PathBuf) {
        if !self.search_paths.contains(&path) {
            self.search_paths.push(path);
        }
    }

    /// Register `<root>/bundled-pipelines` as a LOWEST-precedence search
    /// path. Held separately from `search_paths` so it is materialized
    /// LAST during resolution no matter the builder call order — this is
    /// the BLOCKER 3 installed-wins guarantee.
    pub fn add_bundled_pipelines_dir(&mut self, root: &Path) {
        let dir = root.join(BUNDLED_PIPELINES_DIR);
        if !self.bundled_dirs.contains(&dir) {
            self.bundled_dirs.push(dir);
        }
    }

    /// All search dirs in precedence order: installed locations first,
    /// then the bundled-pipelines dirs (lowest precedence). First-found
    /// wins, so installed copies always shadow the bundled fallback.
    fn ordered_search_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.search_paths.iter().chain(self.bundled_dirs.iter())
    }

    /// List all discoverable pipelines.
    pub fn list_available(&self) -> Vec<PipelineInfo> {
        let mut pipelines = Vec::new();

        for dir in self.ordered_search_paths() {
            // Direct .dot files in the directory
            scan_dot_files(dir, &mut pipelines);

            // Also scan one level deeper (skills/<name>/*.dot)
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let sub = entry.path();
                    if sub.is_dir() {
                        scan_dot_files(&sub, &mut pipelines);
                    }
                }
            }
        }

        pipelines
    }

    /// Resolve a pipeline name, path, or inline DOT content to its DOT string.
    pub async fn resolve(&self, name_or_path: &str) -> Result<String> {
        // 0. Check if it's inline DOT content (starts with "digraph")
        let trimmed = name_or_path.trim();
        if trimmed.starts_with("digraph ") || trimmed.starts_with("digraph{") {
            return Ok(name_or_path.to_string());
        }

        // 1. Check if it's a direct file path
        let as_path = PathBuf::from(name_or_path);
        if as_path.exists() && as_path.extension().is_some_and(|e| e == "dot") {
            return tokio::fs::read_to_string(&as_path)
                .await
                .map_err(|e| eyre::eyre!("failed to read pipeline file: {e}"));
        }

        // 2. Check if it's a relative path like "mofa-research/deep_research.dot".
        //    Only INSTALLED search paths participate in this direct-path
        //    short-circuit — the bundled dirs are deliberately excluded so a
        //    bundled `deep_research.dot` (a direct file) can never out-race a
        //    nested installed `skills/<x>/deep_research.dot` (BLOCKER 3
        //    installed-wins). Bundled pipelines are resolved by bare name in
        //    step 3 via `list_available`, where the ordered scan keeps them
        //    lowest precedence.
        for dir in &self.search_paths {
            let candidate = dir.join(name_or_path);
            if candidate.exists() {
                return tokio::fs::read_to_string(&candidate)
                    .await
                    .map_err(|e| eyre::eyre!("failed to read pipeline file: {e}"));
            }
            // Try with .dot extension
            let with_ext = dir.join(format!("{name_or_path}.dot"));
            if with_ext.exists() {
                return tokio::fs::read_to_string(&with_ext)
                    .await
                    .map_err(|e| eyre::eyre!("failed to read pipeline file: {e}"));
            }
        }

        // 3. Search by bare name across all paths (including nested skill dirs)
        let all = self.list_available();
        for info in &all {
            if info.name == name_or_path {
                return tokio::fs::read_to_string(&info.path)
                    .await
                    .map_err(|e| eyre::eyre!("failed to read pipeline file: {e}"));
            }
        }

        let available: Vec<_> = all.iter().map(|p| p.name.as_str()).collect();
        eyre::bail!(
            "pipeline '{}' not found. Available: {}",
            name_or_path,
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        )
    }
}

fn scan_dot_files(dir: &Path, pipelines: &mut Vec<PipelineInfo>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "dot") {
                let name = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if !pipelines.iter().any(|p| p.name == name) {
                    pipelines.push(PipelineInfo { name, path });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_resolve_inline_dot() {
        let discovery = PipelineDiscovery::new(Path::new("/tmp"), Path::new("/tmp"));
        let dot = "digraph test { a [prompt=\"hello\"] }";
        let result = discovery.resolve(dot).await.unwrap();
        assert_eq!(result, dot);
    }

    #[tokio::test]
    async fn should_resolve_inline_dot_with_whitespace() {
        let discovery = PipelineDiscovery::new(Path::new("/tmp"), Path::new("/tmp"));
        let dot = "  digraph research {\n  search -> analyze\n}";
        let result = discovery.resolve(dot).await.unwrap();
        assert_eq!(result, dot);
    }

    /// Gap 4.1 BLOCKER 3 (installed-wins) — an installed `deep_research.dot`
    /// in a skills dir MUST shadow the bundled copy. RED before the fix:
    /// the bundled dir was `<data>/pipelines`, searched BEFORE `<data>/skills`,
    /// so the bundled copy won. After: bundled dirs are a separate,
    /// lowest-precedence search path appended LAST, so the installed copy
    /// always resolves.
    #[tokio::test]
    async fn installed_skill_pipeline_wins_over_bundled() {
        let data = tempfile::tempdir().unwrap();
        let working = tempfile::tempdir().unwrap();

        // Bundled fallback written by bootstrap.
        let bundled_dir = data.path().join(BUNDLED_PIPELINES_DIR);
        std::fs::create_dir_all(&bundled_dir).unwrap();
        std::fs::write(
            bundled_dir.join("deep_research.dot"),
            "digraph deep_research { bundled [prompt=\"BUNDLED\"] }",
        )
        .unwrap();

        // Installed skill copy of the SAME pipeline name.
        let skill_dir = data.path().join("skills").join("mofa-research");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("deep_research.dot"),
            "digraph deep_research { installed [prompt=\"INSTALLED\"] }",
        )
        .unwrap();

        let mut discovery = PipelineDiscovery::new(data.path(), working.path());
        discovery.add_bundled_pipelines_dir(data.path());

        let resolved = discovery.resolve("deep_research").await.unwrap();
        assert!(
            resolved.contains("INSTALLED"),
            "installed skill deep_research.dot must win over the bundled copy, got: {resolved}"
        );
        assert!(
            !resolved.contains("BUNDLED"),
            "bundled copy must NOT shadow an installed pipeline of the same name"
        );
    }

    /// Installed-wins must hold regardless of builder call order: even if
    /// the bundled dir is registered FIRST, then an octos_home/skills path
    /// is added later, the bundled dir stays lowest-precedence.
    #[tokio::test]
    async fn bundled_dir_stays_lowest_precedence_regardless_of_call_order() {
        let data = tempfile::tempdir().unwrap();
        let working = tempfile::tempdir().unwrap();
        let octos_home = tempfile::tempdir().unwrap();

        let bundled_dir = data.path().join(BUNDLED_PIPELINES_DIR);
        std::fs::create_dir_all(&bundled_dir).unwrap();
        std::fs::write(
            bundled_dir.join("deep_research.dot"),
            "digraph deep_research { bundled [prompt=\"BUNDLED\"] }",
        )
        .unwrap();

        let home_skills = octos_home.path().join("skills").join("mofa-research");
        std::fs::create_dir_all(&home_skills).unwrap();
        std::fs::write(
            home_skills.join("deep_research.dot"),
            "digraph deep_research { installed [prompt=\"INSTALLED\"] }",
        )
        .unwrap();

        let mut discovery = PipelineDiscovery::new(data.path(), working.path());
        // Bundled FIRST, installed search path SECOND — the bundled dir
        // must still lose.
        discovery.add_bundled_pipelines_dir(data.path());
        discovery.add_search_path(octos_home.path().join("skills"));

        let resolved = discovery.resolve("deep_research").await.unwrap();
        assert!(
            resolved.contains("INSTALLED"),
            "bundled dir registered first must still be lowest precedence, got: {resolved}"
        );
    }

    /// When ONLY the bundled copy exists, it must still resolve — the
    /// fallback is the whole point of bundling (no-discovery → still
    /// runnable).
    #[tokio::test]
    async fn bundled_pipeline_resolves_when_no_installed_copy() {
        let data = tempfile::tempdir().unwrap();
        let working = tempfile::tempdir().unwrap();

        let bundled_dir = data.path().join(BUNDLED_PIPELINES_DIR);
        std::fs::create_dir_all(&bundled_dir).unwrap();
        std::fs::write(
            bundled_dir.join("deep_research.dot"),
            "digraph deep_research { bundled [prompt=\"BUNDLED\"] }",
        )
        .unwrap();

        let mut discovery = PipelineDiscovery::new(data.path(), working.path());
        discovery.add_bundled_pipelines_dir(data.path());

        let resolved = discovery.resolve("deep_research").await.unwrap();
        assert!(resolved.contains("BUNDLED"));
    }
}
