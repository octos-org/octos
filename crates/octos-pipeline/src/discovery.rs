//! Pipeline file discovery — finds .dot pipeline files from standard locations.

use std::path::{Path, PathBuf};

use eyre::Result;

/// Information about a discoverable pipeline.
pub struct PipelineInfo {
    pub name: String,
    pub path: PathBuf,
}

/// Typed failure kinds for [`PipelineDiscovery::resolve`], so callers can tell
/// a TRUE miss apart from a located-but-unreadable candidate.
///
/// Gap 4.1 (codex review): the embedded bundled fallback in `RunPipelineTool`
/// must fire ONLY on a true miss ([`PipelineResolveError::NotFound`]). When
/// discovery LOCATED an installed `.dot` but failed to read/parse it
/// ([`PipelineResolveError::Read`]), falling back would MASK the broken install
/// and let the bundled copy out-rank a present installed pipeline. The error is
/// carried inside the `eyre::Report` so the existing `Result<String>` signature
/// (and all `.await?` consumers) stay unchanged — the tool layer distinguishes
/// the two via `downcast_ref::<PipelineResolveError>()`.
#[derive(Debug)]
pub enum PipelineResolveError {
    /// No candidate file was located in any search path — a TRUE miss. The
    /// embedded bundled fallback may correctly fire for this case.
    NotFound {
        /// The name/path the caller asked for.
        requested: String,
        /// The discoverable pipeline names, for a helpful error message.
        available: Vec<String>,
    },
    /// A candidate file WAS located but could not be read/parsed (I/O,
    /// permission, or UTF-8 error). The fallback must NOT mask this — it would
    /// out-rank a present installed pipeline. Propagate it instead.
    Read {
        /// The located candidate that failed to load.
        path: PathBuf,
        /// The underlying read error, rendered.
        source: String,
    },
}

impl std::fmt::Display for PipelineResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipelineResolveError::NotFound {
                requested,
                available,
            } => {
                write!(
                    f,
                    "pipeline '{requested}' not found. Available: {}",
                    if available.is_empty() {
                        "(none)".to_string()
                    } else {
                        available.join(", ")
                    }
                )
            }
            PipelineResolveError::Read { path, source } => {
                write!(
                    f,
                    "failed to read pipeline file '{}': {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for PipelineResolveError {}

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
            return read_located(&as_path).await;
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
                return read_located(&candidate).await;
            }
            // Try with .dot extension
            let with_ext = dir.join(format!("{name_or_path}.dot"));
            if with_ext.exists() {
                return read_located(&with_ext).await;
            }
        }

        // 3. Search by bare name across all paths (including nested skill dirs).
        //    Gap 4.1 BLOCKER 2: discovery stores names as file STEMS
        //    (`deep_research`), so canonicalize the input to the same stem
        //    form (strip any directory component AND a trailing `.dot`) before
        //    comparing. This makes `deep_research` and `deep_research.dot`
        //    resolve identically here — both hit the INSTALLED copy — so the
        //    embedded-bytes fallback (in the tool layer) can never out-rank an
        //    installed pipeline for the `.dot` input form. Direct file paths
        //    were already handled at higher precedence by steps 1-2.
        let want_stem = pipeline_name_stem(name_or_path);
        let all = self.list_available();
        for info in &all {
            if info.name == want_stem {
                return read_located(&info.path).await;
            }
        }

        // TRUE MISS: no candidate located in any search path. This is the ONLY
        // error kind for which the tool-layer embedded bundled fallback may
        // correctly fire (see `PipelineResolveError`).
        Err(eyre::Report::new(PipelineResolveError::NotFound {
            requested: name_or_path.to_string(),
            available: all.into_iter().map(|p| p.name).collect(),
        }))
    }
}

/// Read a LOCATED candidate file to its DOT string. A failure here is a
/// found-but-unreadable case ([`PipelineResolveError::Read`]) — never a miss —
/// so the tool layer propagates it instead of masking it with the bundled copy.
async fn read_located(path: &Path) -> Result<String> {
    tokio::fs::read_to_string(path).await.map_err(|e| {
        eyre::Report::new(PipelineResolveError::Read {
            path: path.to_path_buf(),
            source: e.to_string(),
        })
    })
}

/// Canonicalize a pipeline name-or-path input to the bare file STEM that
/// [`PipelineDiscovery`] stores in [`PipelineInfo::name`] (see
/// [`scan_dot_files`], which uses `Path::file_stem`).
///
/// Strips any directory component AND a trailing `.dot` extension, so
/// `deep_research`, `deep_research.dot`, and `mofa-research/deep_research.dot`
/// all canonicalize to `deep_research`. Used for the bare-name discovery
/// comparison (BLOCKER 2 installed-wins) and mirrored by the embedded-bundled
/// fallback in `RunPipelineTool`, so both input forms resolve identically:
/// discovery (installed) first, embedded bytes only on a true miss.
pub fn pipeline_name_stem(name_or_path: &str) -> String {
    // Drop any directory component first (`mofa-research/deep_research.dot`
    // -> `deep_research.dot`).
    let file = Path::new(name_or_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name_or_path.to_string());
    // Strip ONLY a trailing `.dot` — never a different extension. A bare name
    // like `my.pipeline` (no `.dot`) must stay intact so we don't accidentally
    // canonicalize away a legitimate stem the way `Path::file_stem` would.
    file.strip_suffix(".dot").unwrap_or(&file).to_string()
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

    /// Gap 4.1 BLOCKER 2 (`.dot`-suffixed input bypasses installed-wins) —
    /// `resolve("deep_research.dot")` MUST resolve to the INSTALLED
    /// `skills/mofa-research/deep_research.dot` (stem `deep_research`), the
    /// same as the bare-name form. Discovery stores names as file stems, so
    /// before the fix the `.dot` form missed the bare-name comparison (step 3
    /// compared `info.name == "deep_research.dot"` against stem
    /// `deep_research`) and discovery returned Err — which (in the tool layer)
    /// let the embedded bundled bytes win over the installed copy. After the
    /// fix the input is canonicalized to the bare stem before the bare-name
    /// comparison, so both forms resolve identically to the installed copy.
    #[tokio::test]
    async fn dot_suffixed_input_resolves_installed_same_as_bare_name() {
        let data = tempfile::tempdir().unwrap();
        let working = tempfile::tempdir().unwrap();

        // Installed skill copy (nested — NOT a top-level direct path), stored
        // by discovery under the bare stem `deep_research`.
        let skill_dir = data.path().join("skills").join("mofa-research");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("deep_research.dot"),
            "digraph deep_research { installed [prompt=\"INSTALLED\"] }",
        )
        .unwrap();

        let discovery = PipelineDiscovery::new(data.path(), working.path());

        // Bare name resolves to the installed copy.
        let bare = discovery.resolve("deep_research").await.unwrap();
        assert!(
            bare.contains("INSTALLED"),
            "bare name must resolve installed copy, got: {bare}"
        );

        // `.dot`-suffixed form MUST resolve identically (RED before the fix:
        // step-3 stem comparison missed `deep_research.dot`, so this errored).
        let dotted = discovery.resolve("deep_research.dot").await.unwrap();
        assert!(
            dotted.contains("INSTALLED"),
            "`.dot`-suffixed input must resolve the SAME installed copy as the bare name, got: {dotted}"
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

    /// A TRUE miss (no candidate anywhere) returns `PipelineResolveError::NotFound`
    /// — the ONLY error kind the tool-layer bundled fallback may fire for.
    #[tokio::test]
    async fn resolve_returns_not_found_on_true_miss() {
        let data = tempfile::tempdir().unwrap();
        let working = tempfile::tempdir().unwrap();
        let discovery = PipelineDiscovery::new(data.path(), working.path());

        let err = discovery.resolve("nope_missing").await.unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<PipelineResolveError>(),
                Some(PipelineResolveError::NotFound { .. })
            ),
            "true miss must surface NotFound, got: {err:?}"
        );
    }

    /// A LOCATED-but-unreadable candidate returns `PipelineResolveError::Read`
    /// (NOT NotFound), so the tool layer propagates it instead of masking it
    /// with the bundled copy. Here the installed `deep_research.dot` is a
    /// directory: discovery's `.dot` extension scan locates it, but
    /// `read_to_string` on a directory fails.
    #[tokio::test]
    async fn resolve_returns_read_error_when_located_candidate_unreadable() {
        let data = tempfile::tempdir().unwrap();
        let working = tempfile::tempdir().unwrap();

        let skill_dir = data.path().join("skills").join("mofa-research");
        std::fs::create_dir_all(skill_dir.join("deep_research.dot")).unwrap();

        let discovery = PipelineDiscovery::new(data.path(), working.path());
        let err = discovery.resolve("deep_research").await.unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<PipelineResolveError>(),
                Some(PipelineResolveError::Read { .. })
            ),
            "located-but-unreadable candidate must surface Read, NOT NotFound (which \
             would let the bundled fallback mask the broken install), got: {err:?}"
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
