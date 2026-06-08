//! PATH resolution + shadow detection, product-agnostic.
//!
//! Ported from octos-tui's `doctor.rs` locate/on_path/shadow logic, with the
//! binary name driven by [`ProductSpec`]. Carries the #189 npm fix: when the
//! method is [`InstallMethod::Npm`] and nothing is located, both the on-PATH
//! and shadow checks PASS (the real binary lives under `node_modules/.bin_real`
//! whose dir isn't on PATH and whose basename isn't the product name; the shim
//! IS runnable by name).

use std::path::{Path, PathBuf};

use crate::install_method::InstallMethod;
use crate::report::Check;
use crate::spec::ProductSpec;

const CAT_BINARY: &str = "Binary & version";

/// Product binaries discovered on the host. `$PATH` hits are tracked separately
/// from extra known-install prefixes (cargo bin, brew, …) that may not be on
/// `$PATH`, so "on PATH" reflects bare-name runnability, not mere on-disk
/// presence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocatedBinaries {
    /// Resolved via `$PATH` (runnable by bare name), in PATH precedence order.
    pub on_path: Vec<PathBuf>,
    /// Found only in extra known-install prefixes NOT on `$PATH`.
    pub off_path: Vec<PathBuf>,
}

impl LocatedBinaries {
    /// Every distinct location (PATH hits first, then off-PATH extras).
    pub fn all(&self) -> Vec<PathBuf> {
        let mut v = self.on_path.clone();
        v.extend(self.off_path.iter().cloned());
        v
    }
}

/// Enumerate every product binary on `$PATH` plus known install prefixes,
/// de-duplicated by canonical path, preserving PATH precedence (first wins).
pub fn locate(spec: &ProductSpec) -> LocatedBinaries {
    let exe_name = spec.binary_file_name();
    let mut located = LocatedBinaries::default();
    let mut seen: Vec<PathBuf> = Vec::new();

    let push_if_present = |dir: &Path, dest: &mut Vec<PathBuf>, seen: &mut Vec<PathBuf>| {
        let candidate = dir.join(&exe_name);
        if !candidate.is_file() {
            return;
        }
        let canonical = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        if seen.contains(&canonical) {
            return;
        }
        seen.push(canonical);
        dest.push(candidate);
    };

    // Actual `$PATH` resolutions, in precedence order (first wins).
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            push_if_present(&dir, &mut located.on_path, &mut seen);
        }
    }

    // Extra known-install prefixes that may NOT be on `$PATH`.
    let mut extras: Vec<PathBuf> = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"]
        .iter()
        .map(PathBuf::from)
        .collect();
    if let Some(home) = std::env::var_os("HOME") {
        extras.push(PathBuf::from(&home).join(".cargo").join("bin"));
        extras.push(PathBuf::from(&home).join(".local").join("bin"));
    }
    for dir in extras {
        push_if_present(&dir, &mut located.off_path, &mut seen);
    }

    located
}

/// Whether the product is runnable by bare name (`$PATH`-resolvable).
pub fn on_path_check(
    located: &LocatedBinaries,
    current_exe: Option<&Path>,
    method: &InstallMethod,
    spec: &ProductSpec,
) -> Check {
    let bin = &spec.binary_name;
    let name = format!("{bin} on PATH");
    if let Some(first) = located.on_path.first() {
        return Check::pass(CAT_BINARY, name, "resolvable by name")
            .with_value(first.display().to_string());
    }
    // #189: npm global (esp. Windows) — the launcher shim is on PATH and
    // runnable by name, but `current_exe()` resolves to the real binary deep
    // under `node_modules/.bin_real`, whose dir is NOT on PATH and whose
    // basename isn't the product name — so the PATH scan finds nothing. Don't
    // false-warn, and never suggest adding an internal node_modules dir.
    if matches!(method, InstallMethod::Npm) {
        return Check::pass(CAT_BINARY, name, "runnable by name via the npm global shim")
            .with_value(
                current_exe
                    .map(|e| e.display().to_string())
                    .unwrap_or_default(),
            );
    }
    // Not on $PATH at all. If we know where this exe lives, point at its dir.
    match current_exe.and_then(|e| e.parent()) {
        Some(dir) => Check::warn(
            CAT_BINARY,
            name,
            format!("{bin} isn't on $PATH — you ran it by path"),
            format!("add {} to PATH to run by name", dir.display()),
        )
        .with_value(dir.display().to_string()),
        None => Check::warn(
            CAT_BINARY,
            name,
            format!("{bin} not found on $PATH"),
            "add the install dir to your PATH",
        ),
    }
}

/// Shadowing-install check from the located binaries. >1 total is the
/// shadowing failure mode; npm-with-nothing-located is exactly one healthy
/// install (#189), not a missing one.
pub fn shadow_check(
    located: &LocatedBinaries,
    method: &InstallMethod,
    spec: &ProductSpec,
) -> Check {
    let bin = &spec.binary_name;
    let all = located.all();
    match all.len() {
        0 if matches!(method, InstallMethod::Npm) => Check::pass(
            CAT_BINARY,
            "no shadowing installs",
            "exactly one (npm global)",
        ),
        0 => Check::warn(
            CAT_BINARY,
            "no shadowing installs",
            format!("{bin} not found on $PATH or known install dirs"),
            format!("install {bin} or add its dir to your PATH"),
        ),
        1 => {
            let only = &all[0];
            let where_ = if located.on_path.is_empty() {
                "off PATH"
            } else {
                "on PATH"
            };
            Check::pass(
                CAT_BINARY,
                "no shadowing installs",
                format!("exactly one ({where_})"),
            )
            .with_value(only.display().to_string())
        }
        n => {
            let label = |p: &PathBuf| -> String {
                let tag = if located.on_path.contains(p) {
                    "PATH"
                } else {
                    "known-dir"
                };
                format!("{} [{tag}]", p.display())
            };
            let labelled: Vec<String> = all.iter().map(label).collect();
            Check::warn(
                CAT_BINARY,
                "no shadowing installs",
                format!("{n} {bin} binaries found; first wins: {}", labelled[0]),
                format!("remove the extras: {}", labelled[1..].join(", ")),
            )
            .with_value(labelled.join(" | "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::CheckStatus;

    fn spec() -> ProductSpec {
        ProductSpec::new("octos", "octos", "1.0.0", "octos-org/octos", "octos-bundle")
    }

    #[test]
    fn shadow_check_passes_for_single_and_warns_for_multiple() {
        let one = shadow_check(
            &LocatedBinaries {
                on_path: vec![PathBuf::from("/usr/local/bin/octos")],
                off_path: vec![],
            },
            &InstallMethod::Homebrew,
            &spec(),
        );
        assert_eq!(one.status, CheckStatus::Pass);
        assert!(one.detail.contains("on PATH"));

        let two = shadow_check(
            &LocatedBinaries {
                on_path: vec![PathBuf::from("/opt/homebrew/bin/octos")],
                off_path: vec![PathBuf::from("/home/u/.cargo/bin/octos")],
            },
            &InstallMethod::Homebrew,
            &spec(),
        );
        assert_eq!(two.status, CheckStatus::Warn);
        assert!(two.detail.contains("2 octos binaries"));
        let fix = two.fix.unwrap();
        assert!(fix.contains(".cargo/bin/octos"));
        assert!(fix.contains("[known-dir]") || two.detail.contains("[PATH]"));
    }

    #[test]
    fn shadow_check_warns_when_nothing_found() {
        let none = shadow_check(
            &LocatedBinaries::default(),
            &InstallMethod::Homebrew,
            &spec(),
        );
        assert_eq!(none.status, CheckStatus::Warn);
    }

    #[test]
    fn npm_install_does_not_false_warn_on_path_or_shadow() {
        // #189: npm-global — the locator finds nothing on PATH (the shim is
        // .ps1/.cmd; the real .exe is under node_modules/.bin_real). Both
        // checks must PASS, not warn, and on-PATH must not suggest a fix.
        let located = LocatedBinaries::default();
        let exe = PathBuf::from(
            "C:/Users/u/AppData/Roaming/npm/node_modules/@octos-org/octos/node_modules/.bin_real/octos.exe",
        );
        let on_path = on_path_check(&located, Some(exe.as_path()), &InstallMethod::Npm, &spec());
        assert_eq!(on_path.status, CheckStatus::Pass);
        assert!(
            on_path.fix.is_none(),
            "npm on-PATH check must not suggest a fix"
        );

        let shadow = shadow_check(&located, &InstallMethod::Npm, &spec());
        assert_eq!(shadow.status, CheckStatus::Pass);
        assert!(shadow.detail.contains("npm"));
    }

    #[test]
    fn on_path_check_passes_when_resolvable_by_name() {
        let located = LocatedBinaries {
            on_path: vec![PathBuf::from("/usr/local/bin/octos")],
            off_path: vec![],
        };
        let check = on_path_check(
            &located,
            Some(Path::new("/usr/local/bin/octos")),
            &InstallMethod::Homebrew,
            &spec(),
        );
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn on_path_check_warns_when_ran_by_abs_path_and_dir_not_on_path() {
        let located = LocatedBinaries {
            on_path: vec![],
            off_path: vec![PathBuf::from("/home/u/.cargo/bin/octos")],
        };
        let exe = PathBuf::from("/home/u/.cargo/bin/octos");
        let check = on_path_check(&located, Some(&exe), &InstallMethod::CargoGit, &spec());
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("isn't on $PATH"));
        assert!(check.fix.unwrap().contains("/home/u/.cargo/bin"));
    }
}
