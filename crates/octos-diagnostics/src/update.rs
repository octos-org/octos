//! Semver parse/compare helpers + the pure update *planner*.
//!
//! Stage 1 ships **types + a pure decision function only** — NO network, NO
//! mutation. Given the current version, a (separately-fetched, Stage 2) latest
//! version, and the [`InstallMethod`], [`plan`] returns an [`UpdatePlan`]
//! describing what *should* happen. Each binary's own driver (Stage 3) executes
//! it.

use crate::install_method::InstallMethod;
use crate::spec::ProductSpec;

/// Minimal semantic version (major.minor.patch), pre-release/build metadata
/// stripped. Sufficient for "is latest newer than current" comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

/// Parse a version string, tolerating a leading `v` and trailing
/// pre-release/build metadata (`-rc.1`, `+build`). Returns `None` if the
/// major/minor/patch core can't be read.
pub fn parse_version(s: &str) -> Option<SemVer> {
    let s = s.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    // Drop pre-release (`-`) and build (`+`) metadata before splitting.
    let core = s.split(['-', '+']).next().unwrap_or(s);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some(SemVer {
        major,
        minor,
        patch,
    })
}

/// Whether `latest` is strictly newer than `current`.
pub fn is_newer(current: &SemVer, latest: &SemVer) -> bool {
    latest > current
}

/// What the caller's update driver should do. Pure planning only — produced by
/// [`plan`], never executed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdatePlan {
    /// Current is >= latest; nothing to do.
    UpToDate,
    /// A newer release exists; carries the latest version string. For a
    /// self-updating install this is paired with [`UpdatePlan::SelfUpdateAllowed`]
    /// semantics — see [`plan`], which returns the package-manager or
    /// self-update variant directly.
    UpdateAvailable { latest: String },
    /// A newer release exists but a package manager owns the binary; the caller
    /// should print this command rather than mutate a file it doesn't own.
    DeferToPackageManager { cmd: String },
    /// A newer release exists and the install is self-updating (cargo-dist
    /// receipt); the caller's Stage-3 driver may mutate in place.
    SelfUpdateAllowed,
}

/// Decide what to do given the current version, the latest known version, and
/// the install method. Pure: no IO.
///
/// - current >= latest (or unparseable comparison) → [`UpdatePlan::UpToDate`].
/// - newer + self-updating install → [`UpdatePlan::SelfUpdateAllowed`].
/// - newer + package-manager install with an upgrade hint →
///   [`UpdatePlan::DeferToPackageManager`].
/// - newer but no upgrade command available → [`UpdatePlan::UpdateAvailable`].
pub fn plan(current: &str, latest: &str, method: &InstallMethod, spec: &ProductSpec) -> UpdatePlan {
    let (Some(cur), Some(lat)) = (parse_version(current), parse_version(latest)) else {
        // Can't compare — treat as up-to-date so we never push a spurious
        // update; a precise check is the caller's (Stage 2) responsibility.
        return UpdatePlan::UpToDate;
    };
    if !is_newer(&cur, &lat) {
        return UpdatePlan::UpToDate;
    }
    if method.is_self_updating() {
        return UpdatePlan::SelfUpdateAllowed;
    }
    // Only genuinely package-manager/cargo-owned installs defer to a manager.
    // `Unknown` (manual install) has an advisory installer hint but no owning
    // manager, so it's a plain `UpdateAvailable` — not "package-manager owned".
    match (method.is_package_managed(), method.upgrade_hint(spec)) {
        (true, Some(cmd)) => UpdatePlan::DeferToPackageManager { cmd },
        _ => UpdatePlan::UpdateAvailable {
            latest: latest.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ProductSpec {
        ProductSpec::new("octos", "octos", "1.0.0", "octos-org/octos", "octos-bundle")
            .with_brew_formula("octos-org/tap/octos")
    }

    #[test]
    fn parse_tolerates_v_prefix_and_metadata() {
        assert_eq!(
            parse_version("v1.2.3"),
            Some(SemVer {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(
            parse_version("1.2.3-rc.1+build5"),
            Some(SemVer {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(
            parse_version("2"),
            Some(SemVer {
                major: 2,
                minor: 0,
                patch: 0
            })
        );
        assert_eq!(parse_version("not-a-version"), None);
    }

    #[test]
    fn is_newer_compares_by_precedence() {
        let a = parse_version("1.2.3").unwrap();
        let b = parse_version("1.2.4").unwrap();
        let c = parse_version("1.3.0").unwrap();
        assert!(is_newer(&a, &b));
        assert!(is_newer(&a, &c));
        assert!(!is_newer(&b, &a));
        assert!(!is_newer(&a, &a));
    }

    #[test]
    fn plan_up_to_date_when_current_ge_latest() {
        assert_eq!(
            plan("1.2.3", "1.2.3", &InstallMethod::Homebrew, &spec()),
            UpdatePlan::UpToDate
        );
        assert_eq!(
            plan("1.3.0", "1.2.9", &InstallMethod::Homebrew, &spec()),
            UpdatePlan::UpToDate
        );
    }

    #[test]
    fn plan_self_update_for_cargo_dist_installer() {
        assert_eq!(
            plan(
                "1.0.0",
                "1.1.0",
                &InstallMethod::CargoDistInstaller,
                &spec()
            ),
            UpdatePlan::SelfUpdateAllowed
        );
    }

    #[test]
    fn plan_defers_to_package_manager_for_brew() {
        match plan("1.0.0", "1.1.0", &InstallMethod::Homebrew, &spec()) {
            UpdatePlan::DeferToPackageManager { cmd } => {
                assert!(cmd.contains("brew upgrade octos-org/tap/octos"));
            }
            other => panic!("expected DeferToPackageManager, got {other:?}"),
        }
    }

    #[test]
    fn plan_up_to_date_on_unparseable() {
        assert_eq!(
            plan("garbage", "1.1.0", &InstallMethod::Homebrew, &spec()),
            UpdatePlan::UpToDate
        );
    }

    #[test]
    fn plan_unknown_install_is_update_available_not_package_manager() {
        // codex: a manual (Unknown) install has an advisory curl|sh hint but is
        // NOT package-manager owned — it must be UpdateAvailable, not deferred.
        assert_eq!(
            plan("1.0.0", "1.1.0", &InstallMethod::Unknown, &spec()),
            UpdatePlan::UpdateAvailable {
                latest: "1.1.0".to_owned()
            }
        );
    }
}
