//! `octos doctor` — flutter-doctor-style local diagnostics (Stage 1).
//!
//! Runs the shared, product-agnostic checks from `octos-diagnostics` against an
//! octos-server [`ProductSpec`] and renders them through the shared [`Report`]:
//! one line per check (`[✓]` pass / `[!]` warn / `[✗]` fail), grouped by
//! category, each non-pass line followed by an indented `→ fix:` action,
//! closing with a one-line summary. `--json` emits the support bundle;
//! `--verbose` adds resolved paths/versions; `--strict` promotes warnings to
//! failures.
//!
//! Local checks (Stage 1): binary/install-method/on-path/shadow, terminal,
//! config+data writability, and a structural protocol-skew check against the
//! server's compiled-in capabilities.
//!
//! **Stage 2** adds a `Network` category (behind octos-diagnostics' `github`
//! feature, which octos-cli enables): GitHub reachability via the shared
//! `reachability()` and a best-effort newer-release check via `update_check`.
//! Both are advisory — a network/API failure WARNs, never FAILs, so `doctor`
//! works offline. There is still NO update mutation (Stage 3), and the LIVE-WS
//! `config/capabilities/list` probe for protocol skew is left as a documented
//! `// TODO Stage 2.5` (it needs a client WS connection).

use std::path::PathBuf;

use clap::Args;
use eyre::Result;
use octos_core::ui_protocol::{UI_PROTOCOL_KNOWN_FEATURES, UI_PROTOCOL_V1, UiProtocolCapabilities};
use octos_diagnostics::{
    Check, InstallMethod, ProductSpec, Reachability, Report, UpdatePlan, config_writability_check,
    data_writability_check, detect, locate, on_path_check, protocol_skew_check, reachability,
    shadow_check, terminal_checks, update_check,
};

use super::Executable;

const CAT_BINARY: &str = "Binary & version";
const CAT_NETWORK: &str = "Network";

/// Run local environment diagnostics for the octos server.
#[derive(Debug, Args)]
pub struct DoctorCommand {
    /// Emit machine-readable JSON (support bundle).
    #[arg(long)]
    pub json: bool,
    /// Add resolved paths / versions to each line.
    #[arg(long)]
    pub verbose: bool,
    /// Promote warnings to failures (affects exit code).
    #[arg(long)]
    pub strict: bool,
    /// Data dir override (defaults to the resolved `~/.octos`).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

/// Build the octos-server [`ProductSpec`]. `current_version` is the CLI's OWN
/// `CARGO_PKG_VERSION`, passed IN here — never the diagnostics crate's.
fn octos_server_spec() -> ProductSpec {
    ProductSpec::new(
        "octos",                   // binary on PATH
        "octos",                   // package / display name
        env!("CARGO_PKG_VERSION"), // passed IN — octos-cli's own version
        "octos-org/octos",         // github repo
        "octos-bundle",            // asset prefix → octos-bundle-<triple>
    )
    .with_github_token_env("OCTOS_GITHUB_TOKEN")
    .with_brew_formula("octos-org/octos/octos")
    .with_npm_package("@octos-org/octos")
    .with_cargo_install("octos-cli")
    .with_cargo_dist_app("octos")
}

impl Executable for DoctorCommand {
    fn execute(self) -> Result<()> {
        // Network checks run for the real `octos doctor` invocation (Stage 2).
        let report = build_report(&self, true)?;
        if self.json {
            let bundle = report.to_json(
                self.strict,
                "octos",
                env!("CARGO_PKG_VERSION"),
                UI_PROTOCOL_V1,
                octos_core::ui_protocol::UI_PROTOCOL_SCHEMA_VERSION,
            );
            println!("{}", serde_json::to_string_pretty(&bundle)?);
        } else {
            print!("{}", report.render(self.verbose, self.strict));
        }
        std::process::exit(report.exit_code(self.strict));
    }
}

/// Assemble the full report. Separated from `execute` so it does not call
/// `process::exit` and can be exercised by tests. `with_network` gates the
/// Stage-2 Network category (GitHub reachability + newer-release check) so unit
/// tests stay offline/deterministic; the real command always passes `true`.
fn build_report(cmd: &DoctorCommand, with_network: bool) -> Result<Report> {
    let spec = octos_server_spec();
    let mut report = Report::default();

    // --- Binary & version --------------------------------------------------
    let current_exe = std::env::current_exe().ok();
    match &current_exe {
        Some(exe) => report.push(
            Check::pass(
                CAT_BINARY,
                "octos binary",
                format!("v{}", spec.current_version),
            )
            .with_value(exe.display().to_string()),
        ),
        None => report.push(Check::warn(
            CAT_BINARY,
            "octos binary",
            "could not resolve current executable",
            "ensure octos is on a real filesystem path",
        )),
    }

    let method = detect(&spec);
    report.push(Check::pass(CAT_BINARY, "install method", method.label()).with_value(method.id()));
    if let Some(hint) = method.upgrade_hint(&spec) {
        // Informational only in Stage 1 (no version comparison without the
        // network); surface the per-method upgrade command as the value, and
        // label the OWNERSHIP honestly — self-update vs package-manager vs a
        // manual install whose hint is just an advisory reinstall line.
        let detail = if method.is_self_updating() {
            "self-updating (octos update)"
        } else if method.is_package_managed() {
            "package-manager owned"
        } else {
            "manual install — upgrade by reinstalling"
        };
        report.push(Check::pass(CAT_BINARY, "upgrade path", detail).with_value(hint));
    }

    let located = locate(&spec);
    report.push(on_path_check(
        &located,
        current_exe.as_deref(),
        &method,
        &spec,
    ));
    report.push(shadow_check(&located, &method, &spec));

    // --- Network (Stage 2: github feature) --------------------------------
    // GitHub reachability + a best-effort "newer release available" check.
    // Both are advisory: a network failure WARNs (never FAILs) so `doctor`
    // works offline. The LIVE-WS `config/capabilities/list` probe for protocol
    // skew is still TODO Stage 2.5 (it needs a client WS connection); the
    // compiled-in `protocol_skew_check` from Stage 1 remains the skew check.
    if with_network {
        report.extend(network_checks(&spec, &method));
    }

    // --- Terminal environment ---------------------------------------------
    report.extend(terminal_checks());

    // --- Config & data -----------------------------------------------------
    // Resolve the REAL config_home (~/.config/octos by default) and data_dir
    // (~/.octos) via the canonical resolver so doctor reports what octos
    // actually reads/writes. Read-only here: no migrations, no dir creation.
    let ctx = crate::config_context::resolve_config_context(cmd.data_dir.as_deref());
    report.push(config_writability_check(&ctx.config_home));
    report.push(data_writability_check(&ctx.data_dir));

    // --- Backend / protocol skew ------------------------------------------
    // The server's own compiled-in capabilities are authoritative for the
    // structural skew check. `first_server_slice()` advertises the protocol's
    // full known-feature registry (what `octos serve` actually negotiates), so
    // comparing it against that same registry confirms the build's octos-core
    // matches the protocol it ships — a divergence would surface here.
    // TODO Stage 2.5: replace the compiled-in caps with a LIVE WS
    // `config/capabilities/list` probe against a configured/running server (it
    // needs a client WS connection, deliberately out of Stage 2 scope). Until
    // then the compiled-in `protocol_skew_check` is authoritative.
    let server_caps = UiProtocolCapabilities::first_server_slice();
    report.push(protocol_skew_check(
        &server_caps,
        UI_PROTOCOL_KNOWN_FEATURES.iter().copied(),
    ));

    Ok(report)
}

/// Network category (Stage 2, `github` feature): GitHub API reachability + a
/// best-effort newer-release check. Both are advisory — a network/API failure
/// produces a `[!]` WARN, never a `[✗]` FAIL, so `doctor` never blocks offline.
fn network_checks(spec: &ProductSpec, method: &InstallMethod) -> Vec<Check> {
    let mut checks = Vec::new();

    // GitHub reachability (shared probe) — called once, result reused below.
    let reach = reachability(spec);
    let reachable = matches!(reach, Reachability::Reachable);
    match reach {
        Reachability::Reachable => checks.push(
            Check::pass(CAT_NETWORK, "GitHub reachable", "api.github.com responded")
                .with_value("https://api.github.com".to_string()),
        ),
        Reachability::Unreachable { reason } => checks.push(Check::warn(
            CAT_NETWORK,
            "GitHub reachable",
            format!("could not reach api.github.com: {reason}"),
            "check network/proxy/DNS, or set OCTOS_GITHUB_TOKEN to dodge rate limits",
        )),
    }

    // Best-effort "newer release available" via the shared planner. Only attempt
    // when GitHub looked reachable; any failure is a WARN, never a FAIL.
    if reachable {
        match update_check(spec, method) {
            Ok(UpdatePlan::UpToDate) => checks.push(Check::pass(
                CAT_NETWORK,
                "latest release",
                format!("up to date (v{})", spec.current_version),
            )),
            Ok(UpdatePlan::UpdateAvailable { latest }) => checks.push(Check::warn(
                CAT_NETWORK,
                "latest release",
                format!("a newer octos is available: v{latest}"),
                method
                    .upgrade_hint(spec)
                    .unwrap_or_else(|| "run `octos update --check`".to_string()),
            )),
            Ok(UpdatePlan::DeferToPackageManager { cmd }) => checks.push(Check::warn(
                CAT_NETWORK,
                "latest release",
                "a newer octos is available",
                cmd,
            )),
            Ok(UpdatePlan::SelfUpdateAllowed) => checks.push(Check::warn(
                CAT_NETWORK,
                "latest release",
                "a newer octos is available (this install can self-update in Stage 3)",
                "run `octos update --check`",
            )),
            Err(err) => checks.push(Check::warn(
                CAT_NETWORK,
                "latest release",
                format!("could not check for a newer release: {err}"),
                "retry when online, or set OCTOS_GITHUB_TOKEN if rate-limited",
            )),
        }
    }

    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd() -> DoctorCommand {
        DoctorCommand {
            json: false,
            verbose: false,
            strict: false,
            data_dir: None,
        }
    }

    #[test]
    fn spec_carries_cli_version_not_diagnostics_crate_version() {
        let spec = octos_server_spec();
        assert_eq!(spec.current_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(spec.binary_name, "octos");
        assert_eq!(spec.github_repo, "octos-org/octos");
        assert_eq!(
            spec.asset_selector.asset_name("aarch64-apple-darwin"),
            "octos-bundle-aarch64-apple-darwin"
        );
    }

    #[test]
    fn report_includes_core_categories_and_protocol_skew_passes() {
        let report = build_report(&cmd(), false).expect("report builds");
        let text = report.render(false, false);
        assert!(text.contains("Binary & version"));
        assert!(text.contains("Terminal environment"));
        assert!(text.contains("Config & data"));
        assert!(text.contains("Backend"));
        // The server's own build advertises every known feature, so the
        // structural skew check must pass.
        let skew = report
            .checks
            .iter()
            .find(|c| c.name == "protocol skew")
            .expect("protocol skew check present");
        assert_eq!(skew.status, octos_diagnostics::CheckStatus::Pass);
        // Glyphs are present in the rendered output.
        assert!(text.contains("[✓]"));
    }
}
