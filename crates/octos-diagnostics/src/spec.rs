//! [`ProductSpec`] — the product-agnostic seam.
//!
//! Shared diagnostics code must never hardcode `octos-tui` vs `octos`. Callers
//! describe their product once via a `ProductSpec`; everything else
//! (install-method labels/upgrade hints, PATH/shadow locating, asset selection)
//! reads from it.
//!
//! The single most important rule (ADR "Traps"): **`current_version` is passed
//! IN by the caller** (its own `env!("CARGO_PKG_VERSION")`). This crate must
//! never read its *own* `CARGO_PKG_VERSION` to describe the product — that would
//! report the diagnostics-crate version instead of the binary's.

/// How to build the per-OS release asset name for a product. Stage 1 only needs
/// the *shape* (Stage 2 will consume it when the GitHub client lands); we model
/// it as a template prefix joined to the platform triple, e.g.
/// `octos-bundle-<triple>` or `octos-tui-<triple>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetSelector {
    /// Prefix prepended to the target triple (no trailing dash), e.g.
    /// `octos-bundle` → `octos-bundle-aarch64-apple-darwin`.
    pub prefix: String,
}

impl AssetSelector {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }

    /// Build the asset base name for a target triple, e.g.
    /// `octos-bundle-aarch64-apple-darwin`. The archive extension is left to the
    /// caller/Stage 2 (tarball vs zip differs per OS).
    pub fn asset_name(&self, target_triple: &str) -> String {
        format!("{}-{}", self.prefix, target_triple)
    }
}

/// Product description threaded into every shared diagnostic. Constructed by the
/// binary (octos-cli / octos-tui), never inferred from this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductSpec {
    /// Bare binary name as run on PATH (no extension; `.exe` is appended on
    /// Windows by the locator), e.g. `octos` or `octos-tui`.
    pub binary_name: String,
    /// Package / display name, e.g. `octos`.
    pub package_name: String,
    /// The caller's own version — passed IN (its `env!("CARGO_PKG_VERSION")`).
    /// NEVER this crate's `CARGO_PKG_VERSION`.
    pub current_version: String,
    /// `owner/repo` on GitHub, e.g. `octos-org/octos`.
    pub github_repo: String,
    /// Env var holding an optional GitHub token to dodge the unauthenticated
    /// rate limit, e.g. `OCTOS_GITHUB_TOKEN`. Optional auth — the GitHub client
    /// (Stage 2, `github` feature) reads it only when this is `Some` and the var
    /// is set & non-blank; a public repo never requires it.
    pub github_token_env: Option<String>,
    /// Homebrew formula (tap-qualified), e.g. `octos-org/octos/octos`.
    pub brew_formula: Option<String>,
    /// npm package name, e.g. `@octos-org/octos`.
    pub npm_package: Option<String>,
    /// `cargo install` crate name (registry), e.g. `octos-cli`.
    pub cargo_install: Option<String>,
    /// cargo-dist app name used by the shell/PowerShell installer + receipt.
    pub cargo_dist_app: Option<String>,
    /// Per-OS release asset selector.
    pub asset_selector: AssetSelector,
}

impl ProductSpec {
    /// Minimal constructor; optional package-manager fields default to `None`
    /// and can be filled with the builder-style setters below.
    pub fn new(
        binary_name: impl Into<String>,
        package_name: impl Into<String>,
        current_version: impl Into<String>,
        github_repo: impl Into<String>,
        asset_prefix: impl Into<String>,
    ) -> Self {
        Self {
            binary_name: binary_name.into(),
            package_name: package_name.into(),
            current_version: current_version.into(),
            github_repo: github_repo.into(),
            github_token_env: None,
            brew_formula: None,
            npm_package: None,
            cargo_install: None,
            cargo_dist_app: None,
            asset_selector: AssetSelector::new(asset_prefix),
        }
    }

    /// Set the env var name holding an optional GitHub token (rate-limit auth).
    pub fn with_github_token_env(mut self, env_var: impl Into<String>) -> Self {
        self.github_token_env = Some(env_var.into());
        self
    }

    pub fn with_brew_formula(mut self, formula: impl Into<String>) -> Self {
        self.brew_formula = Some(formula.into());
        self
    }

    pub fn with_npm_package(mut self, package: impl Into<String>) -> Self {
        self.npm_package = Some(package.into());
        self
    }

    pub fn with_cargo_install(mut self, crate_name: impl Into<String>) -> Self {
        self.cargo_install = Some(crate_name.into());
        self
    }

    pub fn with_cargo_dist_app(mut self, app: impl Into<String>) -> Self {
        self.cargo_dist_app = Some(app.into());
        self
    }

    /// Platform-aware binary file name (appends `.exe` on Windows).
    pub fn binary_file_name(&self) -> String {
        if cfg!(windows) {
            format!("{}.exe", self.binary_name)
        } else {
            self.binary_name.clone()
        }
    }

    /// `https://github.com/<owner/repo>`.
    pub fn github_url(&self) -> String {
        format!("https://github.com/{}", self.github_repo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_selector_joins_prefix_and_triple() {
        let sel = AssetSelector::new("octos-bundle");
        assert_eq!(
            sel.asset_name("aarch64-apple-darwin"),
            "octos-bundle-aarch64-apple-darwin"
        );
    }

    #[test]
    fn builder_sets_optional_fields() {
        let spec = ProductSpec::new("octos", "octos", "1.2.3", "octos-org/octos", "octos-bundle")
            .with_brew_formula("octos-org/octos/octos")
            .with_npm_package("@octos-org/octos")
            .with_cargo_install("octos-cli")
            .with_cargo_dist_app("octos");
        assert_eq!(spec.current_version, "1.2.3");
        assert_eq!(spec.brew_formula.as_deref(), Some("octos-org/octos/octos"));
        assert_eq!(spec.npm_package.as_deref(), Some("@octos-org/octos"));
        assert_eq!(spec.cargo_install.as_deref(), Some("octos-cli"));
        assert_eq!(spec.cargo_dist_app.as_deref(), Some("octos"));
        assert_eq!(spec.github_url(), "https://github.com/octos-org/octos");
        assert_eq!(
            spec.asset_selector.asset_name("x86_64-unknown-linux-gnu"),
            "octos-bundle-x86_64-unknown-linux-gnu"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn binary_file_name_is_bare_on_unix() {
        let spec = ProductSpec::new("octos", "octos", "0.1.0", "octos-org/octos", "octos-bundle");
        assert_eq!(spec.binary_file_name(), "octos");
    }
}
