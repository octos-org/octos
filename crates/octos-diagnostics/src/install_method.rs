//! Install-method detection, product-agnostic.
//!
//! Ported from octos-tui's `install_method.rs`, but every product-specific
//! string (formula, package, repo, crate) now comes from the [`ProductSpec`]
//! rather than being hardcoded. The path classifier ([`classify_path`]) stays
//! pure/testable; [`detect`] wires it to the live host.
//!
//! Stage 1 is dependency-light: the cargo-dist install-receipt probe is a
//! Stage-3 stub that always returns "no receipt", so `detect` classifies via
//! path heuristics + env. When the `update` engine lands (Stage 3) this is the
//! one seam that flips to an axoupdater receipt check.

use std::path::{Path, PathBuf};

use crate::spec::ProductSpec;

/// How a product binary was installed. Drives upgrade advice (self-update vs.
/// print-the-command) and `doctor`'s fix lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// Installed by the cargo-dist shell/PowerShell installer — a receipt is
    /// present and (Stage 3) the binary can self-update in place.
    CargoDistInstaller,
    /// Installed via Homebrew.
    Homebrew,
    /// Installed via npm global.
    Npm,
    /// `cargo install <crate>` from the crates.io registry.
    CargoRegistry,
    /// `cargo install --git …` from the GitHub repo.
    CargoGit,
    /// Anything else (distro package, manual copy, dev build, …).
    Unknown,
}

impl InstallMethod {
    /// Short, stable identifier for `--json` output.
    pub fn id(&self) -> &'static str {
        match self {
            InstallMethod::CargoDistInstaller => "cargo-dist-installer",
            InstallMethod::Homebrew => "homebrew",
            InstallMethod::Npm => "npm",
            InstallMethod::CargoRegistry => "cargo-registry",
            InstallMethod::CargoGit => "cargo-git",
            InstallMethod::Unknown => "unknown",
        }
    }

    /// Human-readable label.
    pub fn label(&self) -> &'static str {
        match self {
            InstallMethod::CargoDistInstaller => "cargo-dist installer (self-updating)",
            InstallMethod::Homebrew => "Homebrew",
            InstallMethod::Npm => "npm (global)",
            InstallMethod::CargoRegistry => "cargo install (crates.io)",
            InstallMethod::CargoGit => "cargo install --git",
            InstallMethod::Unknown => "unknown / distro package",
        }
    }

    /// The exact command the user should run to upgrade, built from the
    /// [`ProductSpec`]. `None` for the self-updating installer (the caller
    /// prints a self-update message instead). For package-manager methods whose
    /// spec field is absent, falls back to the one-line installer hint.
    pub fn upgrade_hint(&self, spec: &ProductSpec) -> Option<String> {
        match self {
            InstallMethod::CargoDistInstaller => None,
            InstallMethod::Homebrew => spec
                .brew_formula
                .as_ref()
                .map(|f| format!("brew update && brew upgrade {f}"))
                .or_else(|| Some(self.installer_hint(spec))),
            InstallMethod::Npm => spec
                .npm_package
                .as_ref()
                .map(|p| format!("npm update -g {p}"))
                .or_else(|| Some(self.installer_hint(spec))),
            InstallMethod::CargoRegistry => spec
                .cargo_install
                .as_ref()
                .map(|c| format!("cargo install {c} --force"))
                .or_else(|| Some(self.installer_hint(spec))),
            InstallMethod::CargoGit => Some(format!(
                "cargo install --git {} {} --force",
                spec.github_url(),
                spec.cargo_install
                    .clone()
                    .unwrap_or_else(|| spec.binary_name.clone())
            )),
            // No package manager owns the binary; suggest the one-line installer.
            InstallMethod::Unknown => Some(self.installer_hint(spec)),
        }
    }

    /// The one-line cargo-dist installer command for this product.
    fn installer_hint(&self, spec: &ProductSpec) -> String {
        let app = spec
            .cargo_dist_app
            .clone()
            .unwrap_or_else(|| spec.binary_name.clone());
        format!(
            "curl --proto '=https' --tlsv1.2 -LsSf \
{}/releases/latest/download/{app}-installer.sh | sh",
            spec.github_url()
        )
    }

    /// Whether `update` can mutate the binary in place (only the cargo-dist
    /// installer; everything else defers to its package manager).
    pub fn is_self_updating(&self) -> bool {
        matches!(self, InstallMethod::CargoDistInstaller)
    }

    /// Whether this install is owned by a package manager (or cargo) the user
    /// upgrades through — as opposed to a manual install we can't drive.
    /// `Unknown` is NOT package-managed: there's no owning manager to defer to,
    /// even though `upgrade_hint` offers an advisory `curl | sh` reinstall line.
    pub fn is_package_managed(&self) -> bool {
        matches!(
            self,
            InstallMethod::Homebrew
                | InstallMethod::Npm
                | InstallMethod::CargoRegistry
                | InstallMethod::CargoGit
        )
    }
}

/// Inputs to the pure path classifier. All prefixes are optional because they
/// are resolved best-effort from the host.
#[derive(Debug, Default, Clone)]
pub struct PathClassifierInput {
    /// Resolved `current_exe()` path (canonicalized when possible).
    pub current_exe: PathBuf,
    /// Confirmed Homebrew prefixes to test as ancestors. `/usr/local` is
    /// included only when brew actually lives there.
    pub brew_prefixes: Vec<PathBuf>,
    /// npm global root(s) (`npm root -g`, i.e. `…/lib/node_modules`).
    pub npm_global_roots: Vec<PathBuf>,
    /// `~/.cargo/bin`.
    pub cargo_bin: Option<PathBuf>,
    /// Whether `.crates2.json` records this crate as a `--git` source.
    /// `Some(true)` → git, `Some(false)` → registry, `None` → unknown.
    pub cargo_source_is_git: Option<bool>,
}

/// Classify the binary purely from its path + resolved prefixes (no receipt).
/// First match wins.
pub fn classify_path(input: &PathClassifierInput) -> InstallMethod {
    let exe = &input.current_exe;

    // npm global: under an npm root, or any ancestor is a node_modules dir.
    if input
        .npm_global_roots
        .iter()
        .any(|root| is_ancestor(root, exe))
        || path_has_segment(exe, "node_modules")
    {
        return InstallMethod::Npm;
    }

    // Homebrew: under a brew prefix, or anywhere under a `Cellar` dir.
    if input
        .brew_prefixes
        .iter()
        .any(|prefix| is_ancestor(prefix, exe))
        || path_has_segment(exe, "Cellar")
    {
        return InstallMethod::Homebrew;
    }

    // cargo install destination (`~/.cargo/bin/<bin>`).
    if input
        .cargo_bin
        .as_ref()
        .is_some_and(|bin| is_ancestor(bin, exe))
        || path_has_segments(exe, &[".cargo", "bin"])
    {
        return match input.cargo_source_is_git {
            Some(true) => InstallMethod::CargoGit,
            // Default cargo installs come from the registry; treat unknown
            // source as registry so the printed command is the common case.
            Some(false) | None => InstallMethod::CargoRegistry,
        };
    }

    InstallMethod::Unknown
}

/// Returns true when `ancestor` is a component-wise path prefix of `path`.
fn is_ancestor(ancestor: &Path, path: &Path) -> bool {
    if ancestor.as_os_str().is_empty() {
        return false;
    }
    let mut a = ancestor.components();
    let mut p = path.components();
    loop {
        match (a.next(), p.next()) {
            (Some(ac), Some(pc)) if ac == pc => continue,
            (Some(_), _) => return false, // ancestor longer / diverged
            (None, _) => return true,     // ancestor fully consumed → prefix
        }
    }
}

/// Whether any path component equals `segment`.
fn path_has_segment(path: &Path, segment: &str) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_string_lossy() == segment)
}

/// Whether `segments` appear as a contiguous run of path components.
fn path_has_segments(path: &Path, segments: &[&str]) -> bool {
    let comps: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    comps
        .windows(segments.len())
        .any(|w| w.iter().zip(segments).all(|(a, b)| a == b))
}

/// Detect the install method for `spec` against the live host.
///
/// Stage 1: the cargo-dist receipt probe is a stub (always "absent"), so this
/// classifies via [`classify_path`] over `current_exe()` + best-effort
/// package-manager prefix resolution. Stage 3 flips [`receipt_for_this_executable`]
/// to a real axoupdater receipt check behind an `update` feature.
pub fn detect(spec: &ProductSpec) -> InstallMethod {
    if receipt_for_this_executable(spec) {
        return InstallMethod::CargoDistInstaller;
    }
    classify_path(&live_classifier_input(spec))
}

/// Stage-1 stub: there is no axoupdater receipt to load without the (Stage-3)
/// `update` feature, so path heuristics decide. Kept as a named seam so Stage 3
/// only has to change this function.
fn receipt_for_this_executable(_spec: &ProductSpec) -> bool {
    false
}

/// Assemble the live classifier input from `current_exe()` + best-effort
/// package-manager prefix resolution.
fn live_classifier_input(spec: &ProductSpec) -> PathClassifierInput {
    let current_exe = std::env::current_exe()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .unwrap_or_default();

    PathClassifierInput {
        current_exe,
        brew_prefixes: brew_prefixes(),
        npm_global_roots: npm_global_roots(),
        cargo_bin: cargo_bin(),
        cargo_source_is_git: cargo_source_is_git(spec),
    }
}

/// Candidate Homebrew prefixes. `/opt/homebrew` is always a candidate (Apple
/// Silicon default); `/usr/local` is added only when `brew --prefix` confirms
/// brew lives there.
fn brew_prefixes() -> Vec<PathBuf> {
    let mut prefixes = vec![PathBuf::from("/opt/homebrew")];
    if let Ok(out) = std::process::Command::new("brew").arg("--prefix").output() {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                prefixes.push(PathBuf::from(p));
            }
        }
    }
    prefixes
}

/// `npm root -g` only (the specific `…/lib/node_modules` path that owns global
/// packages; deliberately NOT `npm prefix -g`).
fn npm_global_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(out) = std::process::Command::new("npm")
        .args(["root", "-g"])
        .output()
    {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                roots.push(PathBuf::from(p));
            }
        }
    }
    roots
}

/// `~/.cargo/bin`, honoring `CARGO_HOME`.
fn cargo_bin() -> Option<PathBuf> {
    cargo_home().map(|home| home.join("bin"))
}

fn cargo_home() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("CARGO_HOME") {
        if !home.is_empty() {
            return Some(PathBuf::from(home));
        }
    }
    home_dir().map(|h| h.join(".cargo"))
}

/// Inspect `~/.cargo/.crates2.json` for the recorded source of the crate.
/// Returns `Some(true)` for a git source, `Some(false)` for a registry source,
/// `None` if not found / unparseable. Keys look like
/// `<crate> <version> (registry+https://…)` or `(git+https://…)`.
fn cargo_source_is_git(spec: &ProductSpec) -> Option<bool> {
    let crate_name = spec.cargo_install.as_deref().unwrap_or(&spec.binary_name);
    let path = cargo_home()?.join(".crates2.json");
    let contents = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    let installs = value.get("installs")?.as_object()?;
    let prefix = format!("{crate_name} ");
    for key in installs.keys() {
        if key.starts_with(&prefix) {
            return Some(key.contains("(git+"));
        }
    }
    None
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::ProductSpec;

    fn input(exe: &str) -> PathClassifierInput {
        PathClassifierInput {
            current_exe: PathBuf::from(exe),
            ..Default::default()
        }
    }

    fn octos_spec() -> ProductSpec {
        ProductSpec::new("octos", "octos", "1.0.0", "octos-org/octos", "octos-bundle")
            .with_brew_formula("octos-org/tap/octos")
            .with_npm_package("@octos-org/octos")
            .with_cargo_install("octos-cli")
            .with_cargo_dist_app("octos")
    }

    #[test]
    fn should_classify_npm_global_when_under_npm_root() {
        let mut i = input("/usr/local/lib/node_modules/@octos-org/octos/bin/octos");
        i.npm_global_roots = vec![PathBuf::from("/usr/local/lib/node_modules")];
        assert_eq!(classify_path(&i), InstallMethod::Npm);
    }

    #[test]
    fn should_classify_npm_when_node_modules_in_path() {
        let i = input("/home/u/.nvm/versions/node/v20/lib/node_modules/octos/octos");
        assert_eq!(classify_path(&i), InstallMethod::Npm);
    }

    #[test]
    fn should_classify_homebrew_when_under_prefix() {
        let mut i = input("/opt/homebrew/bin/octos");
        i.brew_prefixes = vec![PathBuf::from("/opt/homebrew")];
        assert_eq!(classify_path(&i), InstallMethod::Homebrew);
    }

    #[test]
    fn should_classify_homebrew_when_cellar_segment_present() {
        let i = input("/usr/local/Cellar/octos/1.0.0/bin/octos");
        assert_eq!(classify_path(&i), InstallMethod::Homebrew);
    }

    #[test]
    fn should_classify_usr_local_as_unknown_when_no_brew_present() {
        // No brew present → `/usr/local` is NOT a brew prefix → Unknown (avoids
        // printing a wrong `brew upgrade`). Mirrors octos-tui finding #1.
        let i = input("/usr/local/bin/octos");
        assert_eq!(classify_path(&i), InstallMethod::Unknown);
    }

    #[test]
    fn should_classify_cellar_as_homebrew_not_npm_with_real_npm_root() {
        let mut i = input("/opt/homebrew/Cellar/octos/1.0.0/bin/octos");
        i.npm_global_roots = vec![PathBuf::from("/opt/homebrew/lib/node_modules")];
        i.brew_prefixes = vec![PathBuf::from("/opt/homebrew")];
        assert_eq!(classify_path(&i), InstallMethod::Homebrew);
    }

    #[test]
    fn should_classify_cargo_registry_without_git_source() {
        let mut i = input("/home/u/.cargo/bin/octos");
        i.cargo_bin = Some(PathBuf::from("/home/u/.cargo/bin"));
        i.cargo_source_is_git = Some(false);
        assert_eq!(classify_path(&i), InstallMethod::CargoRegistry);
    }

    #[test]
    fn should_classify_cargo_git_when_source_is_git() {
        let mut i = input("/home/u/.cargo/bin/octos");
        i.cargo_bin = Some(PathBuf::from("/home/u/.cargo/bin"));
        i.cargo_source_is_git = Some(true);
        assert_eq!(classify_path(&i), InstallMethod::CargoGit);
    }

    #[test]
    fn should_default_cargo_to_registry_when_source_unknown() {
        let i = input("/home/u/.cargo/bin/octos");
        assert_eq!(classify_path(&i), InstallMethod::CargoRegistry);
    }

    #[test]
    fn should_classify_unknown_for_distro_path() {
        let i = input("/usr/bin/octos");
        assert_eq!(classify_path(&i), InstallMethod::Unknown);
    }

    #[test]
    fn npm_takes_precedence_over_cargo_bin_when_both_match() {
        let mut i = input("/x/.cargo/bin/node_modules/octos/octos");
        i.cargo_bin = Some(PathBuf::from("/x/.cargo/bin"));
        assert_eq!(classify_path(&i), InstallMethod::Npm);
    }

    #[test]
    fn upgrade_hints_are_method_and_spec_specific() {
        let spec = octos_spec();
        assert!(
            InstallMethod::CargoDistInstaller
                .upgrade_hint(&spec)
                .is_none()
        );
        assert_eq!(
            InstallMethod::Homebrew.upgrade_hint(&spec).unwrap(),
            "brew update && brew upgrade octos-org/tap/octos"
        );
        assert_eq!(
            InstallMethod::Npm.upgrade_hint(&spec).unwrap(),
            "npm update -g @octos-org/octos"
        );
        assert_eq!(
            InstallMethod::CargoRegistry.upgrade_hint(&spec).unwrap(),
            "cargo install octos-cli --force"
        );
        assert_eq!(
            InstallMethod::CargoGit.upgrade_hint(&spec).unwrap(),
            "cargo install --git https://github.com/octos-org/octos octos-cli --force"
        );
        assert!(
            InstallMethod::Unknown
                .upgrade_hint(&spec)
                .unwrap()
                .contains("octos-installer.sh")
        );
    }

    #[test]
    fn upgrade_hint_falls_back_to_installer_when_pkg_field_absent() {
        // A spec lacking brew/npm/cargo fields must still produce a usable hint
        // (the one-line installer), not panic or return None.
        let bare = ProductSpec::new("octos", "octos", "1.0.0", "octos-org/octos", "octos-bundle");
        assert!(
            InstallMethod::Homebrew
                .upgrade_hint(&bare)
                .unwrap()
                .contains("installer.sh")
        );
        assert!(
            InstallMethod::Npm
                .upgrade_hint(&bare)
                .unwrap()
                .contains("installer.sh")
        );
    }

    #[test]
    fn only_cargo_dist_is_self_updating() {
        assert!(InstallMethod::CargoDistInstaller.is_self_updating());
        for m in [
            InstallMethod::Homebrew,
            InstallMethod::Npm,
            InstallMethod::CargoRegistry,
            InstallMethod::CargoGit,
            InstallMethod::Unknown,
        ] {
            assert!(!m.is_self_updating(), "{} should not self-update", m.id());
        }
    }

    #[test]
    fn is_ancestor_is_component_wise_not_substring() {
        assert!(!is_ancestor(
            Path::new("/opt/home"),
            Path::new("/opt/homebrew/bin/octos")
        ));
        assert!(is_ancestor(
            Path::new("/opt/homebrew"),
            Path::new("/opt/homebrew/bin/octos")
        ));
    }
}
