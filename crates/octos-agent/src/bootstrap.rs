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
