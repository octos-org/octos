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
            // Only treat the joined path as a LOCATED pipeline candidate when
            // it is an actual pipeline FILE — a regular file with the `.dot`
            // extension. `candidate.exists()` alone is over-inclusive: for a
            // BARE name like `deep_research`, a coincidental non-`.dot` entry
            // (e.g. a directory `<dir>/deep_research`, or a non-pipeline file)
            // would otherwise be `read_located`'d, fail, and surface as `Read`
            // — which BLOCKS the embedded bundled fallback even though no real
            // `deep_research.dot` candidate exists anywhere (a true miss
            // mis-tagged as `Read`). Narrowing to `is_dot_file` lets such a
            // coincidence fall through to step 3 / `NotFound` so the bundled
            // fallback can fire. A legitimate relative `.dot` path
            // (`subdir/foo.dot`, input already carries `.dot`) is a regular
            // `.dot` file and still resolves here.
            let candidate = dir.join(name_or_path);
            if is_dot_file(&candidate) {
                return read_located(&candidate).await;
            }
            // Try with .dot extension
            let with_ext = dir.join(format!("{name_or_path}.dot"));
            if is_dot_file(&with_ext) {
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

/// Whether `path` is an actual pipeline FILE — a regular file with the `.dot`
/// extension. This is the criterion the step-2 relative-path short-circuit uses
/// to decide whether the joined path is a LOCATED pipeline candidate.
///
/// `is_file()` (which follows symlinks and returns `false` for directories /
/// missing paths) plus the `.dot` extension check together reject the
/// over-inclusive cases — a coincidental directory or a non-`.dot` file sharing
/// a bare pipeline name — so a true miss is not mis-tagged as a `Read` failure.
fn is_dot_file(path: &Path) -> bool {
    path.is_file() && path.extension().is_some_and(|e| e == "dot")
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

    /// Gap 4.1 (codex review) — STEP 2 (the relative-path short-circuit) must
    /// only treat `dir.join(name_or_path)` as a LOCATED pipeline candidate when
    /// it is an actual pipeline FILE (a regular file with the `.dot`
    /// extension). For a BARE name like `deep_research`, a coincidental
    /// non-`.dot` entry — here a DIRECTORY `<search_dir>/deep_research` — must
    /// NOT be treated as the located pipeline.
    ///
    /// RED on ffdfdb98: step 2 did `if candidate.exists() { read_located(..) }`,
    /// so the coincidental directory matched, `read_to_string` on it failed,
    /// and `resolve` returned `Read` — which BLOCKS the embedded bundled
    /// fallback even though there is NO `deep_research.dot` candidate anywhere.
    /// A true miss was mis-tagged as `Read`. GREEN after: step 2 skips the
    /// non-`.dot` entry, resolution falls through to bare-name discovery (step
    /// 3), and ultimately to `NotFound` so the bundled fallback can fire.
    #[tokio::test]
    async fn bare_name_with_coincidental_non_dot_path_is_a_true_miss_not_read() {
        let data = tempfile::tempdir().unwrap();
        let working = tempfile::tempdir().unwrap();

        // A coincidental DIRECTORY named exactly like the bare pipeline name,
        // directly in a search path. NO `deep_research.dot` anywhere.
        let pipelines_dir = data.path().join("pipelines");
        std::fs::create_dir_all(pipelines_dir.join("deep_research")).unwrap();

        let discovery = PipelineDiscovery::new(data.path(), working.path());
        let err = discovery.resolve("deep_research").await.unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<PipelineResolveError>(),
                Some(PipelineResolveError::NotFound { .. })
            ),
            "a bare name whose only coincidental match is a non-`.dot` directory \
             must surface NotFound (so the bundled fallback can fire), NOT Read \
             (which blocks it), got: {err:?}"
        );
    }

    /// Companion to the above: a coincidental non-`.dot` regular FILE (not a
    /// directory) named exactly like the bare pipeline name must also be a true
    /// miss — step 2's candidate criterion is `is_file() && ext == "dot"`, so a
    /// `.dot`-less file is ignored too.
    #[tokio::test]
    async fn bare_name_with_coincidental_non_dot_file_is_a_true_miss_not_read() {
        let data = tempfile::tempdir().unwrap();
        let working = tempfile::tempdir().unwrap();

        // A coincidental plain FILE (no `.dot` extension) named like the bare
        // pipeline name. NO `deep_research.dot` anywhere.
        let pipelines_dir = data.path().join("pipelines");
        std::fs::create_dir_all(&pipelines_dir).unwrap();
        std::fs::write(pipelines_dir.join("deep_research"), "not a pipeline").unwrap();

        let discovery = PipelineDiscovery::new(data.path(), working.path());
        let err = discovery.resolve("deep_research").await.unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<PipelineResolveError>(),
                Some(PipelineResolveError::NotFound { .. })
            ),
            "a bare name whose only coincidental match is a non-`.dot` file must \
             surface NotFound, NOT Read, got: {err:?}"
        );
    }

    /// No-regression guard for step 2: a LEGITIMATE relative path whose input
    /// already carries `.dot` (`subdir/foo.dot`) must STILL resolve via the
    /// step-2 short-circuit. The fix narrows the candidate criterion to a
    /// regular `.dot` file — it must not break the legitimate relative-`.dot`
    /// path that step 2 exists to serve.
    #[tokio::test]
    async fn step2_legitimate_dot_relative_path_still_resolves() {
        let data = tempfile::tempdir().unwrap();
        let working = tempfile::tempdir().unwrap();

        // <data>/pipelines/subdir/foo.dot — reached by joining the relative
        // input `subdir/foo.dot` onto the `<data>/pipelines` search path.
        let subdir = data.path().join("pipelines").join("subdir");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(
            subdir.join("foo.dot"),
            "digraph foo { a [prompt=\"RELATIVE_DOT\"] }",
        )
        .unwrap();

        let discovery = PipelineDiscovery::new(data.path(), working.path());
        let resolved = discovery.resolve("subdir/foo.dot").await.unwrap();
        assert!(
            resolved.contains("RELATIVE_DOT"),
            "a legitimate relative `.dot` path must still resolve via step 2, got: {resolved}"
        );
    }

    /// Close codex's noted step-1/step-2 test gap: an existing `.dot` REGULAR
    /// FILE that fails to read (here made unreadable by removing all
    /// permissions on Unix) must surface `Read` — the read error is propagated,
    /// not masked or mis-tagged as a miss. This proves step 2 still hands real
    /// `.dot` files to `read_located` (so genuine read failures reach the
    /// tool layer's propagate-not-fallback path).
    #[cfg(unix)]
    #[tokio::test]
    async fn step2_existing_dot_file_that_fails_to_read_surfaces_read() {
        use std::os::unix::fs::PermissionsExt;

        let data = tempfile::tempdir().unwrap();
        let working = tempfile::tempdir().unwrap();

        // A real `.dot` file reachable by step 2 via the relative input
        // `subdir/locked.dot`, then made unreadable.
        let subdir = data.path().join("pipelines").join("subdir");
        std::fs::create_dir_all(&subdir).unwrap();
        let dot_path = subdir.join("locked.dot");
        std::fs::write(&dot_path, "digraph locked { a [prompt=\"x\"] }").unwrap();
        std::fs::set_permissions(&dot_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let discovery = PipelineDiscovery::new(data.path(), working.path());
        let result = discovery.resolve("subdir/locked.dot").await;

        // Restore perms so the tempdir can be cleaned up regardless of outcome.
        let _ = std::fs::set_permissions(&dot_path, std::fs::Permissions::from_mode(0o644));

        let err = result.expect_err("an unreadable .dot file located by step 2 must Err");
        assert!(
            matches!(
                err.downcast_ref::<PipelineResolveError>(),
                Some(PipelineResolveError::Read { .. })
            ),
            "a located-but-unreadable `.dot` FILE must surface Read (propagated, \
             not masked), got: {err:?}"
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
