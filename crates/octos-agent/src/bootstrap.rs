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

/// Subdirectory name for bundled app-skills (layer 2).
pub const BUNDLED_APP_SKILLS_DIR: &str = "bundled-app-skills";

/// Subdirectory name for platform skills (layer 1).
pub const PLATFORM_SKILLS_DIR: &str = "platform-skills";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapFailure {
    pub skill: String,
    pub path: std::path::PathBuf,
    pub operation: &'static str,
    pub error: String,
}

impl std::fmt::Display for BootstrapFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "skill={} operation={} path={}: {}",
            self.skill,
            self.operation,
            self.path.display(),
            self.error
        )
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BootstrapReport {
    pub ready: usize,
    pub failed: Vec<BootstrapFailure>,
}

impl BootstrapReport {
    pub fn into_required(self, bundle: &str) -> eyre::Result<Self> {
        if self.failed.is_empty() {
            return Ok(self);
        }
        let details = self
            .failed
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        Err(eyre::eyre!("required {bundle} bootstrap failed: {details}"))
    }
}

fn record_bootstrap_failure(
    report: &mut BootstrapReport,
    skill: &str,
    path: &Path,
    operation: &'static str,
    error: impl std::fmt::Display,
) {
    report.failed.push(BootstrapFailure {
        skill: skill.to_owned(),
        path: path.to_path_buf(),
        operation,
        error: error.to_string(),
    });
}

/// Bootstrap required bundled app-skills into `octos_home/bundled-app-skills/`.
pub fn bootstrap_bundled_skills(octos_home: &Path) -> eyre::Result<BootstrapReport> {
    let target_dir = octos_home.join(BUNDLED_APP_SKILLS_DIR);
    bootstrap_entries(&target_dir, BUNDLED_APP_SKILLS).into_required("bundled app-skill")
}

/// Bootstrap optional platform skills into `octos_home/platform-skills/`.
pub fn bootstrap_platform_skills(octos_home: &Path) -> BootstrapReport {
    let target_dir = octos_home.join(PLATFORM_SKILLS_DIR);
    bootstrap_entries(&target_dir, PLATFORM_SKILLS)
}

/// Per-skill marker file recording the sha256 of the source sibling binary
/// that was last copied into `<skill_dir>/main`. Used to detect staleness so
/// an octos UPGRADE refreshes the skill binary instead of leaving it pinned to
/// whatever was copied on first install.
const BUNDLE_SRC_SHA_MARKER: &str = ".bundle-src-sha256";

/// Compute the lowercase-hex sha256 of a file's contents without hiding read
/// failures: a present-but-unreadable source is a bootstrap error, not absence.
fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// A matching marker alone is insufficient after a partial copy or failed
/// chmod: revalidate the installed executable before accepting it as ready.
fn installed_main_is_current(path: &Path, src_hash: &str) -> bool {
    if !sha256_file(path).is_ok_and(|hash| hash == src_hash) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
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
fn bootstrap_entries(skills_dir: &Path, entries: &[(&str, &str, &str, &str)]) -> BootstrapReport {
    let exe_dir = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        Some(d) => d,
        None => return BootstrapReport::default(),
    };
    bootstrap_entries_in(&exe_dir, skills_dir, entries)
}

/// Bootstrap skill entries from sibling binaries in `exe_dir` into `skills_dir`.
///
/// Testable seam for [`bootstrap_entries`] (the public callers resolve
/// `exe_dir` via `current_exe`). Returns ready and failed bundle outcomes.
///
/// **Staleness refresh:** instead of the old "skip if `main` exists" check —
/// which pinned the skill binary to whatever shipped on first install and so
/// went stale across an octos UPGRADE — each skill records the sha256 of its
/// source sibling binary in a `<skill_dir>/.bundle-src-sha256` marker. A skill
/// is left untouched only when `main` matches the source and is executable,
/// the marker matches, and its bundled instructions and manifest are current.
/// Metadata-only upgrades must refresh tool routing even when the sibling
/// binary is unchanged.
///
/// Individual filesystem errors stop that bundle, are recorded in the report,
/// and do not prevent independent entries from being attempted.
fn bootstrap_entries_in(
    exe_dir: &Path,
    skills_dir: &Path,
    entries: &[(&str, &str, &str, &str)],
) -> BootstrapReport {
    bootstrap_entries_in_with_permissions(exe_dir, skills_dir, entries, &set_executable_permissions)
}

fn bootstrap_entries_in_with_permissions(
    exe_dir: &Path,
    skills_dir: &Path,
    entries: &[(&str, &str, &str, &str)],
    set_permissions: &dyn Fn(&Path) -> std::io::Result<()>,
) -> BootstrapReport {
    let mut report = BootstrapReport::default();

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
            Ok(hash) => hash,
            Err(error) => {
                record_bootstrap_failure(&mut report, dir_name, &src_binary, "read_source", error);
                continue;
            }
        };

        // Up-to-date: the complete installed main and metadata are current,
        // so leave them untouched and count the bundle as ready.
        if main_path.exists() {
            if let Ok(recorded) = std::fs::read_to_string(&marker_path) {
                if recorded == src_hash
                    && installed_main_is_current(&main_path, &src_hash)
                    && std::fs::read_to_string(skill_dir.join("SKILL.md"))
                        .is_ok_and(|text| text == *skill_md)
                    && std::fs::read_to_string(skill_dir.join("manifest.json"))
                        .is_ok_and(|text| text == *manifest_json)
                {
                    report.ready += 1;
                    continue;
                }
            }
        }

        // (Re)write: main missing, or marker missing / mismatched (upgrade).

        // Create skill directory
        if let Err(error) = std::fs::create_dir_all(&skill_dir) {
            record_bootstrap_failure(&mut report, dir_name, &skill_dir, "create_dir_all", error);
            continue;
        }

        // Write SKILL.md
        let skill_path = skill_dir.join("SKILL.md");
        if let Err(error) = std::fs::write(&skill_path, skill_md) {
            record_bootstrap_failure(&mut report, dir_name, &skill_path, "write_skill", error);
            continue;
        }

        // Write manifest.json
        let manifest_path = skill_dir.join("manifest.json");
        if let Err(error) = std::fs::write(&manifest_path, manifest_json) {
            record_bootstrap_failure(
                &mut report,
                dir_name,
                &manifest_path,
                "write_manifest",
                error,
            );
            continue;
        }

        // Copy binary as "main"
        if let Err(error) = std::fs::copy(&src_binary, &main_path) {
            record_bootstrap_failure(&mut report, dir_name, &main_path, "copy_binary", error);
            continue;
        }

        // chmod 755 on Unix
        if let Err(error) = set_permissions(&main_path) {
            record_bootstrap_failure(&mut report, dir_name, &main_path, "set_permissions", error);
            continue;
        }

        // Record the source hash so a future, unchanged run is a no-op and an
        // upgrade (changed source) re-triggers the copy above.
        if let Err(error) = std::fs::write(&marker_path, &src_hash) {
            record_bootstrap_failure(&mut report, dir_name, &marker_path, "write_marker", error);
            continue;
        }

        report.ready += 1;
    }

    report
}

fn set_executable_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Returns the `binary_name`s of [`BUNDLED_APP_SKILLS`] that the standard bundle
/// is expected to ship but whose sibling binary is absent from `exe_dir` — i.e.
/// the signal of an incomplete deploy.
///
/// Testable seam for [`missing_bundled_skill_binaries`].
fn missing_sibling_skill_binaries_in(exe_dir: &Path) -> Vec<&'static str> {
    BUNDLED_APP_SKILLS
        .iter()
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
    bootstrap_single_skill_in(&exe_dir, octos_home, name)
}

/// Testable seam for [`bootstrap_single_skill`] (the public caller resolves
/// `exe_dir` via `current_exe`). Bootstraps the single named skill from a
/// sibling binary found in `exe_dir`; returns `true` on success. Mirrors the
/// [`bootstrap_entries`] / [`bootstrap_entries_in`] split so tests can pass a
/// controlled `exe_dir` rather than depending on whatever sits beside the test
/// runner (on Windows cargo leaves bare-named skill `.exe`s in `deps/`).
fn bootstrap_single_skill_in(exe_dir: &Path, octos_home: &Path, name: &str) -> bool {
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

    let src_binary = match resolve_sibling_binary(exe_dir, binary_name) {
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
        assert_eq!(n1.ready, 1, "first bootstrap writes the skill");
        let main_path = skills_dir.join("wx").join("main");
        assert_eq!(std::fs::read(&main_path).unwrap(), b"v1");

        // Re-run unchanged: still ready, with no rewrite and main unchanged.
        let n2 = bootstrap_entries_in(&exe_dir, &skills_dir, entries);
        assert_eq!(n2.ready, 1, "unchanged source remains ready");
        assert_eq!(std::fs::read(&main_path).unwrap(), b"v1");

        // Upgrade: overwrite the sibling binary with new, bigger bytes.
        std::fs::write(&src, b"v2-bigger").unwrap();
        let n3 = bootstrap_entries_in(&exe_dir, &skills_dir, entries);
        assert_eq!(n3.ready, 1, "a changed source binary triggers a refresh");
        assert_eq!(
            std::fs::read(&main_path).unwrap(),
            b"v2-bigger",
            "stale main must be refreshed with the new source binary bytes"
        );
    }

    #[test]
    fn bootstrap_entries_in_refreshes_metadata_with_unchanged_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("bin");
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::write(exe_dir.join("research"), b"same-binary").unwrap();
        let old = &[("research", "research", "use pipeline", r#"{"tools":[]}"#)];
        let current = &[(
            "research",
            "research",
            "use native search",
            r#"{"tools":[{"name":"search"}]}"#,
        )];
        assert_eq!(bootstrap_entries_in(&exe_dir, &skills_dir, old).ready, 1);
        assert_eq!(
            bootstrap_entries_in(&exe_dir, &skills_dir, current).ready,
            1
        );
        assert_eq!(
            std::fs::read_to_string(skills_dir.join("research/SKILL.md")).unwrap(),
            "use native search"
        );
        assert_eq!(
            std::fs::read_to_string(skills_dir.join("research/manifest.json")).unwrap(),
            current[0].3
        );
        assert_eq!(
            bootstrap_entries_in(&exe_dir, &skills_dir, current).ready,
            1
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
        assert_eq!(n.ready, 1);

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
        assert!(
            missing.contains(&"skill-evolve"),
            "standard skill-evolve must be reported missing, got: {missing:?}"
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
    fn should_report_unreadable_sibling_binary_instead_of_skipping() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("exe");
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let source = exe_dir.join("wx");
        std::fs::create_dir(&source).unwrap();
        let entries: &[(&str, &str, &str, &str)] = &[("wx", "wx", "SKILL", "MANIFEST")];

        let report = bootstrap_entries_in(&exe_dir, &skills_dir, entries);
        assert_eq!(report.ready, 0);
        assert_eq!(report.failed.len(), 1);
        let failure = &report.failed[0];
        assert_eq!(failure.skill, "wx");
        assert_eq!(failure.path, source);
        assert_eq!(failure.operation, "read_source");
        let error = report.into_required("test bundle").unwrap_err();
        assert!(format!("{error:#}").contains("read_source"));
    }

    #[test]
    fn bootstrap_bundled_skills_with_empty_dir_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        // Point the bootstrap seam at a controlled, EMPTY exe_dir. Unit tests
        // run from `target/debug/deps/`, and on Windows cargo drops bare-named
        // skill binaries (`weather.exe`, `news_fetch.exe`, …) right there next
        // to the test runner — so probing beside the real runner finds sibling
        // binaries and bootstraps them (this test used to see 7 on Windows).
        // An empty exe_dir exercises the "no sibling binaries → nothing
        // bootstrapped" invariant deterministically on every platform.
        let exe_dir = tmp.path().join("exe");
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&skills_dir).unwrap();
        let count = bootstrap_entries_in(&exe_dir, &skills_dir, BUNDLED_APP_SKILLS);
        assert_eq!(count.ready, 0);
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
        // Controlled empty exe_dir (see the empty-dir test above): on Windows
        // the real test runner has sibling skill `.exe`s in `deps/`, so probe
        // an empty dir to exercise the "known name, missing binary" path.
        let exe_dir = tmp.path().join("exe");
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&skills_dir).unwrap();
        // "news" is a real bundled skill name, but no sibling binary exists in
        // the empty exe_dir, so bootstrap must fail.
        assert!(!bootstrap_single_skill_in(&exe_dir, &skills_dir, "news"));
    }

    const D14_ENTRIES: &[(&str, &str, &str, &str)] = &[("wx", "wx", "SKILL", "MANIFEST")];

    fn d14_dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("exe");
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(exe_dir.join("wx"), b"d14-binary").unwrap();
        (tmp, exe_dir, skills_dir)
    }

    fn d14_assert_failure(report: &BootstrapReport, operation: &str, path: &std::path::Path) {
        assert_eq!(report.ready, 0, "failed bundle must not count as ready");
        assert_eq!(
            report.failed.len(),
            1,
            "failure must be structured: {report:?}"
        );
        let failure = &report.failed[0];
        assert_eq!(failure.skill, "wx");
        assert_eq!(failure.operation, operation);
        assert_eq!(failure.path, path);
        let detail = failure.to_string();
        assert!(detail.contains("skill=wx"), "{detail}");
        assert!(detail.contains(operation), "{detail}");
        assert!(detail.contains(&path.display().to_string()), "{detail}");
    }

    #[test]
    fn should_report_mkdir_failure_instead_of_skipping() {
        let (_tmp, exe_dir, skills_dir) = d14_dirs();
        let skill_dir = skills_dir.join("wx");
        std::fs::write(&skill_dir, b"not-a-directory").unwrap();
        let report = bootstrap_entries_in(&exe_dir, &skills_dir, D14_ENTRIES);
        d14_assert_failure(&report, "create_dir_all", &skill_dir);
    }

    #[test]
    fn should_report_skill_write_failure_instead_of_skipping() {
        let (_tmp, exe_dir, skills_dir) = d14_dirs();
        let path = skills_dir.join("wx").join("SKILL.md");
        std::fs::create_dir_all(&path).unwrap();
        let report = bootstrap_entries_in(&exe_dir, &skills_dir, D14_ENTRIES);
        d14_assert_failure(&report, "write_skill", &path);
    }

    #[test]
    fn should_report_manifest_write_failure_instead_of_skipping() {
        let (_tmp, exe_dir, skills_dir) = d14_dirs();
        let path = skills_dir.join("wx").join("manifest.json");
        std::fs::create_dir_all(&path).unwrap();
        let report = bootstrap_entries_in(&exe_dir, &skills_dir, D14_ENTRIES);
        d14_assert_failure(&report, "write_manifest", &path);
    }

    #[test]
    fn should_report_binary_copy_failure_instead_of_skipping() {
        let (_tmp, exe_dir, skills_dir) = d14_dirs();
        let path = skills_dir.join("wx").join("main");
        std::fs::create_dir_all(&path).unwrap();
        let report = bootstrap_entries_in(&exe_dir, &skills_dir, D14_ENTRIES);
        d14_assert_failure(&report, "copy_binary", &path);
    }

    #[test]
    fn should_report_chmod_failure_instead_of_counting_ready() {
        let (_tmp, exe_dir, skills_dir) = d14_dirs();
        let path = skills_dir.join("wx").join("main");
        let report =
            bootstrap_entries_in_with_permissions(&exe_dir, &skills_dir, D14_ENTRIES, &|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "D14_CHMOD_FAILED",
                ))
            });
        d14_assert_failure(&report, "set_permissions", &path);
        assert!(
            path.exists(),
            "injected failure happens after the real copy"
        );
        assert!(!skills_dir.join("wx").join(".bundle-src-sha256").exists());
        let retry = bootstrap_entries_in(&exe_dir, &skills_dir, D14_ENTRIES);
        assert_eq!(retry.ready, 1);
        assert!(retry.failed.is_empty(), "successful retry becomes ready");
    }

    #[test]
    fn should_report_marker_failure_instead_of_counting_ready() {
        let (_tmp, exe_dir, skills_dir) = d14_dirs();
        let path = skills_dir.join("wx").join(".bundle-src-sha256");
        std::fs::create_dir_all(&path).unwrap();
        let report = bootstrap_entries_in(&exe_dir, &skills_dir, D14_ENTRIES);
        d14_assert_failure(&report, "write_marker", &path);
        assert!(skills_dir.join("wx").join("main").exists());
        let retry = bootstrap_entries_in(&exe_dir, &skills_dir, D14_ENTRIES);
        d14_assert_failure(&retry, "write_marker", &path);
    }

    #[test]
    fn should_not_accept_partial_main_with_current_marker_and_metadata() {
        let (_tmp, exe_dir, skills_dir) = d14_dirs();
        let old = &[("wx", "wx", "old-skill", "old-manifest")];
        let current = &[("wx", "wx", "current-skill", "current-manifest")];
        assert_eq!(bootstrap_entries_in(&exe_dir, &skills_dir, old).ready, 1);
        std::fs::write(skills_dir.join("wx/SKILL.md"), "current-skill").unwrap();
        std::fs::write(skills_dir.join("wx/manifest.json"), "current-manifest").unwrap();
        std::fs::write(skills_dir.join("wx/main"), b"partial").unwrap();
        let retry = bootstrap_entries_in(&exe_dir, &skills_dir, current);
        assert_eq!(retry.ready, 1);
        assert!(retry.failed.is_empty());
        assert_eq!(
            std::fs::read(skills_dir.join("wx/main")).unwrap(),
            b"d14-binary",
            "matching marker/metadata must not hide a partially copied main"
        );
    }

    #[cfg(unix)]
    #[test]
    fn should_retry_metadata_only_chmod_failure_before_ready() {
        use std::os::unix::fs::PermissionsExt;

        let (_tmp, exe_dir, skills_dir) = d14_dirs();
        let old = &[("wx", "wx", "old-skill", "old-manifest")];
        let current = &[("wx", "wx", "current-skill", "current-manifest")];
        assert_eq!(bootstrap_entries_in(&exe_dir, &skills_dir, old).ready, 1);
        let main = skills_dir.join("wx").join("main");
        let report = bootstrap_entries_in_with_permissions(&exe_dir, &skills_dir, current, &|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "D14_CHMOD_FAILED",
            ))
        });
        d14_assert_failure(&report, "set_permissions", &main);
        std::fs::set_permissions(&main, std::fs::Permissions::from_mode(0o644)).unwrap();
        let retry = bootstrap_entries_in(&exe_dir, &skills_dir, current);
        assert_eq!(retry.ready, 1);
        assert!(retry.failed.is_empty());
        assert_ne!(
            std::fs::metadata(&main).unwrap().permissions().mode() & 0o111,
            0,
            "a matching old marker must not hide non-executable main"
        );
    }

    #[test]
    fn should_fail_required_bundle_with_skill_path_and_operation() {
        let (_tmp, exe_dir, skills_dir) = d14_dirs();
        let skill_dir = skills_dir.join("wx");
        std::fs::write(&skill_dir, b"not-a-directory").unwrap();
        let report = bootstrap_entries_in(&exe_dir, &skills_dir, D14_ENTRIES);
        let error = report.into_required("test-app").unwrap_err();
        let detail = format!("{error:#}");
        assert!(
            detail.contains("required test-app bootstrap failed"),
            "{detail}"
        );
        assert!(detail.contains("skill=wx"), "{detail}");
        assert!(detail.contains("operation=create_dir_all"), "{detail}");
        assert!(
            detail.contains(&skill_dir.display().to_string()),
            "{detail}"
        );
    }
}
