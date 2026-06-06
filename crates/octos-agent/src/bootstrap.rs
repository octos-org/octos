//! Bootstrap bundled app-skill and platform-skill binaries into their directories.
//!
//! At gateway startup, copies sibling binaries (built alongside `octos`) into
//! the appropriate skills directory, plus writes the embedded SKILL.md and manifest.json.
//!
//! ## Layered skill directories
//!
//! ```text
//! ~/.octos/platform-skills/       # Layer 1: platform-wide (asr, etc.)
//! ~/.octos/bundled-app-skills/    # Layer 2: bundled app-skills (news, send-email, etc.)
//! ~/.octos/profiles/{id}/skills/  # Layer 3: per-profile custom installs
//! ```

use std::path::Path;

use crate::bundled_app_skills::{BUNDLED_APP_SKILLS, PLATFORM_SKILLS};
use crate::bundled_pipelines::BUNDLED_PIPELINES;

/// Subdirectory name for bundled app-skills (layer 2).
pub const BUNDLED_APP_SKILLS_DIR: &str = "bundled-app-skills";

/// Subdirectory name for platform skills (layer 1).
pub const PLATFORM_SKILLS_DIR: &str = "platform-skills";

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

/// Bootstrap bundled app-skills into `octos_home/bundled-app-skills/`.
///
/// Returns the number of skills bootstrapped.
pub fn bootstrap_bundled_skills(octos_home: &Path) -> usize {
    let target_dir = octos_home.join(BUNDLED_APP_SKILLS_DIR);
    bootstrap_entries(&target_dir, BUNDLED_APP_SKILLS)
}

/// Bootstrap platform skills into `octos_home/platform-skills/`.
///
/// Returns the number of skills bootstrapped.
pub fn bootstrap_platform_skills(octos_home: &Path) -> usize {
    let target_dir = octos_home.join(PLATFORM_SKILLS_DIR);
    bootstrap_entries(&target_dir, PLATFORM_SKILLS)
}

/// Per-skill marker file recording the sha256 of the source sibling binary
/// that was last copied into `<skill_dir>/main`. Used to detect staleness so
/// an octos UPGRADE refreshes the skill binary instead of leaving it pinned to
/// whatever was copied on first install.
const BUNDLE_SRC_SHA_MARKER: &str = ".bundle-src-sha256";

/// Compute the lowercase-hex sha256 of a file's contents.
///
/// Returns `None` if the file cannot be read, so callers can treat an
/// unreadable source as "skip" (best-effort, matching the surrounding code).
fn sha256_file(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(format!("{:x}", hasher.finalize()))
}

/// Resolve a bundled skill's source binary that sits beside the octos
/// executable. Tries the bare `binary_name` first, then `binary_name.exe` on
/// Windows — release bundles ship `weather.exe`, `news_fetch.exe`, … so a
/// bare-name-only lookup would falsely report every skill missing on Windows
/// (and never bootstrap them). Returns the first existing path, or `None`.
fn resolve_sibling_binary(exe_dir: &Path, binary_name: &str) -> Option<std::path::PathBuf> {
    let bare = exe_dir.join(binary_name);
    if bare.exists() {
        return Some(bare);
    }
    #[cfg(windows)]
    {
        let exe = exe_dir.join(format!("{binary_name}.exe"));
        if exe.exists() {
            return Some(exe);
        }
    }
    None
}

/// Bootstrap skill entries into the given directory.
fn bootstrap_entries(skills_dir: &Path, entries: &[(&str, &str, &str, &str)]) -> usize {
    let exe_dir = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        Some(d) => d,
        None => return 0,
    };
    bootstrap_entries_in(&exe_dir, skills_dir, entries)
}

/// Bootstrap skill entries from sibling binaries in `exe_dir` into `skills_dir`.
///
/// Testable seam for [`bootstrap_entries`] (the public callers resolve
/// `exe_dir` via `current_exe`). Returns the number of skills (re)written.
///
/// **Staleness refresh:** instead of the old "skip if `main` exists" check —
/// which pinned the skill binary to whatever shipped on first install and so
/// went stale across an octos UPGRADE — each skill records the sha256 of its
/// source sibling binary in a `<skill_dir>/.bundle-src-sha256` marker. A skill
/// is left untouched only when `main` exists AND the marker matches the current
/// source hash; otherwise SKILL.md, manifest.json, and `main` are (re)written
/// and the marker refreshed.
///
/// Best-effort: individual filesystem errors `continue` to the next entry,
/// matching the prior behaviour.
fn bootstrap_entries_in(
    exe_dir: &Path,
    skills_dir: &Path,
    entries: &[(&str, &str, &str, &str)],
) -> usize {
    let mut count = 0;

    for &(dir_name, binary_name, skill_md, manifest_json) in entries {
        let skill_dir = skills_dir.join(dir_name);
        let main_path = skill_dir.join("main");
        let marker_path = skill_dir.join(BUNDLE_SRC_SHA_MARKER);

        // Find sibling binary. If it's missing this is silent drift (e.g. a
        // bare-binary deploy without the bundle); the preflight detector
        // (`missing_bundled_skill_binaries`) reports it loudly elsewhere, so
        // here we simply skip — there is nothing to copy.
        let src_binary = match resolve_sibling_binary(exe_dir, binary_name) {
            Some(p) => p,
            None => continue,
        };
        let src_hash = match sha256_file(&src_binary) {
            Some(h) => h,
            None => continue,
        };

        // Up-to-date: `main` already present AND the recorded source hash
        // matches the current sibling binary → leave it untouched, don't count.
        if main_path.exists() {
            if let Ok(recorded) = std::fs::read_to_string(&marker_path) {
                if recorded == src_hash {
                    continue;
                }
            }
        }

        // (Re)write: main missing, or marker missing / mismatched (upgrade).

        // Create skill directory
        if std::fs::create_dir_all(&skill_dir).is_err() {
            continue;
        }

        // Write SKILL.md
        if std::fs::write(skill_dir.join("SKILL.md"), skill_md).is_err() {
            continue;
        }

        // Write manifest.json
        if std::fs::write(skill_dir.join("manifest.json"), manifest_json).is_err() {
            continue;
        }

        // Copy binary as "main"
        if std::fs::copy(&src_binary, &main_path).is_err() {
            continue;
        }

        // chmod 755 on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&main_path, std::fs::Permissions::from_mode(0o755));
        }

        // Record the source hash so a future, unchanged run is a no-op and an
        // upgrade (changed source) re-triggers the copy above.
        let _ = std::fs::write(&marker_path, &src_hash);

        count += 1;
    }

    count
}

/// Bundled app-skills that are *declared* (so they still bootstrap when their
/// binary happens to sit beside octos) but are NOT shipped by the standard
/// release bundle (`scripts/build-local-bundle.sh`, `release.yml`). The
/// bare-binary preflight must not flag these as "missing" — their absence is
/// expected on a normal full-bundle install, so warning about them would make
/// the guard cry wolf on every host. (`skill-evolve` is a key-gated meta-skill
/// kept out of the public bundle; shipping it is a separate product decision.)
const PREFLIGHT_OPTIONAL_SKILLS: &[&str] = &["skill-evolve"];

/// Returns the `binary_name`s of [`BUNDLED_APP_SKILLS`] that the standard bundle
/// is expected to ship but whose sibling binary is absent from `exe_dir` — i.e.
/// the signal of a bare-binary deploy. Skills in [`PREFLIGHT_OPTIONAL_SKILLS`]
/// are excluded so a normal full-bundle install never false-warns.
///
/// Testable seam for [`missing_bundled_skill_binaries`].
fn missing_sibling_skill_binaries_in(exe_dir: &Path) -> Vec<&'static str> {
    BUNDLED_APP_SKILLS
        .iter()
        .filter(|&&(_, binary_name, _, _)| !PREFLIGHT_OPTIONAL_SKILLS.contains(&binary_name))
        .filter(|&&(_, binary_name, _, _)| resolve_sibling_binary(exe_dir, binary_name).is_none())
        .map(|&(_, binary_name, _, _)| binary_name)
        .collect()
}

/// Preflight detector: which bundled app-skill binaries are missing from beside
/// the running `octos` executable.
///
/// A non-empty result almost always means a bare-binary install (someone copied
/// just `octos` without the rest of the bundle), in which case the affected
/// app-skill tools (`get_weather`, etc.) will silently fail to register. The
/// caller is expected to surface this as an actionable warning.
///
/// Returns an empty vec if `current_exe` cannot be resolved.
pub fn missing_bundled_skill_binaries() -> Vec<&'static str> {
    let exe_dir = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        Some(d) => d,
        None => return Vec::new(),
    };
    missing_sibling_skill_binaries_in(&exe_dir)
}

/// Bootstrap a single named skill into the appropriate directory under `octos_home`.
///
/// Unlike `bootstrap_bundled_skills`/`bootstrap_platform_skills`, this always
/// overwrites existing files (used for conditional skills that may need
/// re-bootstrap after updates).
///
/// Returns `true` if the skill was successfully bootstrapped.
pub fn bootstrap_single_skill(octos_home: &Path, name: &str) -> bool {
    let exe_dir = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        Some(d) => d,
        None => return false,
    };

    // Determine which list this skill belongs to and its target directory
    let (entry, subdir) =
        if let Some(e) = BUNDLED_APP_SKILLS.iter().find(|&&(d, _, _, _)| d == name) {
            (e, BUNDLED_APP_SKILLS_DIR)
        } else if let Some(e) = PLATFORM_SKILLS.iter().find(|&&(d, _, _, _)| d == name) {
            (e, PLATFORM_SKILLS_DIR)
        } else {
            return false;
        };

    let &(dir_name, binary_name, skill_md, manifest_json) = entry;

    let skill_dir = octos_home.join(subdir).join(dir_name);
    let main_path = skill_dir.join("main");

    let src_binary = match resolve_sibling_binary(&exe_dir, binary_name) {
        Some(p) => p,
        None => return false,
    };

    if std::fs::create_dir_all(&skill_dir).is_err() {
        return false;
    }

    if std::fs::write(skill_dir.join("SKILL.md"), skill_md).is_err() {
        return false;
    }
    if std::fs::write(skill_dir.join("manifest.json"), manifest_json).is_err() {
        return false;
    }
    if std::fs::copy(&src_binary, &main_path).is_err() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&main_path, std::fs::Permissions::from_mode(0o755));
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compute the lowercase hex sha256 of the given bytes (test helper that
    /// mirrors what `sha256_file` produces on disk).
    fn sha256_bytes(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    #[test]
    fn bootstrap_entries_in_refreshes_stale_main() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("exe");
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&exe_dir).unwrap();

        // Fake sibling binary "wx" with bytes "v1".
        let src = exe_dir.join("wx");
        std::fs::write(&src, b"v1").unwrap();

        let entries: &[(&str, &str, &str, &str)] = &[("wx", "wx", "SKILL", "MANIFEST")];

        // First run: main written, returns 1.
        let n1 = bootstrap_entries_in(&exe_dir, &skills_dir, entries);
        assert_eq!(n1, 1, "first bootstrap writes the skill");
        let main_path = skills_dir.join("wx").join("main");
        assert_eq!(std::fs::read(&main_path).unwrap(), b"v1");

        // Re-run unchanged: up-to-date skip, returns 0, main unchanged.
        let n2 = bootstrap_entries_in(&exe_dir, &skills_dir, entries);
        assert_eq!(n2, 0, "unchanged source is an up-to-date skip");
        assert_eq!(std::fs::read(&main_path).unwrap(), b"v1");

        // Upgrade: overwrite the sibling binary with new, bigger bytes.
        std::fs::write(&src, b"v2-bigger").unwrap();
        let n3 = bootstrap_entries_in(&exe_dir, &skills_dir, entries);
        assert_eq!(n3, 1, "a changed source binary triggers a refresh");
        assert_eq!(
            std::fs::read(&main_path).unwrap(),
            b"v2-bigger",
            "stale main must be refreshed with the new source binary bytes"
        );
    }

    #[test]
    fn bootstrap_entries_in_writes_hash_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("exe");
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&exe_dir).unwrap();

        std::fs::write(exe_dir.join("wx"), b"v1").unwrap();
        let entries: &[(&str, &str, &str, &str)] = &[("wx", "wx", "SKILL", "MANIFEST")];

        let n = bootstrap_entries_in(&exe_dir, &skills_dir, entries);
        assert_eq!(n, 1);

        let marker = skills_dir.join("wx").join(".bundle-src-sha256");
        assert!(marker.exists(), "marker file must be written after a copy");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            sha256_bytes(b"v1"),
            "marker must contain the sha256 of the source sibling binary"
        );
    }

    #[test]
    fn missing_sibling_skill_binaries_in_reports_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("exe");
        std::fs::create_dir_all(&exe_dir).unwrap();

        // None of the bundled binaries exist beside this fake exe dir.
        let missing = missing_sibling_skill_binaries_in(&exe_dir);
        assert!(
            !missing.is_empty(),
            "an empty exe dir must report missing bundled binaries"
        );
        assert!(
            missing.contains(&"weather"),
            "the weather sibling binary should be reported missing, got: {missing:?}"
        );
        // Preflight-optional skills (not shipped by the standard bundle) must
        // never be reported, even when absent, or the guard cries wolf.
        assert!(
            !missing.contains(&"skill-evolve"),
            "preflight-optional skill-evolve must not be reported missing, got: {missing:?}"
        );

        // Touch every bundled binary beside the exe dir.
        for &(_, binary_name, _, _) in BUNDLED_APP_SKILLS {
            std::fs::write(exe_dir.join(binary_name), b"x").unwrap();
        }
        let after = missing_sibling_skill_binaries_in(&exe_dir);
        assert!(
            !after.contains(&"weather"),
            "weather must no longer be reported missing once its sibling exists"
        );
        assert!(
            after.is_empty(),
            "no bundled binaries should be missing once all are present, got: {after:?}"
        );
    }

    #[test]
    fn bootstrap_bundled_skills_with_empty_dir_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        // No sibling binaries exist next to the test runner, so nothing gets bootstrapped.
        let count = bootstrap_bundled_skills(&skills_dir);
        assert_eq!(count, 0);
    }

    #[test]
    fn bootstrap_single_skill_nonexistent_name_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        assert!(!bootstrap_single_skill(&skills_dir, "no-such-skill-xyz"));
    }

    #[test]
    fn bootstrap_single_skill_valid_name_no_binary_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        // "news" is a real bundled skill name, but the binary won't exist next to the test runner.
        assert!(!bootstrap_single_skill(&skills_dir, "news"));
    }

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
