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

/// Bootstrap skill entries into the given directory.
fn bootstrap_entries(skills_dir: &Path, entries: &[(&str, &str, &str, &str)]) -> usize {
    let exe_dir = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        Some(d) => d,
        None => return 0,
    };

    let mut count = 0;

    for &(dir_name, binary_name, skill_md, manifest_json) in entries {
        let skill_dir = skills_dir.join(dir_name);
        let main_path = skill_dir.join("main");

        // Skip if already bootstrapped
        if main_path.exists() {
            continue;
        }

        // Find sibling binary
        let src_binary = exe_dir.join(binary_name);
        if !src_binary.exists() {
            continue;
        }

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

        count += 1;
    }

    count
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

    let src_binary = exe_dir.join(binary_name);
    if !src_binary.exists() {
        return false;
    }

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
}
