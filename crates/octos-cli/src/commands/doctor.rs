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
//! Stage 1 is **local checks only** — binary/install-method/on-path/shadow,
//! terminal, config+data writability, and a structural protocol-skew check
//! against the server's compiled-in capabilities. There is NO network (GitHub
//! reachability + live WS capability probe are Stage 2) and NO update wiring
//! (Stage 3); see the `// TODO Stage 2` markers below.

use std::path::PathBuf;

use clap::Args;
use eyre::Result;
use octos_core::ui_protocol::{UI_PROTOCOL_KNOWN_FEATURES, UI_PROTOCOL_V1, UiProtocolCapabilities};
use octos_diagnostics::{
    Check, ProductSpec, Report, config_writability_check, data_writability_check, detect, locate,
    on_path_check, protocol_skew_check, shadow_check, terminal_checks,
};

use super::Executable;

const CAT_BINARY: &str = "Binary & version";

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
    .with_brew_formula("octos-org/tap/octos")
    .with_npm_package("@octos-org/octos")
    .with_cargo_install("octos-cli")
    .with_cargo_dist_app("octos")
}

impl Executable for DoctorCommand {
    fn execute(self) -> Result<()> {
        let report = build_report(&self)?;
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

/// Assemble the full local-checks report. Separated from `execute` so it does
/// not call `process::exit` and can be exercised by tests.
fn build_report(cmd: &DoctorCommand) -> Result<Report> {
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
        // network); surface the per-method upgrade command as the value.
        report.push(
            Check::pass(CAT_BINARY, "upgrade path", "package-manager owned").with_value(hint),
        );
    }

    let located = locate(&spec);
    report.push(on_path_check(
        &located,
        current_exe.as_deref(),
        &method,
        &spec,
    ));
    report.push(shadow_check(&located, &method, &spec));
    // TODO Stage 2: GitHub latest-release reachability + "newer release
    // available" check (needs the `github` feature / reqwest).

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
    // TODO Stage 2: replace the compiled-in caps with a LIVE WS
    // `config/capabilities/list` probe against a configured/running server.
    let server_caps = UiProtocolCapabilities::first_server_slice();
    report.push(protocol_skew_check(
        &server_caps,
        UI_PROTOCOL_KNOWN_FEATURES.iter().copied(),
    ));

    Ok(report)
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
        let report = build_report(&cmd()).expect("report builds");
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
