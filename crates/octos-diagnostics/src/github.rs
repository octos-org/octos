//! Minimal **blocking** GitHub Releases client for `update --check` and the
//! `doctor` network category (Stage 2). Behind the `github` feature → `reqwest`.
//!
//! Ported, product-agnostic, from octoscode's `cmd/github.rs`: a plain blocking
//! `reqwest` GET against the public Releases API. No auth is required for public
//! repos, but the product's token env var (`spec.github_token_env`) is honored
//! when set to dodge the unauthenticated rate limit — optional, never a hard
//! requirement.
//!
//! Stage 2 is **read + plan only**: [`latest_release`] fetches the newest
//! release, [`reachability`] probes `api.github.com`, and [`update_check`]
//! threads the result through the pure Stage-1 [`crate::plan`]. NOTHING here
//! mutates a binary — self-update is Stage 3.

use std::time::Duration;

use eyre::{Result, WrapErr, eyre};

use crate::install_method::InstallMethod;
use crate::spec::ProductSpec;
use crate::update::{UpdatePlan, plan};

const API_BASE: &str = "https://api.github.com";
const TIMEOUT: Duration = Duration::from_secs(10);

/// User-Agent for GitHub API calls. The version is the *diagnostics* crate's
/// version — this identifies the HTTP client, not the product, so it is the one
/// legitimate `CARGO_PKG_VERSION` use here (the product version travels in
/// [`ProductSpec::current_version`]).
const USER_AGENT: &str = concat!("octos-diagnostics/", env!("CARGO_PKG_VERSION"));

/// The release info `update --check`/`doctor` care about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// Version with any leading `v` stripped, e.g. `0.1.2`.
    pub version: String,
    /// The raw release tag, e.g. `v0.1.2`.
    pub tag: String,
    /// `browser_download_url` of the asset matching this host's triple, if the
    /// release published one. `None` when no matching asset is present (a
    /// package-manager install never needs it).
    pub asset_url: Option<String>,
}

/// Result of the `api.github.com` reachability probe used by `doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reachability {
    /// `api.github.com` answered successfully (or with a benign rate-limit;
    /// see [`Reachability::reason`] semantics — a 403 is folded into
    /// `Unreachable` only for connectivity, never for planning).
    Reachable,
    /// Not reachable, or answered with an error/rate-limit status. `reason`
    /// carries a short human-readable cause (network error or `HTTP <status>`).
    Unreachable { reason: String },
}

/// The platform target triple for asset selection, derived from the compile
/// target (`std::env::consts::{OS, ARCH}`). Mirrors the cargo-dist asset names
/// (`<prefix>-<triple>`). Unknown OS/arch combinations fall back to a
/// best-effort `<arch>-unknown-<os>` so selection degrades gracefully rather
/// than panicking.
pub fn host_target_triple() -> String {
    let arch = std::env::consts::ARCH; // e.g. "aarch64", "x86_64"
    match std::env::consts::OS {
        "macos" => format!("{arch}-apple-darwin"),
        "linux" => format!("{arch}-unknown-linux-gnu"),
        "windows" => format!("{arch}-pc-windows-msvc"),
        other => format!("{arch}-unknown-{other}"),
    }
}

fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(TIMEOUT)
        .build()
        .wrap_err("failed to build HTTP client")
}

/// The product's GitHub token from its configured env var, if set and non-blank.
/// Optional — used only to dodge the unauthenticated rate limit.
fn token(spec: &ProductSpec) -> Option<String> {
    let var = spec.github_token_env.as_deref()?;
    std::env::var(var).ok().filter(|t| !t.trim().is_empty())
}

fn authed(
    req: reqwest::blocking::RequestBuilder,
    spec: &ProductSpec,
) -> reqwest::blocking::RequestBuilder {
    let req = req
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    match token(spec) {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

/// Whether `api.github.com` is reachable (a cheap GET against the API root).
/// Used by `doctor`'s network check. Any non-success status (including a 403
/// rate-limit) or transport error becomes [`Reachability::Unreachable`] with a
/// short reason — `doctor` renders this as a WARN, never a hard failure.
pub fn reachability(spec: &ProductSpec) -> Reachability {
    let client = match client() {
        Ok(c) => c,
        Err(_) => {
            return Reachability::Unreachable {
                reason: "failed to build HTTP client".into(),
            };
        }
    };
    match authed(client.get(API_BASE), spec).send() {
        Ok(resp) if resp.status().is_success() => Reachability::Reachable,
        Ok(resp) if resp.status().as_u16() == 403 => Reachability::Unreachable {
            reason: "rate-limited (HTTP 403)".into(),
        },
        Ok(resp) => Reachability::Unreachable {
            reason: format!("HTTP {}", resp.status()),
        },
        Err(err) => Reachability::Unreachable {
            reason: err.to_string(),
        },
    }
}

/// Fetch the latest published release for `spec.github_repo` from the public
/// GitHub Releases API, honoring the spec's token env var when set.
pub fn latest_release(spec: &ProductSpec) -> Result<ReleaseInfo> {
    let client = client()?;
    let url = format!("{API_BASE}/repos/{}/releases/latest", spec.github_repo);
    let resp = authed(client.get(&url), spec)
        .send()
        .wrap_err("failed to reach api.github.com")?;
    let status = resp.status();
    if !status.is_success() {
        return Err(eyre!(
            "GitHub returned {status} for the latest {} release",
            spec.package_name
        ));
    }
    let payload: serde_json::Value = resp
        .json()
        .wrap_err("failed to decode GitHub release payload")?;
    parse_release(&payload, spec)
}

/// Pure parser: turn a GitHub `/releases/latest` JSON payload into a
/// [`ReleaseInfo`], selecting the asset whose name matches this host's triple.
/// Network-free so it is unit-testable against a fixture (no live github.com).
pub fn parse_release(payload: &serde_json::Value, spec: &ProductSpec) -> Result<ReleaseInfo> {
    let tag = payload["tag_name"]
        .as_str()
        .ok_or_else(|| eyre!("release payload is missing `tag_name`"))?
        .to_string();
    let version = tag.strip_prefix('v').unwrap_or(&tag).to_string();

    // Asset selection: the cargo-dist asset is `<prefix>-<triple>` with some
    // archive extension (`.tar.gz`/`.zip`). Match by *prefix* of the asset name
    // so we don't have to hardcode the extension per OS.
    let wanted = spec.asset_selector.asset_name(&host_target_triple());
    let asset_url = payload["assets"].as_array().and_then(|assets| {
        assets
            .iter()
            .filter_map(|a| {
                let name = a["name"].as_str()?;
                let url = a["browser_download_url"].as_str()?;
                Some((name, url))
            })
            .find(|(name, _)| name.starts_with(&wanted))
            .map(|(_, url)| url.to_string())
    });

    Ok(ReleaseInfo {
        version,
        tag,
        asset_url,
    })
}

/// Fetch the latest release and produce an [`UpdatePlan`] via the pure Stage-1
/// [`plan`]. NO mutation: this only decides what *should* happen given the
/// current version (from `spec.current_version`), the fetched latest, and the
/// install `method`. The caller (Stage 3 driver) executes the plan.
pub fn update_check(spec: &ProductSpec, method: &InstallMethod) -> Result<UpdatePlan> {
    let latest = latest_release(spec)?;
    Ok(plan(&spec.current_version, &latest.version, method, spec))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn octos_spec() -> ProductSpec {
        ProductSpec::new("octos", "octos", "1.0.0", "octos-org/octos", "octos-bundle")
            .with_brew_formula("octos-org/octos/octos")
            .with_cargo_dist_app("octos")
    }

    /// A GitHub `/releases/latest` fixture carrying assets for several triples.
    fn releases_fixture() -> serde_json::Value {
        serde_json::json!({
            "tag_name": "v9.9.9",
            "name": "octos 9.9.9",
            "prerelease": false,
            "draft": false,
            "assets": [
                {
                    "name": "octos-bundle-aarch64-apple-darwin.tar.gz",
                    "browser_download_url": "https://github.com/octos-org/octos/releases/download/v9.9.9/octos-bundle-aarch64-apple-darwin.tar.gz"
                },
                {
                    "name": "octos-bundle-x86_64-unknown-linux-gnu.tar.gz",
                    "browser_download_url": "https://github.com/octos-org/octos/releases/download/v9.9.9/octos-bundle-x86_64-unknown-linux-gnu.tar.gz"
                },
                {
                    "name": "octos-bundle-x86_64-pc-windows-msvc.zip",
                    "browser_download_url": "https://github.com/octos-org/octos/releases/download/v9.9.9/octos-bundle-x86_64-pc-windows-msvc.zip"
                }
            ]
        })
    }

    #[test]
    fn parse_release_reads_tag_and_strips_v_prefix() {
        let info = parse_release(&releases_fixture(), &octos_spec()).expect("parses");
        assert_eq!(info.tag, "v9.9.9");
        assert_eq!(info.version, "9.9.9");
    }

    #[test]
    fn parse_release_selects_asset_for_this_host_triple() {
        // The asset URL chosen must match THIS host's triple (whichever the test
        // runs on), proving asset selection uses the live triple + spec prefix.
        let info = parse_release(&releases_fixture(), &octos_spec()).expect("parses");
        let triple = host_target_triple();
        let expected_prefix = format!("octos-bundle-{triple}");
        // Our fixture only carries darwin/linux-gnu/windows-msvc assets; on any
        // of those hosts we must find a matching URL.
        if [
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
        ]
        .contains(&triple.as_str())
        {
            let url = info.asset_url.expect("asset for this host triple");
            assert!(
                url.contains(&expected_prefix),
                "asset url {url} should contain {expected_prefix}"
            );
        }
    }

    #[test]
    fn parse_release_returns_none_asset_when_no_triple_matches() {
        // A release that published no asset for any known triple → asset_url None,
        // but tag/version still parse (package-manager installs don't need an asset).
        let payload = serde_json::json!({
            "tag_name": "v2.0.0",
            "assets": [
                { "name": "SOURCE.tar.gz", "browser_download_url": "https://example.com/src" }
            ]
        });
        let info = parse_release(&payload, &octos_spec()).expect("parses");
        assert_eq!(info.version, "2.0.0");
        assert!(info.asset_url.is_none());
    }

    #[test]
    fn parse_release_errors_without_tag_name() {
        let payload = serde_json::json!({ "assets": [] });
        assert!(parse_release(&payload, &octos_spec()).is_err());
    }

    #[test]
    fn host_target_triple_is_dash_joined_arch_first() {
        let t = host_target_triple();
        assert!(
            t.starts_with(std::env::consts::ARCH),
            "triple {t} starts with arch"
        );
        assert!(t.contains('-'));
    }

    #[test]
    fn token_reads_only_the_specs_env_var() {
        // No env var configured on the spec → never reads anything.
        let mut spec = octos_spec();
        assert!(token(&spec).is_none());
        spec.github_token_env = Some("OCTOS_DIAG_TEST_TOKEN_UNSET_XYZ".into());
        // Unset → None (we don't fall through to a default var).
        assert!(token(&spec).is_none());
    }

    // --- live-network tests (CI-safe: ignored) ------------------------------

    #[test]
    #[ignore = "hits live api.github.com; run manually with --ignored"]
    fn live_reachability_is_reachable() {
        assert_eq!(reachability(&octos_spec()), Reachability::Reachable);
    }

    #[test]
    #[ignore = "hits live api.github.com; run manually with --ignored"]
    fn live_latest_release_parses() {
        let info = latest_release(&octos_spec()).expect("fetches latest");
        assert!(!info.version.is_empty());
        assert!(info.tag.starts_with('v') || !info.tag.is_empty());
    }

    #[test]
    #[ignore = "hits live api.github.com; run manually with --ignored"]
    fn live_update_check_returns_a_plan() {
        // Current version 0.0.0 forces "newer available" so we exercise planning.
        let mut spec = octos_spec();
        spec.current_version = "0.0.0".into();
        let plan = update_check(&spec, &InstallMethod::Unknown).expect("plans");
        assert_ne!(plan, UpdatePlan::UpToDate);
    }
}
